use anyhow::Result;
use crossterm::{
    cursor,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame, Terminal,
};
use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use std::time::Duration;
use stillo_core::document::BrowsePage;
use url::Url;

use crate::text::truncate_to_width;
use crate::widgets::{
    content_view::{ContentView, ScrollAnchor},
    link_bar::{render_hint_bar, render_input_bar},
    status_bar::render_status_bar,
};

pub enum TuiResult {
    Navigate(Url),
    Reload,
    /// Web検索クエリ（検索チェーンは呼び出し側が実行する）
    WebSearch(String),
    Dump,
    Quit,
}

// ---------------------------------------------------------------------------
// 端末セッション
// ---------------------------------------------------------------------------

/// raw mode / alternate screen を有効にしている間だけ true。
/// panic hook はこれを見て復元要否を決める。セッション外の panic で制御シーケンスを出すと、
/// stdout をファイルへリダイレクトしている場合（dump など）に混入するため。
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

/// 端末を通常状態に戻す。複数回呼んでも 2 回目以降は何もしない。
fn restore_terminal() {
    restore_terminal_to(&mut io::stdout());
}

/// 各手順を独立に実行し、エラーは無視する。
/// `disable_raw_mode()?` のように途中で return すると alternate screen の解除が飛ばされるため。
fn restore_terminal_to(out: &mut impl Write) {
    if !TUI_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let _ = terminal::disable_raw_mode();
    let _ = execute!(out, DisableMouseCapture);
    let _ = execute!(out, LeaveAlternateScreen);
    let _ = execute!(out, cursor::Show);
}

/// panic 時に端末を復元してから既存フックを呼ぶ。
/// 先に復元しないと panic メッセージが alternate screen に書かれて読めず、シェルも raw mode のまま残る。
fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            prev(info);
        }));
    });
}

/// Drop 時に端末を復元するガード。`?` による早期 return や panic の unwind でも復元させるため
struct RestoreGuard;

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// alternate screen・raw mode・マウスキャプチャを保持する端末セッション。
/// ページ遷移のたびに alternate screen を出入りするとフェッチ中に元のシェル画面が見えるため、
/// ブラウズ全体で 1 つ保持する。Drop（panic の unwind を含む）で端末を復元する。
pub struct TuiSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    // terminal より後に宣言し、Drop 順で最後に復元させる
    _guard: RestoreGuard,
}

impl TuiSession {
    pub fn start() -> Result<Self> {
        install_panic_hook();
        // 端末操作の途中で失敗しても、それまでに変更した状態を戻せるよう先にガードを作る
        TUI_ACTIVE.store(true, Ordering::SeqCst);
        let guard = RestoreGuard;
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self {
            terminal,
            _guard: guard,
        })
    }

    /// セッションを終了して端末を復元する（Drop と同じ。呼び出し側で終了時点を明示するため）
    pub fn end(self) {}
}

// ---------------------------------------------------------------------------
// ブラウザ
// ---------------------------------------------------------------------------

enum BrowserMode {
    Normal,
    SearchInput(String), // ページ内テキスト検索（/）
    WebSearch(String),   // DuckDuckGo 検索（s）
    UrlInput(String),    // URL直接入力（U）
    Help,
}

pub struct TuiBrowser {
    page: BrowsePage,
    view: ContentView,
    mode: BrowserMode,
    /// 最後に実行したページ内検索。再レイアウトで行番号が変わったときにマッチを再計算するため保持する
    search_query: String,
    search_matches: Vec<usize>,
    search_cursor: usize,
    history: Vec<(BrowsePage, ScrollAnchor)>,
    forward_history: Vec<(BrowsePage, ScrollAnchor)>,
    /// ヒントバーに表示するエラー通知。次のキー入力で消える
    notice: Option<String>,
    /// フェッチ待ちの間ヒントバーに表示するメッセージ
    loading: Option<String>,
}

impl TuiBrowser {
    /// 受け取ったページは描画前に制御文字を除去する（端末エスケープシーケンスの注入対策）。
    /// 折り返し幅は初回描画時に端末幅へ合わせて再レイアウトする。
    pub fn new(page: BrowsePage) -> Self {
        let page = page.sanitize_for_terminal();
        let view = ContentView::from_document(&page.doc, &page.links);
        Self {
            page,
            view,
            mode: BrowserMode::Normal,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_cursor: 0,
            history: Vec::new(),
            forward_history: Vec::new(),
            notice: None,
            loading: None,
        }
    }

