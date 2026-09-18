use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

/// 下部ヒントバー（通常モード）
pub fn render_hint_bar(
    f: &mut Frame,
    area: Rect,
    link_count: usize,
    selected: Option<usize>,
    selected_link_url: Option<&str>,
    can_go_back: bool,
    can_go_forward: bool,
) {
    // リンク選択中はそのURLをヒントバーに表示する
    if let Some(url) = selected_link_url {
        let max_len = area.width.saturating_sub(4) as usize;
        let truncated = truncate_url(url, max_len);
        let line = Line::from(vec![
            Span::styled(" → ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(truncated, Style::default().fg(Color::Cyan)),
        ]);
        let bar = Paragraph::new(line).style(Style::default().bg(Color::DarkGray));
        f.render_widget(bar, area);
        return;
    }

    let link_info = match (link_count, selected) {
        (0, _) => String::new(),
        (n, Some(i)) => format!(" link {}/{} │", i + 1, n),
        (n, None) => format!(" {} links │", n),
    };

    let back_style = if can_go_back {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let fwd_style = if can_go_forward {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let key = Style::default().fg(Color::Green).add_modifier(Modifier::BOLD);

    let line = Line::from(vec![
        Span::styled(link_info, Style::default().fg(Color::Yellow)),
        Span::styled("[Enter]", key),
        Span::raw("follow "),
        Span::styled("[Tab]", key),
        Span::raw("link "),
        Span::styled("[B]", back_style),
        Span::raw("back "),
        Span::styled("[F]", fwd_style),
        Span::raw("fwd "),
        Span::styled("[U]", key),
        Span::raw("url "),
        Span::styled("[s]", key),
        Span::raw("ddg "),
        Span::styled("[/]", key),
        Span::raw("find "),
        Span::styled("[r]", key),
        Span::raw("reload "),
        Span::styled("[?]", key),
        Span::raw("help "),
        Span::styled("[q]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Span::raw("quit"),
    ]);

    let bar = Paragraph::new(line).style(Style::default().bg(Color::DarkGray));
    f.render_widget(bar, area);
}

/// 下部バー（入力モード）
pub fn render_input_bar(f: &mut Frame, area: Rect, prompt: &str, input: &str) {
    let line = Line::from(vec![
        Span::styled(prompt, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw(input),
        Span::styled("█", Style::default().fg(Color::White)),
    ]);
    let bar = Paragraph::new(line).style(Style::default().bg(Color::DarkGray));
    f.render_widget(bar, area);
}

fn truncate_url(s: &str, max: usize) -> String {
    if max == 0 || s.chars().count() <= max {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", truncated)
    }
}
