use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::mem::take;
use stillo_core::{
    document::{ExtractedContent, ExtractedLink},
    Block, Document, Inline,
}; // ExtractedContent は from_content ラッパーで使用
use url::Url;

const WRAP_WIDTH: usize = 80;
const CODE_WIDTH: usize = 76;

/// HTML コンテンツを意味的 AST 経由で ratatui Lines に変換し、
/// スクロールとリンク選択を管理する。
pub struct ContentView {
    pub lines: Vec<Line<'static>>,
    pub scroll_offset: usize,
    pub selected_link: Option<usize>,
    /// (line_idx, link_idx_in_content_links) — navigate 用
    pub link_positions: Vec<(usize, usize)>,
    /// (line_idx, span_start, span_end) — ハイライト再適用用
    link_span_ranges: Vec<(usize, usize, usize)>,
}

impl ContentView {
    /// Document と links から直接 ContentView を構築する（フォーマット非依存エントリポイント）
    pub fn from_document(doc: &Document, links: &[ExtractedLink]) -> Self {
        let mut renderer = DocRenderer::new(links);
        renderer.render(doc);
        Self {
            lines: renderer.lines,
            link_positions: renderer.link_positions,
            link_span_ranges: renderer.link_span_ranges,
            scroll_offset: 0,
            selected_link: None,
        }
    }

    /// HTML コンテンツから ContentView を構築する（後方互換ラッパー）
    pub fn from_content(content: &ExtractedContent) -> Self {
        let doc = stillo_core::parse_html_to_ast(&content.body_html, &content.url);
        Self::from_document(&doc, &content.links)
    }

    pub fn total_lines(&self) -> usize {
        self.lines.len()
    }

    pub fn scroll_down(&mut self, n: usize, viewport_height: usize) {
        let max = self.lines.len().saturating_sub(viewport_height);
        self.scroll_offset = (self.scroll_offset + n).min(max);
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn scroll_to_bottom(&mut self, viewport_height: usize) {
        self.scroll_offset = self.lines.len().saturating_sub(viewport_height);
    }

    pub fn next_link(&mut self) {
        if self.link_positions.is_empty() {
            return;
        }
        self.selected_link = Some(match self.selected_link {
            None => 0,
            Some(i) => (i + 1).min(self.link_positions.len() - 1),
        });
        self.scroll_to_selected_link();
        self.rebuild_link_highlights();
    }

    pub fn prev_link(&mut self) {
        if self.link_positions.is_empty() {
            return;
        }
        self.selected_link = Some(match self.selected_link {
            None => 0,
            Some(i) => i.saturating_sub(1),
        });
        self.scroll_to_selected_link();
        self.rebuild_link_highlights();
    }

    pub fn selected_link_url<'a>(&self, links: &'a [ExtractedLink]) -> Option<&'a Url> {
        let sel = self.selected_link?;
        let (_, link_idx) = self.link_positions.get(sel)?;
        links.get(*link_idx).map(|l| &l.href)
    }

    /// 選択中のリンクが表示領域内に入るようスクロールする
    fn scroll_to_selected_link(&mut self) {
        if let Some(sel) = self.selected_link {
            if let Some(&(line_idx, _)) = self.link_positions.get(sel) {
                if line_idx < self.scroll_offset {
                    self.scroll_offset = line_idx;
                }
            }
        }
    }

    /// 選択状態変化後に span 単位でハイライトを更新する。
    /// 行全体を上書きするのではなく span 範囲だけを変えることで、
    /// 通常テキストのスタイルを保持できる。
    fn rebuild_link_highlights(&mut self) {
        for (pos_idx, (&(line_idx, _), &(_, span_start, span_end))) in self
            .link_positions
            .iter()
            .zip(self.link_span_ranges.iter())
            .enumerate()
        {
            let is_selected = self.selected_link == Some(pos_idx);
            let style = if is_selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default().fg(Color::Cyan)
            };
            if let Some(line) = self.lines.get_mut(line_idx) {
                for span in line.spans[span_start..span_end].iter_mut() {
                    span.style = style;
                }
            }
        }
    }

    /// 検索クエリにマッチする行インデックスを返す
    pub fn search(&self, query: &str) -> Vec<usize> {
        if query.is_empty() {
            return vec![];
        }
        let q = query.to_lowercase();
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                text.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// DocRenderer
// ---------------------------------------------------------------------------

/// Document → ratatui Lines の変換器
struct DocRenderer<'a> {
    links: &'a [ExtractedLink],
    pub lines: Vec<Line<'static>>,
    pub link_positions: Vec<(usize, usize)>,
    pub link_span_ranges: Vec<(usize, usize, usize)>,
    link_counter: usize,
    current_spans: Vec<Span<'static>>,
    current_len: usize,
    /// フラッシュ待ちリンク: (link_idx_in_content, span_start, span_end)
    pending_links: Vec<(usize, usize, usize)>,
}

impl<'a> DocRenderer<'a> {
    fn new(links: &'a [ExtractedLink]) -> Self {
        Self {
            links,
            lines: Vec::new(),
            link_positions: Vec::new(),
            link_span_ranges: Vec::new(),
            link_counter: 0,
            current_spans: Vec::new(),
            current_len: 0,
            pending_links: Vec::new(),
        }
    }

