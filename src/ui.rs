use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, Table, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Focus, LayoutMode, NoticeKind, Tab};
use crate::settings::Settings;
use crate::wave_settings::WaveSettingsDialog;

const ACCENT: Color = Color::Yellow;
const MUTED: Color = Color::DarkGray;
const SUCCESS: Color = Color::Green;
const ERROR: Color = Color::Red;
const PLAYING: Color = Color::LightYellow;

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let mode = layout_mode(area);
    app.layout_mode = mode;

    if mode == LayoutMode::Wide && app.sidebar.wide_visible {
        let [sidebar, main] =
            Layout::horizontal([Constraint::Length(26), Constraint::Fill(1)]).areas(area);
        render_sidebar(frame, sidebar, app);
        render_main(frame, main, app, mode);
    } else {
        render_main(frame, area, app, mode);
    }

    if mode != LayoutMode::Wide && app.sidebar.overlay_open {
        let player_height = player_height(mode);
        let popup = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width.min(30),
            area.height.saturating_sub(2 + player_height),
        );
        frame.render_widget(Clear, popup);
        render_sidebar(frame, popup, app);
    }

    if let Some(settings) = &mut app.settings {
        render_settings(frame, settings);
    }
    if let Some(settings) = &mut app.wave_settings_dialog {
        render_wave_settings(frame, settings);
    }
}

fn layout_mode(area: Rect) -> LayoutMode {
    if area.width >= 100 && area.height >= 16 {
        LayoutMode::Wide
    } else if area.width >= 70 && area.height >= 12 {
        LayoutMode::Compact
    } else {
        LayoutMode::Minimal
    }
}

fn render_main(frame: &mut Frame, area: Rect, app: &mut App, mode: LayoutMode) {
    let player_height = player_height(mode);
    let [header, list, player, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(player_height),
        Constraint::Length(1),
    ])
    .areas(area);

    render_header(frame, header, app, mode);
    render_list(frame, list, app, mode);
    let player = if mode == LayoutMode::Wide && app.sidebar.wide_visible {
        inset_left(player, 1)
    } else {
        player
    };
    render_player(frame, player, app, mode);
    let footer = if mode == LayoutMode::Wide && app.sidebar.wide_visible {
        inset_left(footer, 1)
    } else {
        footer
    };
    render_footer(frame, footer, app, mode);
}

fn inset_left(area: Rect, amount: u16) -> Rect {
    let amount = amount.min(area.width);
    Rect::new(
        area.x.saturating_add(amount),
        area.y,
        area.width.saturating_sub(amount),
        area.height,
    )
}

fn player_height(mode: LayoutMode) -> u16 {
    match mode {
        LayoutMode::Minimal => 3,
        LayoutMode::Wide | LayoutMode::Compact => 4,
    }
}

fn render_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    let border = if app.focus == Focus::Sidebar {
        ACCENT
    } else {
        MUTED
    };
    let block = Block::bordered()
        .title(" tuiya ")
        .border_style(Style::new().fg(border))
        .style(Style::new().bg(Color::Black));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let entries = [
        (Tab::Wave, "My Wave", None),
        (Tab::Likes, "Liked tracks", Some(app.likes.tracks.len())),
    ];
    let mut lines = vec![
        Line::from(Span::styled(" LIBRARY", Style::new().fg(MUTED).bold())),
        Line::raw(""),
    ];
    for (index, (tab, label, count)) in entries.into_iter().enumerate() {
        let active = app.view == tab;
        let selected = app.sidebar.selected == index && app.focus == Focus::Sidebar;
        let marker = if active { "●" } else { " " };
        let text = count.map_or_else(
            || format!(" {marker} {label}"),
            |count| format!(" {marker} {label}  {count}"),
        );
        let style = if selected {
            Style::new().fg(Color::Black).bg(ACCENT).bold()
        } else if active {
            Style::new().fg(ACCENT).bold()
        } else {
            Style::new()
        };
        lines.push(Line::from(Span::styled(text, style)));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_header(frame: &mut Frame, area: Rect, app: &App, mode: LayoutMode) {
    let context = match app.view {
        Tab::Wave => app.wave_settings.as_ref().map_or_else(
            || "My Wave".to_string(),
            |settings| {
                if settings.is_default() {
                    "My Wave".to_string()
                } else {
                    format!("My Wave · {}", settings.name)
                }
            },
        ),
        Tab::Likes => format!("Liked tracks · {}", app.likes.tracks.len()),
    };
    let prefix = if mode == LayoutMode::Wide && app.sidebar.wide_visible {
        ""
    } else {
        "tuiya · "
    };
    if area.width >= 32 {
        let [title, settings] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(12)]).areas(area);
        frame.render_widget(
            Line::from(vec![
                Span::styled(prefix, Style::new().fg(ACCENT).bold()),
                Span::styled(context, Style::new().bold()),
            ]),
            title,
        );
        frame.render_widget(
            Line::from(Span::styled("o Settings", Style::new().fg(MUTED)))
                .alignment(Alignment::Right),
            settings,
        );
    } else {
        frame.render_widget(
            Line::from(Span::styled(
                truncate_to_width(&format!("{prefix}{context}"), area.width as usize),
                Style::new().bold(),
            )),
            area,
        );
    }
}