    /// 遷移・リロード・検索の失敗を、TUI を終了せずにヒントバーへ表示する。
    /// 呼び出し側は表示後に再度 run() を呼んでブラウズを続行する。
    pub fn show_error(&mut self, message: impl std::fmt::Display) {
        self.loading = None;
        self.notice = Some(single_line(message));
    }

    /// 現在ページを履歴に積んで新ページへ遷移する。CLIのナビゲーションループから呼ぶ。
    pub fn load_page(&mut self, page: BrowsePage) {
        let page = page.sanitize_for_terminal();
        let anchor = self.view.anchor();
        let old_page = std::mem::replace(&mut self.page, page);
        self.history.push((old_page, anchor));
        self.forward_history.clear();
        self.view = ContentView::new(&self.page.doc, &self.page.links, self.view.width());
        self.reset_page_state();
    }

    /// 現在ページを履歴に積まずに置き換える（リロード用）。
    pub fn reload_page(&mut self, page: BrowsePage) {
        let page = page.sanitize_for_terminal();
        self.view = ContentView::new(&page.doc, &page.links, self.view.width());
        self.page = page;
        self.reset_page_state();
    }

    fn reset_page_state(&mut self) {
        self.mode = BrowserMode::Normal;
        self.loading = None;
        self.search_query.clear();
        self.search_matches.clear();
        self.search_cursor = 0;
    }

    pub fn current_url(&self) -> &Url {
        &self.page.url
    }

    pub fn markdown(&self) -> &str {
        &self.page.markdown
    }

    /// キー入力を待ち、遷移など呼び出し側で処理すべき操作が発生したら返す。
    pub fn run(&mut self, session: &mut TuiSession) -> Result<TuiResult> {
        self.loading = None;
        loop {
            let viewport_height = self.draw(&mut session.terminal)?;
            let ev = event::read()?;
            if let Some(result) = self.handle_event(ev, viewport_height) {
                return Ok(result);
            }
        }
    }

    /// TUI を維持したまま読み込み中表示を出す。フェッチ開始前に呼ぶ。
    pub fn show_loading(
        &mut self,
        session: &mut TuiSession,
        message: impl std::fmt::Display,
    ) -> Result<()> {
        self.set_loading(message);
        self.draw(&mut session.terminal)?;
        Ok(())
    }

    fn set_loading(&mut self, message: impl std::fmt::Display) {
        self.notice = None;
        self.loading = Some(single_line(message));
    }

    /// 読み込み中に届いた入力を待たずに処理し、キャンセル（Esc / Ctrl-C）が押されたら true を返す。
    /// フェッチの Future と交互に呼ぶ想定のため、入力がなければ即座に返る。
    pub fn poll_cancel(&mut self, session: &mut TuiSession) -> Result<bool> {
        while event::poll(Duration::ZERO)? {
            let ev = event::read()?;
            if is_cancel_event(&ev) {
                self.loading = None;
                return Ok(true);
            }
            if matches!(ev, Event::Resize(..)) {
                self.draw(&mut session.terminal)?;
            }
        }
        Ok(false)
    }

    /// 端末幅に合わせて再レイアウトしてから描画し、コンテンツ領域の高さを返す。
    /// 描画のたびに幅を確認するため、Resize イベント後の描画で自動的に再レイアウトされる。
    fn draw<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> io::Result<usize> {
        let size = terminal.size()?;
        self.ensure_width(size.width as usize);
        terminal.draw(|f| self.render(f))?;
        Ok(size.height.saturating_sub(2) as usize)
    }