    fn render(&mut self, doc: &Document) {
        for block in &doc.blocks {
            match block {
                Block::Heading { level, inlines } => self.render_heading(*level, inlines),
                Block::Paragraph(inlines) => self.render_paragraph(inlines),
                Block::ListItem {
                    depth,
                    ordered,
                    number,
                    inlines,
                } => {
                    self.render_list_item(*depth, *ordered, *number, inlines);
                }
                Block::CodeBlock { lang, content } => {
                    self.render_code_block(lang.as_deref(), content)
                }
                Block::Blockquote(inlines) => self.render_blockquote(inlines),
                Block::Rule => self.render_rule(),
            }
        }
        // 末尾に未フラッシュのスパンがあれば出力する
        self.flush_line();
    }

    fn render_heading(&mut self, level: u8, inlines: &[Inline]) {
        let text = inlines_to_text(inlines);
        self.push_empty_line();
        match level {
            1 => {
                // タイトル行
                let title_line = Line::from(Span::styled(
                    text.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
                self.lines.push(title_line);
                // アンダーライン
                let underline = "═".repeat(text.chars().count());
                self.lines.push(Line::from(Span::styled(
                    underline,
                    Style::default().fg(Color::Yellow),
                )));
            }
            2 => {
                // "── テキスト ─────..." (合計60文字)
                let prefix = "── ";
                let suffix = " ";
                let inner = format!("{}{}{}", prefix, text, suffix);
                let pad_count = 60usize.saturating_sub(inner.chars().count());
                let pad = "─".repeat(pad_count);
                let full = format!("{}{}", inner, pad);
                self.lines.push(Line::from(Span::styled(
                    full,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            3 => {
                let full = format!("▸ {}", text);
                self.lines.push(Line::from(Span::styled(
                    full,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            _ => {
                let full = format!("  § {}", text);
                self.lines.push(Line::from(Span::styled(
                    full,
                    Style::default().add_modifier(Modifier::BOLD),
                )));
            }
        }
        self.push_empty_line();
    }

    fn render_paragraph(&mut self, inlines: &[Inline]) {
        self.push_empty_line();
        self.render_inlines(inlines, Style::default(), WRAP_WIDTH);
        self.flush_line();
    }

    fn render_list_item(&mut self, depth: usize, ordered: bool, number: usize, inlines: &[Inline]) {
        self.flush_line();
        let prefix = if ordered {
            format!("{}{number}. ", "  ".repeat(depth))
        } else {
            format!("{}• ", "  ".repeat(depth.saturating_sub(1)))
        };
        let prefix_len = prefix.chars().count();
        self.current_spans
            .push(Span::styled(prefix, Style::default().fg(Color::DarkGray)));
        self.current_len += prefix_len;
        let remaining = WRAP_WIDTH.saturating_sub(prefix_len);
        self.render_inlines(inlines, Style::default(), remaining);
        self.flush_line();
    }

    fn render_code_block(&mut self, lang: Option<&str>, content: &str) {
        self.flush_line();
        self.push_empty_line();

        // 上境界線: ╭─ {lang} ─────╮
        let lang_label = lang
            .map(|l| format!(" {} ", l))
            .unwrap_or_else(|| " ".to_owned());
        let border_inner_len = CODE_WIDTH + 2; // "─ " + content + " ─"
        let lang_pad = border_inner_len.saturating_sub(lang_label.chars().count() + 1);
        let top = format!("╭─{}{}╮", lang_label, "─".repeat(lang_pad));
        self.lines.push(Line::from(Span::styled(
            top,
            Style::default().fg(Color::DarkGray),
        )));

        // コンテンツ行
        for line in content.lines() {
            let line_len = line.chars().count();
            let pad = CODE_WIDTH.saturating_sub(line_len);
            let padded = format!("{}{}", line, " ".repeat(pad));
            self.lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(padded, Style::default().fg(Color::Yellow)),
                Span::styled(" │", Style::default().fg(Color::DarkGray)),
            ]));
        }

        // 下境界線: ╰──────────╯
        let bottom = format!("╰{}╯", "─".repeat(CODE_WIDTH + 2));
        self.lines.push(Line::from(Span::styled(
            bottom,
            Style::default().fg(Color::DarkGray),
        )));

        self.push_empty_line();
    }

    fn render_blockquote(&mut self, inlines: &[Inline]) {
        self.push_empty_line();
        // 先頭に引用プレフィックスを追加してからインライン展開する
        self.current_spans
            .push(Span::styled("▎ ", Style::default().fg(Color::Cyan)));
        self.current_len += 2;
        let italic_style = Style::default().add_modifier(Modifier::ITALIC);
        self.render_inlines(inlines, italic_style, WRAP_WIDTH.saturating_sub(2));
        self.flush_line();
        self.push_empty_line();
    }

    fn render_rule(&mut self) {
        self.flush_line();
        self.lines.push(Line::from(Span::styled(
            "─".repeat(60),
            Style::default().fg(Color::DarkGray),
        )));
        self.push_empty_line();
    }

    /// インライン要素列をワードラップしながら current_spans に追加する
    fn render_inlines(&mut self, inlines: &[Inline], base_style: Style, wrap_width: usize) {
        for inline in inlines {
            match inline {
                Inline::Text(t) => self.push_words(t, base_style, wrap_width),
                Inline::Bold(t) => {
                    self.push_words(t, base_style.add_modifier(Modifier::BOLD), wrap_width);
                }
                Inline::Italic(t) => {
                    self.push_words(t, base_style.add_modifier(Modifier::ITALIC), wrap_width);
                }
                Inline::BoldItalic(t) => {
                    self.push_words(
                        t,
                        base_style
                            .add_modifier(Modifier::BOLD)
                            .add_modifier(Modifier::ITALIC),
                        wrap_width,
                    );
                }
                Inline::Code(t) => {
                    let display = format!("`{}`", t);
                    let display_len = display.chars().count();
                    let need_space = self.current_len > 0;
                    let total = display_len + if need_space { 1 } else { 0 };
                    if self.current_len > 0 && self.current_len + total > wrap_width {
                        self.flush_line();
                    } else if need_space {
                        self.current_spans.push(Span::raw(" "));
                        self.current_len += 1;
                    }
                    self.current_spans
                        .push(Span::styled(display, Style::default().fg(Color::Yellow)));
                    self.current_len += display_len;
                }
                Inline::Link { text, href } => self.push_link(text, href, wrap_width),
                Inline::SoftBreak => self.flush_line(),
            }
        }
    }

    /// テキストをワード単位でラップしながら追加する
    fn push_words(&mut self, text: &str, style: Style, wrap_width: usize) {
        for word in text.split_whitespace() {
            let need_space = self.current_len > 0;
            let word_len = word.chars().count();
            let total = word_len + if need_space { 1 } else { 0 };
            if self.current_len > 0 && self.current_len + total > wrap_width {
                self.flush_line();
                self.current_spans
                    .push(Span::styled(word.to_owned(), style));
                self.current_len = word_len;
            } else {
                if need_space {
                    self.current_spans.push(Span::raw(" "));
                    self.current_len += 1;
                }
                self.current_spans
                    .push(Span::styled(word.to_owned(), style));
                self.current_len += word_len;
            }
        }
    }

    /// リンクを出力する。content.links に一致するものがあれば link_positions に記録する。
    fn push_link(&mut self, text: &str, href: &str, wrap_width: usize) {
        // content.links との照合（末尾スラッシュを無視して比較）
        let link_idx = self.links.iter().position(|l| {
            l.href.as_str() == href
                || l.href.as_str().trim_end_matches('/') == href.trim_end_matches('/')
        });

        let display_num = self.link_counter + 1;
        self.link_counter += 1;

        let label = format!("[{}]", display_num);
        let trimmed_text = text.trim();
        let display_text = if trimmed_text.is_empty() {
            label.clone()
        } else {
            format!("{} {}", label, trimmed_text)
        };
        let display_len = display_text.chars().count();
        let need_space = self.current_len > 0;
        let total = display_len + if need_space { 1 } else { 0 };

        if self.current_len > 0 && self.current_len + total > wrap_width {
            self.flush_line();
        } else if need_space {
            self.current_spans.push(Span::raw(" "));
            self.current_len += 1;
        }

        let span_start = self.current_spans.len();
        self.current_spans
            .push(Span::styled(label, Style::default().fg(Color::Cyan)));
        if !trimmed_text.is_empty() {
            self.current_spans.push(Span::raw(" "));
            self.current_spans.push(Span::styled(
                trimmed_text.to_owned(),
                Style::default().fg(Color::Cyan),
            ));
        }
        let span_end = self.current_spans.len();
        self.current_len += display_len;

        // navigate できるリンクのみ link_positions に登録する
        if let Some(idx) = link_idx {
            self.pending_links.push((idx, span_start, span_end));
        }
    }

    /// current_spans を1行として確定し、pending_links を記録する
    fn flush_line(&mut self) {
        let line_idx = self.lines.len();
        for (link_idx, span_start, span_end) in self.pending_links.drain(..) {
            self.link_positions.push((line_idx, link_idx));
            self.link_span_ranges.push((line_idx, span_start, span_end));
        }
        if !self.current_spans.is_empty() {
            self.lines.push(Line::from(take(&mut self.current_spans)));
        }
        self.current_len = 0;
    }

    /// 未フラッシュのスパンを先に出力してから空行を挿入する
    fn push_empty_line(&mut self) {
        self.flush_line();
        self.lines.push(Line::from(""));
    }
}

/// Inline のテキスト部分を連結してプレーン文字列にする（ヘッダー描画用）
fn inlines_to_text(inlines: &[Inline]) -> String {
    inlines
        .iter()
        .map(|i| match i {
            Inline::Text(s)
            | Inline::Bold(s)
            | Inline::Italic(s)
            | Inline::BoldItalic(s)
            | Inline::Code(s) => s.as_str(),
            Inline::Link { text, .. } => text.as_str(),
            Inline::SoftBreak => " ",
        })
        .collect()
}
