use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::collections::HashMap;
use std::mem::take;
use stillo_core::{
    document::{ExtractedContent, ExtractedLink},
    Block, Document, Inline,
}; // ExtractedContent は from_content ラッパーで使用
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use url::Url;

use crate::text::{display_width, split_to_width, truncate_to_width};

/// 端末幅が分かる前（初回描画前）に使う折り返し幅
pub const DEFAULT_WIDTH: usize = 80;
/// コードブロック本文の最大幅。広い端末で枠が画面幅いっぱいに伸びると読みづらいため上限を設ける
const CODE_MAX_WIDTH: usize = 76;
/// H2 見出しの罫線・水平線の最大幅
const RULE_MAX_WIDTH: usize = 60;

/// 折り返し幅に依存しないスクロール位置。
/// 行番号は端末幅で変わるため、「何番目のブロックの何行目か」で保持し、
/// リサイズや履歴復帰で再レイアウトした後も同じ箇所を表示できるようにする。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrollAnchor {
    block: usize,
    line_in_block: usize,
}

/// リンクのハイライト対象スパン。折り返しで 1 リンクが複数行・複数スパンにまたがるため、
/// スパン単位で記録する。
#[derive(Debug, Clone, Copy)]
struct LinkSpan {
    pos_idx: usize,
    line: usize,
    span: usize,
}

/// HTML コンテンツを意味的 AST 経由で ratatui Lines に変換し、
/// スクロールとリンク選択を管理する。
pub struct ContentView {
    pub lines: Vec<Line<'static>>,
    pub scroll_offset: usize,
    pub selected_link: Option<usize>,
    /// (line_idx, link_idx_in_content_links) — navigate 用。line_idx はリンク先頭の行
    pub link_positions: Vec<(usize, usize)>,
    link_spans: Vec<LinkSpan>,
    /// 各行の生成元ブロック番号（非減少列）。ScrollAnchor との相互変換に使う
    line_blocks: Vec<usize>,
    width: usize,
}

impl ContentView {
    /// Document と links を指定幅で折り返して ContentView を構築する
    pub fn new(doc: &Document, links: &[ExtractedLink], width: usize) -> Self {
        let mut renderer = DocRenderer::new(links, width);
        renderer.render(doc);
        Self {
            lines: renderer.lines,
            link_positions: renderer.link_positions,
            link_spans: renderer.link_spans,
            line_blocks: renderer.line_blocks,
            width: renderer.width,
            scroll_offset: 0,
            selected_link: None,
        }
    }

    /// Document と links から既定幅で ContentView を構築する（フォーマット非依存エントリポイント）
    pub fn from_document(doc: &Document, links: &[ExtractedLink]) -> Self {
        Self::new(doc, links, DEFAULT_WIDTH)
    }

    /// HTML コンテンツから ContentView を構築する（後方互換ラッパー）
    pub fn from_content(content: &ExtractedContent) -> Self {
        let doc = stillo_core::parse_html_to_ast(&content.body_html, &content.url);
        Self::from_document(&doc, &content.links)
    }

    /// 現在の折り返し幅
    pub fn width(&self) -> usize {
        self.width
    }

    /// 幅を変えて再レイアウトする。行番号ベースの状態（スクロール位置）はブロック基準で引き継ぎ、
    /// リンク選択は文書内の順序で引き継ぐ（リンクの並び順は幅に依存しないため）。
    pub fn relayout(&mut self, doc: &Document, links: &[ExtractedLink], width: usize) {
        let anchor = self.anchor();
        let selected = self.selected_link;
        *self = Self::new(doc, links, width);
        self.restore_anchor(anchor);
        self.selected_link = selected.filter(|&i| i < self.link_positions.len());
        self.rebuild_link_highlights();
    }

    pub fn total_lines(&self) -> usize {
        self.lines.len()
    }

    /// 現在のスクロール位置を幅非依存の形で返す
    pub fn anchor(&self) -> ScrollAnchor {
        self.anchor_of_line(self.scroll_offset)
    }