fn render_list(frame: &mut Frame, area: Rect, app: &mut App, mode: LayoutMode) {
    let tab = app.view;
    let playing = app.queue(tab).playing;
    let digits = app.queue(tab).tracks.len().max(1).to_string().len() as u16;
    let fixed_width = 2u16 // borders
        .saturating_add(2) // highlight symbol
        .saturating_add(if mode == LayoutMode::Minimal { 4 } else { 5 }) // gaps
        .saturating_add(1) // playing marker
        .saturating_add(digits)
        .saturating_add(1) // heart
        .saturating_add(5); // duration
    let flexible_width = area.width.saturating_sub(fixed_width) as usize;
    let artist_width = flexible_width.saturating_mul(35) / 100;
    let title_width = flexible_width.saturating_sub(artist_width);

    let rows: Vec<Row> = {
        let tracks = &app.queue(tab).tracks;
        tracks
            .iter()
            .enumerate()
            .map(|(index, track)| {
                let marker = if Some(index) == playing { "▶" } else { " " };
                let heart = if app.is_liked(&track.id) { "♥" } else { "" };
                let style = if Some(index) == playing {
                    Style::new().fg(PLAYING).add_modifier(Modifier::BOLD)
                } else if !track.available {
                    Style::new().fg(MUTED)
                } else {
                    Style::new()
                };
                let cells = if mode == LayoutMode::Minimal {
                    vec![
                        Cell::from(marker),
                        Cell::from(format!("{}", index + 1)),
                        Cell::from(truncate_to_width(&track.label(), flexible_width)),
                        Cell::from(heart).style(Style::new().fg(Color::Red)),
                        Cell::from(format_duration(track.duration)),
                    ]
                } else {
                    vec![
                        Cell::from(marker),
                        Cell::from(format!("{}", index + 1)),
                        Cell::from(truncate_to_width(&track.artists, artist_width)),
                        Cell::from(truncate_to_width(&track.title, title_width)),
                        Cell::from(heart).style(Style::new().fg(Color::Red)),
                        Cell::from(format_duration(track.duration)),
                    ]
                };
                Row::new(cells).style(style)
            })
            .collect()
    };

    let block = Block::bordered().border_style(Style::new().fg(if app.focus == Focus::Content {
        ACCENT
    } else {
        MUTED
    }));

    if rows.is_empty() {
        let hint = Paragraph::new(app.queue(tab).placeholder.clone())
            .alignment(Alignment::Center)
            .block(block);
        frame.render_widget(hint, area);
        return;
    }

    let constraints = if mode == LayoutMode::Minimal {
        vec![
            Constraint::Length(1),
            Constraint::Length(digits),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(5),
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Length(digits),
            Constraint::Percentage(35),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(5),
        ]
    };
    let table = Table::new(rows, constraints)
        .block(block)
        .column_spacing(1)
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_symbol("› ")
        .row_highlight_style(Style::new().bg(MUTED).add_modifier(Modifier::BOLD));

    let queue = app.queue_mut(tab);
    queue.state.select(Some(queue.cursor));
    frame.render_stateful_widget(table, area, &mut queue.state);
}