    /// 折り返し幅が変わっていれば再レイアウトし、行番号ベースの検索マッチを再計算する。
    fn ensure_width(&mut self, width: usize) {
        if width == self.view.width() {
            return;
        }
        // 現在マッチを「ブロック番号とブロック内で何番目か」で覚えておき、再計算後に引き継ぐ
        let current = self
            .search_matches
            .get(self.search_cursor)
            .and_then(|&line| {
                let block = self.view.block_of_line(line)?;
                let nth = self.search_matches[..self.search_cursor]
                    .iter()
                    .filter(|&&m| self.view.block_of_line(m) == Some(block))
                    .count();
                Some((block, nth))
            });
        self.view.relayout(&self.page.doc, &self.page.links, width);
        if self.search_query.is_empty() {
            return;
        }
        self.search_matches = self.view.search(&self.search_query);
        let view = &self.view;
        let block_of = |m: usize| view.block_of_line(m).unwrap_or(usize::MAX);
        let cursor = match current {
            Some((block, nth)) => {
                let start = self
                    .search_matches
                    .iter()
                    .position(|&m| block_of(m) >= block)
                    .unwrap_or(0);
                let in_block = self.search_matches[start..]
                    .iter()
                    .take_while(|&&m| block_of(m) == block)
                    .count();
                start + nth.min(in_block.saturating_sub(1))
            }
            None => 0,
        };
        self.search_cursor = cursor.min(self.search_matches.len().saturating_sub(1));
    }

