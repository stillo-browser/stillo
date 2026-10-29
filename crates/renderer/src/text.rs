//! 端末の表示幅（セル数）を基準にした文字列操作。
//!
//! 全角文字は 2 セル、結合文字は 0 セルを占めるため、`chars().count()` では
//! 端末上の幅と一致しない。折り返し・切り詰めはすべてここを経由する。

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// 文字列が端末上で占めるセル数を返す
pub fn display_width(s: &str) -> usize {
    s.width()
}

/// 表示幅が `max` を超える場合に末尾を "…" に置き換えて切り詰める。
/// 結合文字や絵文字の ZWJ 列を途中で切らないよう grapheme 単位で数える。
pub fn truncate_to_width(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max - 1; // "…" の 1 セル分
    let mut out = String::new();
    let mut used = 0;
    for g in s.graphemes(true) {
        let w = g.width();
        if used + w > budget {
            break;
        }
        out.push_str(g);
        used += w;
    }
    out.push('…');
    out
}

/// 表示幅が `max` を超える場合に先頭を "…" に置き換え、末尾側を残す。
/// 入力欄ではカーソル位置（末尾）が見えていることが重要なため。
pub fn tail_to_width(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max - 1;
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0;
    for g in s.graphemes(true).rev() {
        let w = g.width();
        if used + w > budget {
            break;
        }
        kept.push(g);
        used += w;
    }
    let mut out = String::from("…");
    out.extend(kept.into_iter().rev());
    out
}

/// 文字列を表示幅 `max` 以下の断片に分割する（コードブロックの折り返し用）。
/// 1 grapheme が `max` より広い場合も無限ループしないよう、各断片は最低 1 grapheme を含む。
pub fn split_to_width(s: &str, max: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0;
    for g in s.graphemes(true) {
        let w = g.width();
        if used + w > max && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push_str(g);
        used += w;
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_chars_count_as_two_cells() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本語"), 6);
    }

    #[test]
    fn truncate_respects_display_width() {
        assert_eq!(truncate_to_width("日本語テキスト", 7), "日本語…");
        assert_eq!(truncate_to_width("short", 10), "short");
        assert_eq!(truncate_to_width("abc", 0), "");
        assert!(display_width(&truncate_to_width("日本語テキスト", 6)) <= 6);
    }

    #[test]
    fn tail_keeps_end_of_input() {
        assert_eq!(tail_to_width("https://example.com/path", 8), "…om/path");
        assert_eq!(tail_to_width("日本語テキスト", 7), "…キスト");
    }

    #[test]
    fn split_keeps_every_grapheme() {
        let rows = split_to_width("あいうえおかきくけこ", 7);
        assert!(rows.iter().all(|r| display_width(r) <= 7));
        assert_eq!(rows.concat(), "あいうえおかきくけこ");
        // 幅より広い grapheme だけの行でも欠落しない
        assert_eq!(split_to_width("あい", 1), vec!["あ", "い"]);
        assert_eq!(split_to_width("", 5), vec![""]);
    }
}
