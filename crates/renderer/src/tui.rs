use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame, Terminal,
};
use stillo_core::document::BrowsePage;
use url::Url;

use crate::widgets::{
    content_view::ContentView,
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

enum BrowserMode {
    Normal,
    SearchInput(String),   // ページ内テキスト検索（/）
    WebSearch(String),     // DuckDuckGo 検索（s）
    UrlInput(String),      // URL直接入力（U）
    Help,
}

pub struct TuiBrowser {
    page: BrowsePage,
    view: ContentView,
    mode: BrowserMode,
    search_matches: Vec<usize>,
    search_cursor: usize,
    history: Vec<(BrowsePage, usize)>,
    forward_history: Vec<(BrowsePage, usize)>,
}

impl TuiBrowser {
    pub fn new(page: BrowsePage) -> Self {
        let view = ContentView::from_document(&page.doc, &page.links);
        Self {
            page,
            view,
            mode: BrowserMode::Normal,
            search_matches: Vec::new(),
            search_cursor: 0,
            history: Vec::new(),
            forward_history: Vec::new(),
        }
    }

    /// 現在ページを履歴に積んで新ページへ遷移する。CLIのナビゲーションループから呼ぶ。
    pub fn load_page(&mut self, page: BrowsePage) {
        let offset = self.view.scroll_offset;
        let old_page = std::mem::replace(&mut self.page, page);
        self.history.push((old_page, offset));
        self.forward_history.clear();
        self.view = ContentView::from_document(&self.page.doc, &self.page.links);
        self.mode = BrowserMode::Normal;
        self.search_matches.clear();
        self.search_cursor = 0;
    }

    /// 現在ページを履歴に積まずに置き換える（リロード用）。
    pub fn reload_page(&mut self, page: BrowsePage) {
        self.view = ContentView::from_document(&page.doc, &page.links);
        self.page = page;
        self.mode = BrowserMode::Normal;
        self.search_matches.clear();
        self.search_cursor = 0;
    }

    pub fn current_url(&self) -> &Url {
        &self.page.url
    }

    pub fn markdown(&self) -> &str {
        &self.page.markdown
    }

    pub fn run(&mut self) -> Result<TuiResult> {
        terminal::enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let result = self.event_loop(&mut terminal);

        terminal::disable_raw_mode()?;
        execute!(terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        result
    }

    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<TuiResult> {
        loop {
            let viewport_height = terminal.size()?.height.saturating_sub(2) as usize;
            terminal.draw(|f| self.render(f))?;

            match event::read()? {
                Event::Key(key) => {
                    if let Some(result) = self.handle_key(key.code, key.modifiers, viewport_height)
                    {
                        return Ok(result);
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollDown => self.view.scroll_down(3, viewport_height),
                    MouseEventKind::ScrollUp => self.view.scroll_up(3),
                    _ => {}
                },
                _ => {}
            }
        }
    }

    fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        viewport_height: usize,
    ) -> Option<TuiResult> {
        match &self.mode {
            BrowserMode::Normal => self.handle_normal(code, modifiers, viewport_height),
            BrowserMode::SearchInput(_) => self.handle_search_input(code),
            BrowserMode::WebSearch(_) => self.handle_web_search(code),
            BrowserMode::UrlInput(_) => self.handle_url_input(code),
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
            KeyCode::Tab => self.view.next_link(),
            KeyCode::BackTab => self.view.prev_link(),

            // リンクフォロー
            KeyCode::Enter => {
                if let Some(url) = self.view.selected_link_url(&self.page.links) {
                    return Some(TuiResult::Navigate(url.clone()));
                }
            }

            // 戻る
            KeyCode::Char('B') | KeyCode::Left => {
                if let Some((prev_page, prev_offset)) = self.history.pop() {
                    let offset = self.view.scroll_offset;
                    let cur = std::mem::replace(&mut self.page, prev_page);
                    self.forward_history.push((cur, offset));
                    let mut v = ContentView::from_document(&self.page.doc, &self.page.links);
                    v.scroll_offset = prev_offset;
                    self.view = v;
                    self.search_matches.clear();
                    self.search_cursor = 0;
                }
            }

            // 進む
            KeyCode::Char('F') | KeyCode::Right => {
                if let Some((next_page, next_offset)) = self.forward_history.pop() {
                    let offset = self.view.scroll_offset;
                    let cur = std::mem::replace(&mut self.page, next_page);
                    self.history.push((cur, offset));
                    let mut v = ContentView::from_document(&self.page.doc, &self.page.links);
                    v.scroll_offset = next_offset;
                    self.view = v;
                    self.search_matches.clear();
                    self.search_cursor = 0;
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

    fn handle_search_input(&mut self, code: KeyCode) -> Option<TuiResult> {
        match code {
            KeyCode::Esc => {
                self.mode = BrowserMode::Normal;
            }
            KeyCode::Enter => {
                let query = match &self.mode {
                    BrowserMode::SearchInput(q) => q.clone(),
                    _ => unreachable!(),
                };
                self.search_matches = self.view.search(&query);
                self.search_cursor = 0;
                if let Some(&line_idx) = self.search_matches.first() {
                    self.view.scroll_offset = line_idx;
                }
                self.mode = BrowserMode::Normal;
            }
            KeyCode::Backspace => {
                if let BrowserMode::SearchInput(ref mut q) = self.mode {
                    q.pop();
                }
            }
            KeyCode::Char(c) => {
                if let BrowserMode::SearchInput(ref mut q) = self.mode {
                    q.push(c);
                }
            }
            _ => {}
        }
        None
    }

    fn handle_web_search(&mut self, code: KeyCode) -> Option<TuiResult> {
        match code {
            KeyCode::Esc => {
                self.mode = BrowserMode::Normal;
            }
            KeyCode::Enter => {
                let query = match &self.mode {
                    BrowserMode::WebSearch(q) => q.clone(),
                    _ => unreachable!(),
                };
                self.mode = BrowserMode::Normal;
                if !query.trim().is_empty() {
                    // 検索HTTPは呼び出し側（cli）で実行し、結果ページを load_page してもらう
                    return Some(TuiResult::WebSearch(query.trim().to_owned()));
                }
            }
            KeyCode::Backspace => {
                if let BrowserMode::WebSearch(ref mut q) = self.mode {
                    q.pop();
                }
            }
            KeyCode::Char(c) => {
                if let BrowserMode::WebSearch(ref mut q) = self.mode {
                    q.push(c);
                }
            }
            _ => {}
        }
        None
    }

    fn handle_url_input(&mut self, code: KeyCode) -> Option<TuiResult> {
        match code {
            KeyCode::Esc => {
                self.mode = BrowserMode::Normal;
            }
            KeyCode::Enter => {
                let input = match &self.mode {
                    BrowserMode::UrlInput(s) => s.clone(),
                    _ => unreachable!(),
                };
                self.mode = BrowserMode::Normal;
                if let Ok(url) = input.parse::<Url>() {
                    return Some(TuiResult::Navigate(url));
                }
                // httpスキームを補完して再試行
                if let Ok(url) = format!("https://{}", input).parse::<Url>() {
                    return Some(TuiResult::Navigate(url));
                }
            }
            KeyCode::Backspace => {
                if let BrowserMode::UrlInput(ref mut s) = self.mode {
                    s.pop();
                }
            }
            KeyCode::Char(c) => {
                if let BrowserMode::UrlInput(ref mut s) = self.mode {
                    s.push(c);
                }
            }
            _ => {}
        }
        None
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

        render_status_bar(f, chunks[0], &self.page.title, self.page.url.as_str(), scroll_pct);

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

        match &self.mode {
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
        ("  q / Esc", "  quit"),
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
