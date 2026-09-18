use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

/// 上部ステータスバー: "stillo | <title> | <url>  XX%"
pub fn render_status_bar(f: &mut Frame, area: Rect, title: &str, url: &str, scroll_pct: usize) {
    let pct_label = format!(" {}% ", scroll_pct);
    let pct_width = pct_label.len() as u16;

    // 固定幅の部品: " stillo " (8) + " │ " (3) + " │ " (3) = 14
    let fixed = 14u16;
    let remaining = area.width.saturating_sub(fixed + pct_width);
    let title_max = (remaining / 2) as usize;
    let url_max = remaining.saturating_sub(remaining / 2) as usize;

    let text = Line::from(vec![
        Span::styled(
            " stillo ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" │ "),
        Span::styled(
            truncate(title, title_max),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" │ "),
        Span::styled(
            truncate(url, url_max),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(pct_label, Style::default().fg(Color::Yellow)),
    ]);
    let bar = Paragraph::new(text).style(Style::default().bg(Color::DarkGray));
    f.render_widget(bar, area);
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 || s.chars().count() <= max {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", truncated)
    }
}