    fn handle_event(&mut self, ev: Event, viewport_height: usize) -> Option<TuiResult> {
        match ev {
            // Windows では 1 打鍵で Press と Release が届くため、Release を処理すると二重入力になる
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.handle_key(key.code, key.modifiers, viewport_height)
            }
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollDown => self.view.scroll_down(3, viewport_height),
                    MouseEventKind::ScrollUp => self.view.scroll_up(3),
                    _ => {}
                }
                None
            }
            // Resize は次の draw で端末幅を読み直して再レイアウトするため、ここでは何もしない
            _ => None,
        }
    }

    fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        viewport_height: usize,
    ) -> Option<TuiResult> {
        self.notice = None;
        // raw mode では Ctrl-C で SIGINT が発生しないため、アプリ側で終了・キャンセルとして解釈する
        if is_ctrl_c(code, modifiers) {
            return match self.mode {
                BrowserMode::Normal => Some(TuiResult::Quit),
                _ => {
                    self.mode = BrowserMode::Normal;
                    None
                }
            };
        }
        match &self.mode {
            BrowserMode::Normal => self.handle_normal(code, modifiers, viewport_height),
            BrowserMode::SearchInput(_) | BrowserMode::WebSearch(_) | BrowserMode::UrlInput(_) => {
                self.handle_input(code, modifiers)
            }
            BrowserMode::Help => {
                self.mode = BrowserMode::Normal;
                None
            }
        }
    }

    fn handle_normal(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        viewport_height: usize,
    ) -> Option<TuiResult> {
        match code {
            // 終了
            KeyCode::Char('q') | KeyCode::Esc => return Some(TuiResult::Quit),

            // スクロール
            KeyCode::Char('j') | KeyCode::Down => self.view.scroll_down(1, viewport_height),
            KeyCode::Char('k') | KeyCode::Up => self.view.scroll_up(1),
            KeyCode::Char('d') if modifiers == KeyModifiers::CONTROL => {
                self.view.scroll_down(viewport_height / 2, viewport_height);
            }
            KeyCode::Char('u') if modifiers == KeyModifiers::CONTROL => {
                self.view.scroll_up(viewport_height / 2);
            }
            KeyCode::PageDown => self.view.scroll_down(viewport_height, viewport_height),
            KeyCode::PageUp => self.view.scroll_up(viewport_height),
            KeyCode::Char('g') | KeyCode::Home => self.view.scroll_to_top(),
            KeyCode::Char('G') | KeyCode::End => self.view.scroll_to_bottom(viewport_height),

            // リンクナビゲーション
            KeyCode::Tab => self.view.next_link(viewport_height),
            KeyCode::BackTab => self.view.prev_link(viewport_height),

            // リンクフォロー
            KeyCode::Enter => {
                if let Some(url) = self.view.selected_link_url(&self.page.links) {
                    return Some(TuiResult::Navigate(url.clone()));
                }
            }

            // 戻る
            KeyCode::Char('B') | KeyCode::Left => {
                if let Some((prev_page, prev_anchor)) = self.history.pop() {
                    let anchor = self.view.anchor();
                    let cur = std::mem::replace(&mut self.page, prev_page);
                    self.forward_history.push((cur, anchor));
                    self.show_history_page(prev_anchor);
                }
            }

            // 進む
            KeyCode::Char('F') | KeyCode::Right => {
                if let Some((next_page, next_anchor)) = self.forward_history.pop() {
                    let anchor = self.view.anchor();
                    let cur = std::mem::replace(&mut self.page, next_page);
                    self.history.push((cur, anchor));
                    self.show_history_page(next_anchor);
                }
            }

            // リロード
            KeyCode::Char('r') => return Some(TuiResult::Reload),

            // URL入力モード（現在URLをプリフィル）
            KeyCode::Char('U') => {
                let cur = self.page.url.to_string();
                self.mode = BrowserMode::UrlInput(cur);
            }

            // ページ内テキスト検索
            KeyCode::Char('/') => {
                self.mode = BrowserMode::SearchInput(String::new());
            }

            // Web 検索（DuckDuckGo）
            KeyCode::Char('s') => {
                self.mode = BrowserMode::WebSearch(String::new());
            }

            // 次の検索マッチ
            KeyCode::Char('n') => {
                if !self.search_matches.is_empty() {
                    self.search_cursor = (self.search_cursor + 1) % self.search_matches.len();
                    self.view.scroll_offset = self.search_matches[self.search_cursor];
                }
            }

            // 前の検索マッチ
            KeyCode::Char('N') => {
                if !self.search_matches.is_empty() {
                    self.search_cursor = if self.search_cursor == 0 {
                        self.search_matches.len() - 1
                    } else {
                        self.search_cursor - 1
                    };
                    self.view.scroll_offset = self.search_matches[self.search_cursor];
                }
            }

            // Markdown dump
            KeyCode::Char('d') => return Some(TuiResult::Dump),

            // ヘルプ
            KeyCode::Char('?') => {
                self.mode = BrowserMode::Help;
            }

            _ => {}
        }
        None
    }

    /// 履歴から戻したページを現在の幅でレイアウトし、保存していた位置へスクロールする。
    /// 位置をブロック基準で保存しているため、離れている間に端末幅が変わっても同じ箇所に戻れる。
    fn show_history_page(&mut self, anchor: ScrollAnchor) {
        let mut v = ContentView::new(&self.page.doc, &self.page.links, self.view.width());
        v.restore_anchor(anchor);
        self.view = v;
        self.search_query.clear();
        self.search_matches.clear();
        self.search_cursor = 0;
    }

    /// 入力モード（ページ内検索・Web検索・URL入力）共通のキー処理
    fn handle_input(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<TuiResult> {
        match code {
            KeyCode::Esc => {
                self.mode = BrowserMode::Normal;
            }
            KeyCode::Enter => {
                let mode = std::mem::replace(&mut self.mode, BrowserMode::Normal);
                return self.submit_input(mode);
            }
            KeyCode::Backspace => {
                if let Some(buf) = self.input_buffer_mut() {
                    buf.pop();
                }
            }
            // readline と同じく Ctrl-U で入力全体を消す（プリフィルされた URL の打ち直し用）
            KeyCode::Char('u') if modifiers == KeyModifiers::CONTROL => {
                if let Some(buf) = self.input_buffer_mut() {
                    buf.clear();
                }
            }
            KeyCode::Char(c) if is_text_input(modifiers) => {
                if let Some(buf) = self.input_buffer_mut() {
                    buf.push(c);
                }
            }
            _ => {}
        }
        None
    }

    fn input_buffer_mut(&mut self) -> Option<&mut String> {
        match &mut self.mode {
            BrowserMode::SearchInput(s) | BrowserMode::WebSearch(s) | BrowserMode::UrlInput(s) => {
                Some(s)
            }
            BrowserMode::Normal | BrowserMode::Help => None,
        }
    }

    /// Enter で確定した入力を処理する。self.mode は呼び出し側で Normal に戻してある
    fn submit_input(&mut self, mode: BrowserMode) -> Option<TuiResult> {
        match mode {
            BrowserMode::SearchInput(query) => {
                self.search_matches = self.view.search(&query);
                self.search_query = query;
                self.search_cursor = 0;
                if let Some(&line_idx) = self.search_matches.first() {
                    self.view.scroll_offset = line_idx;
                }
                None
            }
            BrowserMode::WebSearch(query) => {
                let query = query.trim();
                // 検索HTTPは呼び出し側（cli）で実行し、結果ページを load_page してもらう
                (!query.is_empty()).then(|| TuiResult::WebSearch(query.to_owned()))
            }
            BrowserMode::UrlInput(input) => {
                if let Ok(url) = input.parse::<Url>() {
                    return Some(TuiResult::Navigate(url));
                }
                // httpスキームを補完して再試行
                format!("https://{}", input)
                    .parse::<Url>()
                    .ok()
                    .map(TuiResult::Navigate)
            }
            BrowserMode::Normal | BrowserMode::Help => None,
        }
    }

    fn render(&self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // ステータスバー
                Constraint::Min(0),    // コンテンツ
                Constraint::Length(1), // ヒントバー / 入力バー
            ])
            .split(f.area());

        let total_lines = self.view.total_lines();
        let viewport_height = chunks[1].height as usize;
        let scroll_pct = if total_lines <= viewport_height {
            100usize
        } else {
            let max_offset = total_lines.saturating_sub(viewport_height);
            (self.view.scroll_offset * 100 / max_offset).min(100)
        };

        render_status_bar(
            f,
            chunks[0],
            &self.page.title,
            self.page.url.as_str(),
            scroll_pct,
        );

        // 検索マッチのハイライト描画
        let current_match = self.search_matches.get(self.search_cursor).cloned();
        let match_set: std::collections::HashSet<usize> =
            self.search_matches.iter().cloned().collect();

        let visible_lines: Vec<Line<'static>> = self
            .view
            .lines
            .iter()
            .enumerate()
            .skip(self.view.scroll_offset)
            .take(viewport_height)
            .map(|(i, line)| {
                if Some(i) == current_match {
                    apply_line_bg(line, Color::Indexed(58)) // 暗めの黄緑（現在マッチ）
                } else if match_set.contains(&i) {
                    apply_line_bg(line, Color::Indexed(237)) // 暗めのグレー（他マッチ）
                } else {
                    line.clone()
                }
            })
            .collect();

        let content_widget = Paragraph::new(visible_lines).style(Style::default());
        f.render_widget(content_widget, chunks[1]);

        let selected_link_url = self
            .view
            .selected_link_url(&self.page.links)
            .map(|u| u.as_str());

        let bar_width = chunks[2].width as usize;
        match &self.mode {
            BrowserMode::Normal if self.loading.is_some() => {
                let msg = self.loading.as_deref().unwrap_or_default();
                let text = format!(" ⟳ {}  (Esc: cancel) ", msg);
                let bar = Paragraph::new(Line::from(Span::styled(
                    truncate_to_width(&text, bar_width),
                    Style::default().fg(Color::Yellow),
                )))
                .style(Style::default().bg(Color::DarkGray));
                f.render_widget(bar, chunks[2]);
            }
            BrowserMode::Normal if self.notice.is_some() => {
                let notice = self.notice.as_deref().unwrap_or_default();
                let bar = Paragraph::new(Line::from(Span::styled(
                    truncate_to_width(&format!(" ✗ {} ", notice), bar_width),
                    Style::default().fg(Color::White).bg(Color::Red),
                )));
                f.render_widget(bar, chunks[2]);
            }
            BrowserMode::Normal | BrowserMode::Help => {
                render_hint_bar(
                    f,
                    chunks[2],
                    self.view.link_positions.len(),
                    self.view.selected_link,
                    selected_link_url,
                    !self.history.is_empty(),
                    !self.forward_history.is_empty(),
                );
                if matches!(self.mode, BrowserMode::Help) {
                    render_help_popup(f);
                }
            }
            BrowserMode::SearchInput(q) => {
                render_input_bar(f, chunks[2], "/", q);
            }
            BrowserMode::WebSearch(q) => {
                render_input_bar(f, chunks[2], "DDG: ", q);
            }
            BrowserMode::UrlInput(s) => {
                render_input_bar(f, chunks[2], "URL: ", s);
            }
        }
    }
}