fn render_player(frame: &mut Frame, area: Rect, app: &App, mode: LayoutMode) {
    let audio = app.audio_state();
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(MUTED));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(playing) = app.playing.as_ref() else {
        frame.render_widget(
            Paragraph::new("Nothing playing — press Enter to start the highlighted track")
                .style(Style::new().fg(MUTED)),
            inner,
        );
        return;
    };

    let icon = if app.loading_track {
        "…"
    } else if audio.paused {
        "Ⅱ"
    } else {
        "▶"
    };

    let heart = if app.is_liked(&playing.track.id) {
        Span::styled("  ♥", Style::new().fg(Color::Red))
    } else {
        Span::raw("")
    };

    let now = Line::from(vec![
        Span::styled(format!("{icon} "), Style::new().fg(ACCENT)),
        Span::styled(
            playing.track.artists.clone(),
            Style::new().fg(ACCENT).bold(),
        ),
        Span::raw(" — "),
        Span::styled(playing.track.title.clone(), Style::new().bold()),
        heart,
    ]);

    let total = playing.track.duration;
    let position = audio.position.min(total);
    let suffix = format!(
        "  {} / {}  Vol {}%{}",
        format_duration(position),
        format_duration(total),
        (app.volume * 100.0).round() as i32,
        if app.shuffle { "  shuffle" } else { "" },
    );
    let width = (inner.width as usize)
        .saturating_sub(UnicodeWidthStr::width(suffix.as_str()))
        .max(1);
    let ratio = if total.is_zero() {
        0.0
    } else {
        position.as_secs_f64() / total.as_secs_f64()
    };
    let filled = ((ratio * width as f64).round() as usize).min(width);

    let progress = Line::from(vec![
        Span::styled("█".repeat(filled), Style::new().fg(ACCENT)),
        Span::styled("░".repeat(width - filled), Style::new().fg(Color::DarkGray)),
        Span::styled(suffix, Style::new().fg(MUTED)),
    ]);

    let [line_now, line_progress] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);

    frame.render_widget(now, line_now);
    frame.render_widget(progress, line_progress);
    let _ = mode;
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App, mode: LayoutMode) {
    let (text, color) = if let Some(notice) = &app.notice {
        let color = match notice.kind {
            NoticeKind::Info => MUTED,
            NoticeKind::Success => SUCCESS,
            NoticeKind::Error => ERROR,
        };
        (notice.text.clone(), color)
    } else if app.focus == Focus::Sidebar {
        let sidebar_help = if mode == LayoutMode::Wide {
            "↑/↓ navigate · Enter open · Tab content · q quit"
        } else {
            "↑/↓ navigate · Enter open · Tab content · Esc close"
        };
        (sidebar_help.to_string(), MUTED)
    } else {
        let base = match mode {
            LayoutMode::Wide if app.sidebar.wide_visible => {
                "↑/↓ navigate · Enter play · Tab navigation · Space pause · n/b track · q quit"
            }
            LayoutMode::Wide => {
                "↑/↓ navigate · Enter play · Ctrl-B menu · Space pause · n/b track · q quit"
            }
            LayoutMode::Compact => "↑/↓ navigate · Enter play · Tab menu · Space pause · q quit",
            LayoutMode::Minimal => "↑/↓ · Enter · Tab · Space · q",
        };
        let wave = if app.view == Tab::Wave && app.wave_choices.is_some() {
            if app.wave_is_custom() {
                " · w tune · R reset"
            } else {
                " · w tune"
            }
        } else {
            ""
        };
        (format!("{base}{wave}"), MUTED)
    };
    frame.render_widget(
        Line::from(Span::styled(
            truncate_to_width(&text, area.width as usize),
            Style::new().fg(color),
        )),
        area,
    );
}

fn truncate_to_width(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let target = max_width.saturating_sub(1);
    let mut result = String::new();
    let mut width = 0;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > target {
            break;
        }
        result.push(character);
        width += character_width;
    }
    result.push('…');
    result
}

