use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, HighlightSpacing, Paragraph, Row, Table};

use crate::app::{App, Tab};

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
        "space pause · n next · b prev · ←/→ ±5s · +/- volume · l like · s shuffle · Tab switch · r refresh · q quit",
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