/// 通知やエラーをヒントバー 1 行に収まる形にする（制御文字除去・改行の置換）
fn single_line(message: impl std::fmt::Display) -> String {
    stillo_core::sanitize_for_tui(&message.to_string()).replace('\n', " ")
}

fn is_ctrl_c(code: KeyCode, modifiers: KeyModifiers) -> bool {
    code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL)
}

/// 文字として入力欄に入れてよい修飾キーの組み合わせか。
/// Ctrl / Alt 付きはショートカットとして扱う。ただし Windows の AltGr は Ctrl+Alt として届き、
/// 一部のキーボード配列では '@' などの入力に必要なため、両方が付いている場合は文字入力とみなす。
fn is_text_input(modifiers: KeyModifiers) -> bool {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    ctrl == alt
}

/// 読み込み中のキャンセル操作か（Esc / Ctrl-C の押下）
fn is_cancel_event(ev: &Event) -> bool {
    match ev {
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            key.code == KeyCode::Esc || is_ctrl_c(key.code, key.modifiers)
        }
        _ => false,
    }
}

fn apply_line_bg(line: &Line<'static>, bg: Color) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .iter()
        .map(|s| Span::styled(s.content.clone(), s.style.bg(bg)))
        .collect();
    Line::from(spans)
}

fn render_help_popup(f: &mut Frame) {
    let area = f.area();
    let w = 52u16.min(area.width);
    let h = 26u16.min(area.height);
    let popup = Rect::new(
        (area.width.saturating_sub(w)) / 2,
        (area.height.saturating_sub(h)) / 2,
        w,
        h,
    );

    let bold_yellow = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let key = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    let rows: Vec<(&str, &str)> = vec![
        ("", ""),
        ("  Scroll", ""),
        ("  j / ↓", "  scroll down"),
        ("  k / ↑", "  scroll up"),
        ("  Ctrl+d / Ctrl+u", "  half page"),
        ("  PgDn / PgUp", "  full page"),
        ("  g / G", "  top / bottom"),
        ("", ""),
        ("  Links", ""),
        ("  Tab / Shift+Tab", "  next / prev link"),
        ("  Enter", "  follow link"),
        ("  B / ←", "  back"),
        ("  F / →", "  forward"),
        ("", ""),
        ("  Other", ""),
        ("  U", "  open URL (prefilled)"),
        ("  s", "  DuckDuckGo search"),
        ("  /", "  search in page"),
        ("  n / N", "  next / prev match"),
        ("  r", "  reload"),
        ("  d", "  dump Markdown"),
        ("  ?", "  this help"),
        ("  q / Esc / Ctrl+c", "  quit"),
        ("", ""),
        ("  Press any key to close", ""),
    ];

    let lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|(k, v)| {
            if v.is_empty() && !k.is_empty() {
                Line::from(Span::styled(k.to_owned(), bold_yellow))
            } else if v.is_empty() {
                Line::from("")
            } else {
                Line::from(vec![
                    Span::styled(k.to_owned(), key),
                    Span::styled(v.to_owned(), dim),
                ])
            }
        })
        .collect();

    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Help — stillo ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventState};
    use ratatui::backend::TestBackend;
    use stillo_core::{document::ExtractedLink, Block, Document, Inline};

    fn page_with(s: &str) -> BrowsePage {
        page_with_blocks(
            vec![Block::Paragraph(vec![Inline::Text(s.to_owned())])],
            vec![],
        )
    }

    fn page_with_blocks(blocks: Vec<Block>, links: Vec<ExtractedLink>) -> BrowsePage {
        BrowsePage {
            title: "title".to_owned(),
            url: Url::parse("https://example.com/").unwrap(),
            doc: Document { blocks },
            links,
            markdown: String::new(),
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::NONE,
        })
    }

    fn press(
        browser: &mut TuiBrowser,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<TuiResult> {
        browser.handle_event(key(code, modifiers, KeyEventKind::Press), 10)
    }

    /// TestBackend の各行を文字列として取り出す。全角文字が占める 2 セル目は除く
    fn buffer_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < buf.area.width {
                    let sym = buf[(x, y)].symbol();
                    row.push_str(sym);
                    x += crate::text::display_width(sym).max(1) as u16;
                }
                row
            })
            .collect()
    }

    /// new / load_page / reload_page のどの経路でも制御文字が除去されること
    #[test]
    fn all_page_entry_points_strip_control_chars() {
        let evil = "x\x1b]52;c;SGVsbG8=\x07y";
        let mut p = page_with(evil);
        p.title = evil.to_owned();
        p.markdown = evil.to_owned();
        let mut browser = TuiBrowser::new(p.clone());
        assert!(!browser.markdown().contains('\x1b'));
        browser.load_page(p.clone());
        assert!(!browser.markdown().contains('\x1b'));
        assert!(!browser.page.title.contains('\x1b'));
        browser.reload_page(p);
        assert!(!browser.markdown().contains('\x1b'));
        assert!(!format!("{:?}", browser.page.doc).contains("\\u{1b}"));
    }

    #[test]
    fn error_notice_is_sanitized_and_cleared_by_next_key() {
        let mut browser = TuiBrowser::new(page_with("ok"));
        browser.show_error("fetch failed:\x1b[2J\nline2");
        assert_eq!(browser.notice.as_deref(), Some("fetch failed:[2J line2"));
        browser.handle_key(KeyCode::Char('j'), KeyModifiers::NONE, 10);
        assert!(browser.notice.is_none());
    }

    /// 端末幅 30 列で日本語段落を描画しても本文が欠落しないこと
    #[test]
    fn renders_japanese_paragraph_within_terminal_width() {
        let text = "日本語の段落は空白を含まないため語単位では折り返せない。".repeat(3);
        let mut browser = TuiBrowser::new(page_with(&text));
        let mut terminal = Terminal::new(TestBackend::new(30, 20)).unwrap();
        browser.draw(&mut terminal).unwrap();
        assert_eq!(browser.view.width(), 30);
        let rows = buffer_rows(&terminal);
        // 1 行目はステータスバー、最終行はヒントバー
        let body: String = rows[1..rows.len() - 1]
            .iter()
            .map(|r| r.trim_end())
            .collect();
        assert_eq!(body.trim(), text);
    }

    /// リサイズで再レイアウトしてもスクロール位置・選択リンク・検索マッチが整合すること
    #[test]
    fn resize_relayouts_and_keeps_state() {
        let mut blocks: Vec<Block> = (0..30)
            .map(|i| {
                Block::Paragraph(vec![Inline::Text(format!(
                    "paragraph {i} needle text that wraps on a narrow terminal"
                ))])
            })
            .collect();
        blocks.push(Block::Paragraph(vec![Inline::Link {
            text: "link".into(),
            href: "https://example.com/next".into(),
        }]));
        let links = vec![ExtractedLink {
            text: "link".into(),
            href: Url::parse("https://example.com/next").unwrap(),
            rel: None,
        }];
        let mut browser = TuiBrowser::new(page_with_blocks(blocks, links));
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        browser.draw(&mut terminal).unwrap();

        browser.view.next_link(10);
        browser.mode = BrowserMode::SearchInput("needle".into());
        press(&mut browser, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut browser, KeyCode::Char('n'), KeyModifiers::NONE);
        press(&mut browser, KeyCode::Char('n'), KeyModifiers::NONE);
        let match_line = browser.search_matches[browser.search_cursor];
        let match_block = browser.view.block_of_line(match_line);
        let selected = browser.view.selected_link;

        terminal.backend_mut().resize(24, 12);
        browser.draw(&mut terminal).unwrap();
        assert_eq!(browser.view.width(), 24);
        // 再レイアウト後の全行が新しい幅に収まる（右端が切れない）
        assert!(browser.view.lines.iter().all(|l| l.width() <= 24));
        assert_eq!(browser.view.selected_link, selected);
        let new_line = browser.search_matches[browser.search_cursor];
        assert_eq!(browser.view.block_of_line(new_line), match_block);
        assert!(browser.search_matches.iter().all(|&l| browser.view.lines[l]
            .spans
            .iter()
            .any(|s| s.content.contains("needle"))));
        // 現在マッチの行に移動すると、スクロール位置もそのブロックを指す
        press(&mut browser, KeyCode::Char('n'), KeyModifiers::NONE);
        press(&mut browser, KeyCode::Char('N'), KeyModifiers::NONE);
        assert_eq!(
            browser.view.block_of_line(browser.view.scroll_offset),
            match_block
        );
    }

    #[test]
    fn ctrl_c_quits_in_normal_mode() {
        let mut browser = TuiBrowser::new(page_with("ok"));
        let r = press(&mut browser, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(r, Some(TuiResult::Quit)));
    }

    #[test]
    fn ctrl_c_cancels_input_mode_without_typing() {
        let mut browser = TuiBrowser::new(page_with("ok"));
        press(&mut browser, KeyCode::Char('U'), KeyModifiers::SHIFT);
        assert!(matches!(browser.mode, BrowserMode::UrlInput(_)));
        let r = press(&mut browser, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(r.is_none());
        assert!(matches!(browser.mode, BrowserMode::Normal));
    }

    #[test]
    fn modified_keys_are_not_typed_into_input() {
        let mut browser = TuiBrowser::new(page_with("ok"));
        press(&mut browser, KeyCode::Char('s'), KeyModifiers::NONE);
        press(&mut browser, KeyCode::Char('a'), KeyModifiers::NONE);
        press(&mut browser, KeyCode::Char('B'), KeyModifiers::SHIFT);
        press(&mut browser, KeyCode::Char('x'), KeyModifiers::ALT);
        press(&mut browser, KeyCode::Char('w'), KeyModifiers::CONTROL);
        // AltGr（Ctrl+Alt）経由の記号は入力として扱う
        press(
            &mut browser,
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert!(matches!(&browser.mode, BrowserMode::WebSearch(q) if q == "aB@"));
        press(&mut browser, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(matches!(&browser.mode, BrowserMode::WebSearch(q) if q.is_empty()));
    }

    /// Windows の Release イベントで二重入力にならないこと
    #[test]
    fn key_release_events_are_ignored() {
        let mut browser = TuiBrowser::new(page_with("ok"));
        press(&mut browser, KeyCode::Char('/'), KeyModifiers::NONE);
        for kind in [KeyEventKind::Press, KeyEventKind::Release] {
            browser.handle_event(key(KeyCode::Char('a'), KeyModifiers::NONE, kind), 10);
        }
        assert!(matches!(&browser.mode, BrowserMode::SearchInput(q) if q == "a"));
        let r = browser.handle_event(
            key(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
            10,
        );
        assert!(r.is_none());
    }

    #[test]
    fn cancel_event_detection() {
        assert!(is_cancel_event(&key(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Press
        )));
        assert!(is_cancel_event(&key(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press
        )));
        assert!(!is_cancel_event(&key(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Release
        )));
        assert!(!is_cancel_event(&key(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
            KeyEventKind::Press
        )));
    }

    /// 読み込み中表示が TUI 上のヒントバーに描画され、ページ表示後に消えること
    #[test]
    fn loading_message_is_drawn_in_hint_bar() {
        let mut browser = TuiBrowser::new(page_with("ok"));
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        browser.set_loading("Loading https://example.org/...");
        browser.draw(&mut terminal).unwrap();
        let rows = buffer_rows(&terminal);
        assert!(
            rows[5].contains("Loading https://example.org/..."),
            "{rows:?}"
        );
        assert!(rows[2].contains("ok"));
        browser.load_page(page_with("next"));
        browser.draw(&mut terminal).unwrap();
        assert!(!buffer_rows(&terminal)[5].contains("Loading"));
    }

    /// 復元処理は 1 回だけ実行され、alternate screen の解除を必ず出力すること
    #[test]
    fn restore_terminal_is_idempotent_and_leaves_alternate_screen() {
        TUI_ACTIVE.store(true, Ordering::SeqCst);
        let mut out = Vec::new();
        restore_terminal_to(&mut out);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("\x1b[?1049l"), "{s:?}"); // LeaveAlternateScreen
        assert!(s.contains("\x1b[?25h"), "{s:?}"); // カーソル表示
        let mut again = Vec::new();
        restore_terminal_to(&mut again);
        assert!(again.is_empty());
    }
}