    pub fn restore_anchor(&mut self, anchor: ScrollAnchor) {
        self.scroll_offset = self.line_for_anchor(anchor);
    }

    fn anchor_of_line(&self, line: usize) -> ScrollAnchor {
        let Some(&block) = self.line_blocks.get(line) else {
            return ScrollAnchor::default();
        };
        let first = self.line_blocks.partition_point(|&b| b < block);
        ScrollAnchor {
            block,
            line_in_block: line - first,
        }
    }

    fn line_for_anchor(&self, anchor: ScrollAnchor) -> usize {
        let first = self.line_blocks.partition_point(|&b| b < anchor.block);
        let end = self.line_blocks.partition_point(|&b| b <= anchor.block);
        let line = if first == end {
            first
        } else {
            (first + anchor.line_in_block).min(end - 1)
        };
        line.min(self.lines.len().saturating_sub(1))
    }

    /// 行の生成元ブロック番号
    pub fn block_of_line(&self, line: usize) -> Option<usize> {
        self.line_blocks.get(line).copied()
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

    pub fn next_link(&mut self, viewport_height: usize) {
        if self.link_positions.is_empty() {
            return;
        }
        self.selected_link = Some(match self.selected_link {
            None => 0,
            Some(i) => (i + 1).min(self.link_positions.len() - 1),
        });
        self.scroll_to_selected_link(viewport_height);
        self.rebuild_link_highlights();
    }

    pub fn prev_link(&mut self, viewport_height: usize) {
        if self.link_positions.is_empty() {
            return;
        }
        self.selected_link = Some(match self.selected_link {
            None => 0,
            Some(i) => i.saturating_sub(1),
        });
        self.scroll_to_selected_link(viewport_height);
        self.rebuild_link_highlights();
    }

    pub fn selected_link_url<'a>(&self, links: &'a [ExtractedLink]) -> Option<&'a Url> {
        let sel = self.selected_link?;
        let (_, link_idx) = self.link_positions.get(sel)?;
        links.get(*link_idx).map(|l| &l.href)
    }

    /// 選択中のリンクが表示領域内に入るようスクロールする。
    /// 複数行に折り返されたリンクは末尾行まで見せるが、先頭行が画面外に出る場合は先頭を優先する。
    fn scroll_to_selected_link(&mut self, viewport_height: usize) {
        let Some(sel) = self.selected_link else {
            return;
        };
        let Some(&(first, _)) = self.link_positions.get(sel) else {
            return;
        };
        let last = self
            .link_spans
            .iter()
            .filter(|s| s.pos_idx == sel)
            .map(|s| s.line)
            .max()
            .unwrap_or(first);
        let height = viewport_height.max(1);
        if first < self.scroll_offset {
            self.scroll_offset = first;
        } else if last >= self.scroll_offset + height {
            self.scroll_offset = (last + 1).saturating_sub(height).min(first);
        }
    }

    /// 選択状態変化後に span 単位でハイライトを更新する。
    /// 行全体を上書きするのではなく span だけを変えることで、
    /// 通常テキストのスタイルを保持できる。
    fn rebuild_link_highlights(&mut self) {
        for ls in &self.link_spans {
            let style = if self.selected_link == Some(ls.pos_idx) {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default().fg(Color::Cyan)
            };
            if let Some(span) = self
                .lines
                .get_mut(ls.line)
                .and_then(|l| l.spans.get_mut(ls.span))
            {
                span.style = style;
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

/// リンク照合用に href を正規化する。末尾スラッシュの有無だけが異なる URL を同一視するため
fn normalize_href(href: &str) -> &str {
    href.trim_end_matches('/')
}

/// Document → ratatui Lines の変換器
struct DocRenderer<'a> {
    /// 正規化 href → content.links の添字。リンクごとの線形探索を避けるため事前に索引化する
    link_index: HashMap<&'a str, usize>,
    width: usize,
    lines: Vec<Line<'static>>,
    line_blocks: Vec<usize>,
    current_block: usize,
    link_positions: Vec<(usize, usize)>,
    link_spans: Vec<LinkSpan>,
    link_counter: usize,
    current_spans: Vec<Span<'static>>,
    /// 現在行の表示幅（セル数）
    current_len: usize,
    /// 折り返し後の継続行の先頭に付けるスパン（リストのインデント、引用の縦線など）
    indent: Option<Span<'static>>,
    indent_width: usize,
    /// 現在行で行番号が未確定のリンクスパン: (pos_idx, span_idx)
    pending_link_spans: Vec<(usize, usize)>,
    /// 出力中の登録済みリンクの pos_idx
    active_link: Option<usize>,
    /// active_link の本体スパンを 1 つ以上出力したか。リンク直前の区切り空白をハイライトしないため
    active_link_emitted: bool,
}

impl<'a> DocRenderer<'a> {
    fn new(links: &'a [ExtractedLink], width: usize) -> Self {
        let mut link_index = HashMap::with_capacity(links.len());
        for (i, l) in links.iter().enumerate() {
            // 同じ正規化 href が複数ある場合は従来の線形探索と同じく先頭を採用する
            link_index
                .entry(normalize_href(l.href.as_str()))
                .or_insert(i);
        }
        Self {
            link_index,
            // 幅 0 では 1 セルも置けず折り返しが成立しないため最小 1 とする
            width: width.max(1),
            lines: Vec::new(),
            line_blocks: Vec::new(),
            current_block: 0,
            link_positions: Vec::new(),
            link_spans: Vec::new(),
            link_counter: 0,
            current_spans: Vec::new(),
            current_len: 0,
            indent: None,
            indent_width: 0,
            pending_link_spans: Vec::new(),
            active_link: None,
            active_link_emitted: false,
        }
    }

    fn render(&mut self, doc: &Document) {
        for (i, block) in doc.blocks.iter().enumerate() {
            self.current_block = i;
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
        let cyan_bold = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        match level {
            1 => {
                let style = Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD);
                let start = self.lines.len();
                self.push_text(&text, style);
                self.flush_line();
                // 下線はタイトルの表示幅に合わせる（全角文字は 2 セル）
                let underline_width = self.lines[start..]
                    .iter()
                    .map(|l| l.width())
                    .max()
                    .unwrap_or(0);
                self.push_line(Line::from(Span::styled(
                    "═".repeat(underline_width),
                    Style::default().fg(Color::Yellow),
                )));
            }
            2 => {
                // "── テキスト ─────..." (合計 RULE_MAX_WIDTH セル、端末幅が狭ければ端末幅)
                self.begin_prefixed("── ", cyan_bold, None);
                self.push_text(&text, cyan_bold);
                let target = RULE_MAX_WIDTH.min(self.width);
                if self.current_len + 1 < target {
                    let pad = "─".repeat(target - self.current_len - 1);
                    self.push_span(" ".to_owned(), cyan_bold, true);
                    self.push_span(pad, cyan_bold, false);
                }
                self.end_prefixed();
            }
            3 => {
                self.begin_prefixed("▸ ", cyan_bold, None);
                self.push_text(&text, cyan_bold);
                self.end_prefixed();
            }
            _ => {
                let bold = Style::default().add_modifier(Modifier::BOLD);
                self.begin_prefixed("  § ", bold, None);
                self.push_text(&text, bold);
                self.end_prefixed();
            }
        }
        self.push_empty_line();
    }

    fn render_paragraph(&mut self, inlines: &[Inline]) {
        self.push_empty_line();
        self.render_inlines(inlines, Style::default());
        self.flush_line();
    }

    fn render_list_item(&mut self, depth: usize, ordered: bool, number: usize, inlines: &[Inline]) {
        let prefix = if ordered {
            format!("{}{number}. ", "  ".repeat(depth))
        } else {
            format!("{}• ", "  ".repeat(depth.saturating_sub(1)))
        };
        self.begin_prefixed(&prefix, Style::default().fg(Color::DarkGray), None);
        self.render_inlines(inlines, Style::default());
        self.end_prefixed();
    }

    fn render_code_block(&mut self, lang: Option<&str>, content: &str) {
        self.flush_line();
        self.push_empty_line();

        let border = Style::default().fg(Color::DarkGray);
        // 枠全体を端末幅に収める: "│ " + 本文 + " │" で本文以外に 4 セル使う
        let code_width = CODE_MAX_WIDTH.min(self.width.saturating_sub(4)).max(1);

        // 上境界線: ╭─ {lang} ─────╮
        let lang_label = lang
            .map(|l| format!(" {} ", l))
            .unwrap_or_else(|| " ".to_owned());
        // 右側に最低 1 本の罫線を残すため、ラベルは本文幅 + 1 セルまでに切り詰める
        let lang_label = truncate_to_width(&lang_label, code_width + 1);
        let lang_pad = (code_width + 2).saturating_sub(display_width(&lang_label) + 1);
        let top = format!("╭─{}{}╮", lang_label, "─".repeat(lang_pad));
        self.push_line(Line::from(Span::styled(top, border)));

        // コンテンツ行。本文幅を超える行は切り捨てずに複数行へ折り返す
        for line in content.lines() {
            for row in split_to_width(line, code_width) {
                let pad = code_width.saturating_sub(display_width(&row));
                let padded = format!("{}{}", row, " ".repeat(pad));
                self.push_line(Line::from(vec![
                    Span::styled("│ ", border),
                    Span::styled(padded, Style::default().fg(Color::Yellow)),
                    Span::styled(" │", border),
                ]));
            }
        }

        // 下境界線: ╰──────────╯
        let bottom = format!("╰{}╯", "─".repeat(code_width + 2));
        self.push_line(Line::from(Span::styled(bottom, border)));

        self.push_empty_line();
    }

    fn render_blockquote(&mut self, inlines: &[Inline]) {
        self.push_empty_line();
        let bar = Style::default().fg(Color::Cyan);
        // 折り返した継続行にも引用の縦線を付ける
        self.begin_prefixed("▎ ", bar, Some(Span::styled("▎ ", bar)));
        let italic_style = Style::default().add_modifier(Modifier::ITALIC);
        self.render_inlines(inlines, italic_style);
        self.end_prefixed();
        self.push_empty_line();
    }

    fn render_rule(&mut self) {
        self.flush_line();
        self.push_line(Line::from(Span::styled(
            "─".repeat(RULE_MAX_WIDTH.min(self.width)),
            Style::default().fg(Color::DarkGray),
        )));
        self.push_empty_line();
    }

    /// インライン要素列をワードラップしながら current_spans に追加する
    fn render_inlines(&mut self, inlines: &[Inline], base_style: Style) {
        for inline in inlines {
            match inline {
                Inline::Text(t) => self.push_text(t, base_style),
                Inline::Bold(t) => self.push_text(t, base_style.add_modifier(Modifier::BOLD)),
                Inline::Italic(t) => {
                    self.push_text(t, base_style.add_modifier(Modifier::ITALIC));
                }
                Inline::BoldItalic(t) => {
                    self.push_text(
                        t,
                        base_style
                            .add_modifier(Modifier::BOLD)
                            .add_modifier(Modifier::ITALIC),
                    );
                }
                Inline::Code(t) => {
                    self.push_word(&format!("`{}`", t), Style::default().fg(Color::Yellow));
                }
                Inline::Link { text, href } => self.push_link(text, href),
                Inline::SoftBreak => self.flush_line(),
            }
        }
    }

    /// 先頭記号付きブロック（見出し・リスト・引用）を開始する。
    /// 継続行には `continuation` を、指定がなければ記号と同じ幅の空白を付ける。
    fn begin_prefixed(&mut self, prefix: &str, style: Style, continuation: Option<Span<'static>>) {
        self.flush_line();
        let width = display_width(prefix);
        self.current_spans
            .push(Span::styled(prefix.to_owned(), style));
        self.current_len = width;
        self.indent = Some(continuation.unwrap_or_else(|| Span::raw(" ".repeat(width))));
        self.indent_width = width;
    }

    fn end_prefixed(&mut self) {
        self.flush_line();
        self.indent = None;
        self.indent_width = 0;
    }

    /// 現在行が空なら継続行インデントを置く
    fn start_line(&mut self) {
        if self.current_spans.is_empty() {
            if let Some(indent) = &self.indent {
                self.current_spans.push(indent.clone());
                self.current_len = self.indent_width;
            }
        }
    }

    /// 現在行にインデント以外の内容があるか
    fn line_has_content(&self) -> bool {
        self.current_len > self.indent_width
    }

    /// スパンを現在行に追加する。リンク出力中ならハイライト対象として記録する。
    /// `is_separator` の空白はリンク本体を 1 つ出した後のものだけをリンクに含める。
    fn push_span(&mut self, text: String, style: Style, is_separator: bool) {
        let width = display_width(&text);
        if let Some(pos) = self.active_link {
            if !is_separator || self.active_link_emitted {
                self.pending_link_spans
                    .push((pos, self.current_spans.len()));
            }
            if !is_separator {
                self.active_link_emitted = true;
            }
        }
        self.current_spans.push(Span::styled(text, style));
        self.current_len += width;
    }

    /// テキストを空白区切りの語単位で追加する
    fn push_text(&mut self, text: &str, style: Style) {
        for word in text.split_whitespace() {
            self.push_word(word, style);
        }
    }

    /// 1 語を追加する。行に収まらなければ改行し、行幅そのものを超える語は grapheme 単位で分割する。
    fn push_word(&mut self, word: &str, style: Style) {
        if word.is_empty() {
            return;
        }
        self.start_line();
        let w = word.width();
        let need_space = self.line_has_content();
        let sep = usize::from(need_space);
        if self.current_len + sep + w <= self.width {
            if need_space {
                self.push_span(" ".to_owned(), Style::default(), true);
            }
            self.push_span(word.to_owned(), style, false);
            return;
        }
        let avail = self.width.saturating_sub(self.indent_width);
        if w <= avail {
            self.flush_line();
            self.start_line();
            self.push_span(word.to_owned(), style, false);
            return;
        }
        // 行幅を超える語（空白を含まない日本語の段落、長い URL など）は改行してもはみ出すため、
        // 現在行の残りから詰めて分割する。語単位で改行すると直前の行に大きな空きが残る
        if need_space {
            if self.current_len + 1 < self.width {
                self.push_span(" ".to_owned(), Style::default(), true);
            } else {
                self.flush_line();
                self.start_line();
            }
        }
        self.push_graphemes(word, style);
    }

    /// grapheme 単位で行幅に合わせて分割しながら追加する。
    /// 行頭では幅を超える grapheme でも 1 つは置き、無限ループを防ぐ。
    fn push_graphemes(&mut self, text: &str, style: Style) {
        let mut chunk = String::new();
        let mut chunk_width = 0;
        for g in text.graphemes(true) {
            let gw = g.width();
            let has_content = chunk_width > 0 || self.line_has_content();
            if self.current_len + chunk_width + gw > self.width && has_content {
                if !chunk.is_empty() {
                    self.push_span(take(&mut chunk), style, false);
                    chunk_width = 0;
                }
                self.flush_line();
                self.start_line();
            }
            chunk.push_str(g);
            chunk_width += gw;
        }
        if !chunk.is_empty() {
            self.push_span(chunk, style, false);
        }
    }

    /// リンクを出力する。content.links に一致するものがあれば link_positions に記録する。
    fn push_link(&mut self, text: &str, href: &str) {
        let link_idx = self.link_index.get(normalize_href(href)).copied();

        self.link_counter += 1;
        let label = format!("[{}]", self.link_counter);
        let style = Style::default().fg(Color::Cyan);

        // 番号ラベルだけが行末に残ると対応が分かりにくいため、先頭の語と同じ行に置く
        let head_width = display_width(&label)
            + text
                .split_whitespace()
                .next()
                .map(|w| 1 + w.width())
                .unwrap_or(0);
        let avail = self.width.saturating_sub(self.indent_width);
        if self.line_has_content()
            && self.current_len + 1 + head_width > self.width
            && head_width <= avail
        {
            self.flush_line();
        }

        // navigate できるリンクのみ link_positions に登録する。行番号は flush_line で確定する
        self.active_link = link_idx.map(|idx| {
            self.link_positions.push((usize::MAX, idx));
            self.link_positions.len() - 1
        });
        self.active_link_emitted = false;
        self.push_word(&label, style);
        self.push_text(text, style);
        self.active_link = None;
    }

    fn push_line(&mut self, line: Line<'static>) {
        self.lines.push(line);
        self.line_blocks.push(self.current_block);
    }

    /// current_spans を1行として確定し、リンクスパンの行番号を記録する
    fn flush_line(&mut self) {
        if !self.current_spans.is_empty() {
            let line_idx = self.lines.len();
            for (pos_idx, span) in self.pending_link_spans.drain(..) {
                let first_line = &mut self.link_positions[pos_idx].0;
                if *first_line == usize::MAX {
                    *first_line = line_idx;
                }
                self.link_spans.push(LinkSpan {
                    pos_idx,
                    line: line_idx,
                    span,
                });
            }
            let spans = take(&mut self.current_spans);
            self.push_line(Line::from(spans));
        }
        self.current_len = 0;
    }

    /// 未フラッシュのスパンを先に出力してから空行を挿入する
    fn push_empty_line(&mut self) {
        self.flush_line();
        self.push_line(Line::from(""));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn link(href: &str) -> ExtractedLink {
        ExtractedLink {
            text: String::new(),
            href: Url::parse(href).unwrap(),
            rel: None,
        }
    }

    fn doc(blocks: Vec<Block>) -> Document {
        Document { blocks }
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn all_text(view: &ContentView) -> String {
        view.lines.iter().map(line_text).collect()
    }

    fn assert_fits(view: &ContentView, width: usize) {
        for (i, l) in view.lines.iter().enumerate() {
            assert!(
                l.width() <= width,
                "line {i} is {} cells wide (> {width}): {:?}",
                l.width(),
                line_text(l)
            );
        }
    }

    /// 空白のない日本語段落が端末幅で折り返され、本文が欠落しないこと
    #[test]
    fn japanese_paragraph_wraps_without_loss() {
        let text = "吾輩は猫である。名前はまだ無い。どこで生れたかとんと見当がつかぬ。".repeat(5);
        let d = doc(vec![Block::Paragraph(vec![Inline::Text(text.clone())])]);
        let view = ContentView::new(&d, &[], 30);
        assert_fits(&view, 30);
        assert!(view.lines.len() > 5);
        assert_eq!(all_text(&view), text);
    }

    /// 全角文字は 2 セルとして数え、行が端末幅を超えないこと
    #[test]
    fn wide_chars_are_counted_by_display_width() {
        let d = doc(vec![Block::Paragraph(vec![Inline::Text("あ".repeat(80))])]);
        let view = ContentView::new(&d, &[], 80);
        assert_fits(&view, 80);
        // 160 セル分の本文なので 80 セル幅では 2 行になる
        assert_eq!(view.lines.iter().filter(|l| l.width() > 0).count(), 2);
    }

    /// 80 列未満の端末では端末幅で折り返すこと
    #[test]
    fn narrow_terminal_wraps_english_words() {
        let text = "the quick brown fox jumps over the lazy dog ".repeat(4);
        let d = doc(vec![Block::Paragraph(vec![Inline::Text(text.clone())])]);
        let view = ContentView::new(&d, &[], 20);
        assert_fits(&view, 20);
        let words: Vec<String> = view
            .lines
            .iter()
            .flat_map(|l| {
                line_text(l)
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(words.join(" "), text.trim());
    }

    #[test]
    fn list_and_blockquote_continuation_lines_are_indented() {
        let d = doc(vec![
            Block::ListItem {
                depth: 1,
                ordered: false,
                number: 0,
                inlines: vec![Inline::Text("alpha beta gamma delta epsilon".into())],
            },
            Block::Blockquote(vec![Inline::Text("一二三四五六七八九十".into())]),
        ]);
        let view = ContentView::new(&d, &[], 14);
        assert_fits(&view, 14);
        let texts: Vec<String> = view.lines.iter().map(line_text).collect();
        assert!(texts[0].starts_with("• alpha"));
        assert!(texts[1].starts_with("  "));
        let quote: Vec<&String> = texts.iter().filter(|t| t.starts_with("▎ ")).collect();
        assert!(quote.len() >= 2, "{texts:?}");
    }

    /// H1 下線・H2 罫線・コードブロック枠が表示幅基準で揃うこと
    #[test]
    fn decorations_use_display_width() {
        let d = doc(vec![
            Block::Heading {
                level: 1,
                inlines: vec![Inline::Text("日本語の見出し".into())],
            },
            Block::Heading {
                level: 2,
                inlines: vec![Inline::Text("節".into())],
            },
            Block::CodeBlock {
                lang: Some("rust".into()),
                content: format!("let s = \"{}\";\nshort", "漢".repeat(50)),
            },
        ]);
        let view = ContentView::new(&d, &[], 40);
        assert_fits(&view, 40);
        let texts: Vec<String> = view.lines.iter().map(line_text).collect();
        let h1 = texts.iter().position(|t| t == "日本語の見出し").unwrap();
        assert_eq!(
            display_width(&texts[h1 + 1]),
            display_width("日本語の見出し")
        );
        let h2 = texts.iter().find(|t| t.starts_with("── 節")).unwrap();
        assert_eq!(display_width(h2), 40);
        // コードブロックの枠は全行同じ幅で、長い行は折り返されて欠落しない
        let code: Vec<&String> = texts
            .iter()
            .filter(|t| t.starts_with('╭') || t.starts_with('│') || t.starts_with('╰'))
            .collect();
        assert!(code.len() > 4);
        assert!(code.iter().all(|t| display_width(t) == 40), "{code:?}");
        let body: String = code
            .iter()
            .filter(|t| t.starts_with('│'))
            .map(|t| t.trim_start_matches("│ ").trim_end_matches(" │").trim_end())
            .collect();
        assert!(body.contains(&"漢".repeat(50)));
    }

    /// 折り返されたリンクは全断片がハイライトされ、リンク直前の区切り空白は含まれないこと
    #[test]
    fn wrapped_link_highlights_all_fragments() {
        let links = vec![link("https://example.com/a")];
        let d = doc(vec![Block::Paragraph(vec![
            Inline::Text("intro".into()),
            Inline::Link {
                text: "リンクの文字列がとても長いので折り返される".into(),
                href: "https://example.com/a/".into(), // 末尾スラッシュ違いでも照合できる
            },
        ])]);
        let mut view = ContentView::new(&d, &links, 20);
        assert_fits(&view, 20);
        assert_eq!(view.link_positions.len(), 1);
        view.next_link(10);
        let selected = Style::default().fg(Color::Black).bg(Color::Cyan);
        let highlighted: Vec<(usize, String)> = view
            .lines
            .iter()
            .enumerate()
            .flat_map(|(i, l)| {
                l.spans
                    .iter()
                    .filter(|s| s.style == selected)
                    .map(move |s| (i, s.content.to_string()))
            })
            .collect();
        let lines: std::collections::BTreeSet<usize> = highlighted.iter().map(|h| h.0).collect();
        assert!(lines.len() >= 2, "{highlighted:?}");
        assert_eq!(*lines.first().unwrap(), view.link_positions[0].0);
        let text: String = highlighted.iter().map(|h| h.1.as_str()).collect();
        assert_eq!(text, "[1] リンクの文字列がとても長いので折り返される");
        // "intro" と "[1]" の間の空白はリンクに含めない
        assert_eq!(line_text(&view.lines[1]).find("intro"), Some(0));
        let first_line_spans = &view.lines[view.link_positions[0].0].spans;
        let sep = first_line_spans.iter().find(|s| s.content == " ").unwrap();
        assert_ne!(sep.style, selected);
    }

    #[test]
    fn link_lookup_prefers_first_match() {
        let links = vec![
            link("https://example.com/x/"),
            link("https://example.com/x"),
        ];
        let d = doc(vec![Block::Paragraph(vec![Inline::Link {
            text: "x".into(),
            href: "https://example.com/x".into(),
        }])]);
        let view = ContentView::new(&d, &links, 80);
        assert_eq!(view.link_positions[0].1, 0);
    }

    /// Tab で画面下のリンクを選択すると、そのリンクが見える位置までスクロールすること
    #[test]
    fn next_link_scrolls_down_to_offscreen_link() {
        let links = vec![
            link("https://example.com/top"),
            link("https://example.com/bottom"),
        ];
        let mut blocks = vec![Block::Paragraph(vec![Inline::Link {
            text: "top".into(),
            href: "https://example.com/top".into(),
        }])];
        for i in 0..30 {
            blocks.push(Block::Paragraph(vec![Inline::Text(format!("filler {i}"))]));
        }
        blocks.push(Block::Paragraph(vec![Inline::Link {
            text: "bottom".into(),
            href: "https://example.com/bottom".into(),
        }]));
        let mut view = ContentView::new(&doc(blocks), &links, 80);
        let height = 10;
        view.next_link(height);
        assert_eq!(view.scroll_offset, 0);
        view.next_link(height);
        let line = view.link_positions[1].0;
        assert!(line >= view.scroll_offset && line < view.scroll_offset + height);
        view.prev_link(height);
        let line = view.link_positions[0].0;
        assert!(line >= view.scroll_offset && line < view.scroll_offset + height);
    }

    /// 再レイアウト後もスクロール位置（ブロック基準）と選択リンクが保たれること
    #[test]
    fn relayout_preserves_anchor_and_selection() {
        let links = vec![link("https://example.com/target")];
        let mut blocks = Vec::new();
        for i in 0..20 {
            blocks.push(Block::Paragraph(vec![Inline::Text(format!(
                "paragraph {i} with some words that wrap on narrow terminals"
            ))]));
        }
        blocks.push(Block::Paragraph(vec![Inline::Link {
            text: "target".into(),
            href: "https://example.com/target".into(),
        }]));
        let d = doc(blocks);
        let mut view = ContentView::new(&d, &links, 80);
        // 段落 10 の本文行（空行の次）を先頭に表示する
        let line = view.line_blocks.iter().position(|&b| b == 10).unwrap() + 1;
        view.next_link(5);
        view.scroll_offset = line;
        let anchor = view.anchor();
        let selected_url = view.selected_link_url(&links).cloned();

        view.relayout(&d, &links, 20);
        assert_eq!(view.width(), 20);
        assert_eq!(view.anchor(), anchor);
        assert_eq!(view.selected_link_url(&links).cloned(), selected_url);
        assert!(line_text(&view.lines[view.scroll_offset]).starts_with("paragraph"));
        let selected = Style::default().fg(Color::Black).bg(Color::Cyan);
        let line = view.link_positions[0].0;
        assert!(view.lines[line].spans.iter().any(|s| s.style == selected));
    }
}