fn render_settings(frame: &mut Frame, settings: &mut Settings) {
    let area = frame.area();
    let width = area.width.min(70);
    let height = area.height.min(17);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Settings ")
        .border_style(Style::new().fg(ACCENT))
        .style(Style::new().bg(Color::Black));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [intro, table, help, error, footer] = Layout::vertical([
        Constraint::Length(if inner.height >= 10 { 2 } else { 0 }),
        Constraint::Min(0),
        Constraint::Length(if inner.height >= 10 { 3 } else { 0 }),
        Constraint::Length(if settings.error.is_some() { 2 } else { 0 }),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new("  Choose a setting, then change its value.")
            .style(Style::new().fg(Color::DarkGray)),
        intro,
    );

    let rows = [
        (
            "Audio quality",
            if settings.draft.quality == "high" {
                "◀ MP3 320 ▶".into()
            } else {
                "◀ Lossless ▶".into()
            },
        ),
        (
            "Streaming",
            if settings.draft.streaming {
                "◀ On ▶".into()
            } else {
                "◀ Off ▶".into()
            },
        ),
        ("Cache size", format!("{} MB", settings.cache_input)),
        (
            "Volume",
            format!("◀ {}% ▶", (settings.draft.volume * 100.0).round() as u16),
        ),
        ("Save changes", "Enter".into()),
    ]
    .into_iter()
    .map(|(label, value)| Row::new([Cell::from(format!(" {label}")), Cell::from(value)]));
    settings.table.select(Some(settings.selected));
    frame.render_stateful_widget(
        Table::new(rows, [Constraint::Percentage(40), Constraint::Fill(1)])
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_symbol("› ")
            .row_highlight_style(Style::new().fg(Color::Black).bg(ACCENT).bold()),
        table,
        &mut settings.table,
    );
    frame.render_widget(
        Paragraph::new(settings.help())
            .wrap(Wrap { trim: true })
            .style(Style::new().fg(Color::DarkGray)),
        help,
    );
    if let Some(message) = &settings.error {
        frame.render_widget(
            Paragraph::new(message.as_str())
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(Color::Red)),
            error,
        );
    }
    frame.render_widget(
        Paragraph::new(if inner.width >= 55 {
            "↑/↓ Tab select · ←/→ change · Ctrl-S save · Esc cancel"
        } else {
            "↑/↓ select · Ctrl-S save · Esc cancel"
        })
        .style(Style::new().fg(ACCENT)),
        footer,
    );
}

