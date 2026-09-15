use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, Table, Wrap,
};

use crate::app::{App, Tab};
use crate::settings::Settings;

const ACCENT: Color = Color::Yellow;

pub fn render(frame: &mut Frame, app: &mut App) {
    let [header, list, player, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(5),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_header(frame, header, app);
    render_list(frame, list, app);
    render_player(frame, player, app);
    render_status(frame, status, app);
    if let Some(settings) = &mut app.settings {
        render_settings(frame, settings);
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(" tuiya ", Style::new().fg(ACCENT).bold())];
    for tab in [Tab::Wave, Tab::Likes] {
        let style = if app.tab == tab {
            Style::new().fg(Color::Black).bg(ACCENT).bold()
        } else {
            Style::new().fg(Color::DarkGray)
        };
        spans.push(Span::styled(format!(" {} ", tab.title()), style));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        " o settings ",
        Style::new().fg(Color::DarkGray),
    ));
    frame.render_widget(Line::from(spans), area);
}

fn render_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let tab = app.tab;
    let playing = app.queue(tab).playing;

    let rows: Vec<Row> = {
        let tracks = &app.queue(tab).tracks;
        tracks
            .iter()
            .enumerate()
            .map(|(index, track)| {
                let marker = if Some(index) == playing { "▶" } else { "" };
                let heart = if app.is_liked(&track.id) { "♥" } else { "" };
                let style = if Some(index) == playing {
                    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else if !track.available {
                    Style::new().fg(Color::DarkGray)
                } else {
                    Style::new()
                };
                Row::new(vec![
                    Cell::from(marker),
                    Cell::from(format!("{}", index + 1)),
                    Cell::from(track.artists.clone()),
                    Cell::from(track.title.clone()),
                    Cell::from(heart).style(Style::new().fg(Color::Red)),
                    Cell::from(format_duration(track.duration)),
                ])
                .style(style)
            })
            .collect()
    };

    let title = match tab {
        Tab::Wave => " My Wave ".to_string(),
        Tab::Likes => format!(" Liked ({}) ", app.likes.tracks.len()),
    };
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(Color::DarkGray));

    if rows.is_empty() {
        let hint = Paragraph::new(app.queue(tab).placeholder.clone())
            .alignment(Alignment::Center)
            .block(block);
        frame.render_widget(hint, area);
        return;
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Percentage(35),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(5),
        ],
    )
    .block(block)
    .column_spacing(1)
    .highlight_spacing(HighlightSpacing::Always)
    .row_highlight_style(
        Style::new()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let queue = app.queue_mut(tab);
    queue.state.select(Some(queue.cursor));
    frame.render_stateful_widget(table, area, &mut queue.state);
}

fn render_player(frame: &mut Frame, area: Rect, app: &App) {
    let audio = app.audio_state();
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(playing) = app.playing.as_ref() else {
        frame.render_widget(
            Paragraph::new("Nothing playing — press Enter to start the highlighted track")
                .style(Style::new().fg(Color::DarkGray)),
            inner,
        );
        return;
    };

    let icon = if app.loading_track {
        "⏳"
    } else if audio.paused {
        "⏸"
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
    let width = inner.width.saturating_sub(30).max(10) as usize;
    let ratio = if total.is_zero() {
        0.0
    } else {
        position.as_secs_f64() / total.as_secs_f64()
    };
    let filled = ((ratio * width as f64).round() as usize).min(width);

    let progress = Line::from(vec![
        Span::styled("█".repeat(filled), Style::new().fg(ACCENT)),
        Span::styled("░".repeat(width - filled), Style::new().fg(Color::DarkGray)),
        Span::raw(format!(
            "  {} / {}",
            format_duration(position),
            format_duration(total)
        )),
        Span::styled(
            format!("   🔊 {:>3}%", (app.volume * 100.0).round() as i32),
            Style::new().fg(Color::DarkGray),
        ),
        Span::styled(
            if app.shuffle { "  🔀" } else { "" },
            Style::new().fg(Color::DarkGray),
        ),
    ]);

    let keys = Line::from(Span::styled(
        "space pause · n next · b prev · ←/→ ±5s · +/- volume · l like · s shuffle · Tab switch · r refresh · o settings · q quit",
        Style::new().fg(Color::DarkGray),
    ));

    let [line_now, line_progress, line_keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(now, line_now);
    frame.render_widget(progress, line_progress);
    frame.render_widget(keys, line_keys);
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

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    frame.render_widget(
        Line::from(Span::styled(
            app.status.clone(),
            Style::new().fg(Color::DarkGray),
        )),
        area,
    );
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    format!("{:02}:{:02}", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Preferences;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
}