fn render_wave_settings(frame: &mut Frame, settings: &mut WaveSettingsDialog) {
    let area = frame.area();
    let width = area.width.min(76);
    let height = area.height.min(20);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Tune this Wave ")
        .border_style(Style::new().fg(ACCENT))
        .style(Style::new().bg(Color::Black));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [intro, table, help, footer] = Layout::vertical([
        Constraint::Length(if inner.height >= 10 { 2 } else { 0 }),
        Constraint::Min(0),
        Constraint::Length(if inner.height >= 10 { 3 } else { 0 }),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new("  Presets are provided dynamically by the Yandex Music wheel.")
            .style(Style::new().fg(Color::DarkGray)),
        intro,
    );

    let rows = settings
        .options()
        .iter()
        .map(|option| {
            Row::new([
                Cell::from(format!(" {}", option.name)),
                Cell::from(option.description.clone()),
            ])
        })
        .collect::<Vec<_>>();
    settings.table.select(Some(settings.selected));
    frame.render_stateful_widget(
        Table::new(rows, [Constraint::Percentage(55), Constraint::Fill(1)])
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_symbol("› ")
            .row_highlight_style(Style::new().fg(Color::Black).bg(ACCENT).bold()),
        table,
        &mut settings.table,
    );
    frame.render_widget(
        Paragraph::new(settings.help())
            .wrap(Wrap { trim: true })
            .style(Style::new().fg(Color::DarkGray)),
        help,
    );
    frame.render_widget(
        Paragraph::new("↑/↓ select · Enter apply · Esc cancel").style(Style::new().fg(ACCENT)),
        footer,
    );
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    format!("{:02}:{:02}", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;
    use crate::api::models::Track;
    use crate::audio::Audio;
    use crate::config::Preferences;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn test_app() -> App {
        let preferences = Preferences {
            quality: "high".into(),
            cache_limit_mb: 128,
            streaming: true,
            volume: 0.8,
        };
        let mut app = App::new(
            Arc::new(api::Client::for_test()),
            Audio::for_test(0.8),
            PathBuf::new(),
            preferences,
        );
        app.notice = None;
        app.wave.tracks = vec![Track {
            id: "1".into(),
            album_id: None,
            title: "Очень длинное название трека для проверки интерфейса".into(),
            artists: "Исполнитель".into(),
            duration: Duration::from_secs(252),
            available: true,
        }];
        app
    }

    fn rendered(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn rendered_row(terminal: &Terminal<TestBackend>, row: u16) -> String {
        let buffer = terminal.backend().buffer();
        let width = buffer.area().width as usize;
        let start = row as usize * width;
        buffer.content()[start..start + width]
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn responsive_modes_cover_common_terminal_sizes() {
        assert_eq!(layout_mode(Rect::new(0, 0, 120, 30)), LayoutMode::Wide);
        assert_eq!(layout_mode(Rect::new(0, 0, 90, 24)), LayoutMode::Compact);
        assert_eq!(layout_mode(Rect::new(0, 0, 60, 18)), LayoutMode::Minimal);
        assert_eq!(layout_mode(Rect::new(0, 0, 40, 10)), LayoutMode::Minimal);
        assert_eq!(layout_mode(Rect::new(0, 0, 120, 10)), LayoutMode::Minimal);
    }

    #[test]
    fn player_inset_is_safe_for_narrow_areas() {
        assert_eq!(
            inset_left(Rect::new(10, 5, 20, 4), 1),
            Rect::new(11, 5, 19, 4)
        );
        assert_eq!(
            inset_left(Rect::new(10, 5, 0, 4), 1),
            Rect::new(10, 5, 0, 4)
        );
    }

    #[test]
    fn truncation_is_unicode_safe_and_respects_terminal_width() {
        assert_eq!(truncate_to_width("Исполнитель", 7), "Исполн…");
        assert_eq!(truncate_to_width("трек", 4), "трек");
        assert_eq!(truncate_to_width("anything", 1), "…");
        assert_eq!(truncate_to_width("anything", 0), "");
        assert!(UnicodeWidthStr::width(truncate_to_width("Музыка 🎵", 6).as_str()) <= 6);
    }

    #[test]
    fn main_screen_renders_in_every_layout_mode() {
        for (width, height, sidebar_expected) in [
            (120, 30, true),
            (90, 24, false),
            (60, 18, false),
            (40, 10, false),
        ] {
            let mut app = test_app();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let output = rendered(&terminal);
            assert!(output.contains("My Wave"));
            assert_eq!(output.contains("LIBRARY"), sidebar_expected);
        }
    }

    #[test]
    fn compact_sidebar_renders_as_an_overlay() {
        let mut app = test_app();
        app.layout_mode = LayoutMode::Compact;
        app.sidebar.overlay_open = true;
        app.focus = Focus::Sidebar;
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let output = rendered(&terminal);
        assert!(output.contains("LIBRARY"));
        assert!(output.contains("Liked tracks"));
        assert!(rendered_row(&terminal, 23).contains("Esc close"));
    }

    #[test]
    fn settings_remain_usable_in_small_terminals_and_show_save_errors() {
        for (width, height) in [(80, 24), (40, 10), (20, 6), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut settings = Settings::new(Preferences {
                quality: "lossless".into(),
                cache_limit_mb: 4096,
                streaming: true,
                volume: 1.0,
            });
            settings.selected = 4;
            terminal
                .draw(|frame| render_settings(frame, &mut settings))
                .unwrap();
            if width >= 40 {
                let rendered = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(rendered.contains("Settings"));
                assert!(rendered.contains("Save changes"));
                assert!(rendered.contains("Esc cancel"));
            }
            settings.error = Some("Cannot write config".into());
            terminal
                .draw(|frame| render_settings(frame, &mut settings))
                .unwrap();
            if width == 80 {
                let rendered = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(rendered.contains("Cannot write config"));
            }
        }
    }

    #[test]
    fn wave_settings_render_in_small_terminals() {
        let values = crate::api::models::WaveSettings::default();
        let choices = crate::api::models::WaveChoices {
            options: vec![values.clone()],
        };
        for (width, height) in [(80, 24), (40, 10), (20, 6), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut settings = WaveSettingsDialog::new(values.clone(), choices.clone());
            terminal
                .draw(|frame| render_wave_settings(frame, &mut settings))
                .unwrap();
        }
    }
}
