//! ツール出力のページング。
//! 長いページや大量のリンクをそのまま返すと MCP クライアント側 LLM のコンテキストを溢れさせるため、
//! 1 回の応答量を制限し、続きを取得するための offset を出力に明示する。

use anyhow::{anyhow, Result};
use serde_json::Value;

/// 文字単位で切り出した結果。`offset`/`end`/`total` はすべて文字数（Unicode scalar value 単位）
#[derive(Debug, PartialEq, Eq)]
pub struct CharPage<'a> {
    pub text: &'a str,
    pub offset: usize,
    pub end: usize,
    pub total: usize,
}

impl CharPage<'_> {
    /// 後続が残っている場合の次の offset
    pub fn next_offset(&self) -> Option<usize> {
        (self.end < self.total).then_some(self.end)
    }
}

/// `s` の `offset` 文字目から最大 `max_chars` 文字を切り出す。
/// バイト位置で切るとマルチバイト文字の途中で panic するため、char_indices で文字境界を求める
pub fn slice_chars(s: &str, offset: usize, max_chars: usize) -> Result<CharPage<'_>> {
    let total = s.chars().count();
    if offset > total || (offset == total && total > 0) {
        return Err(anyhow!(
            "offset {} is out of range (total {} chars)",
            offset,
            total
        ));
    }
    let byte_at = |n: usize| s.char_indices().nth(n).map_or(s.len(), |(i, _)| i);
    let start = byte_at(offset);
    let end = offset.saturating_add(max_chars).min(total);
    let stop = start
        + s[start..]
            .char_indices()
            .nth(end - offset)
            .map_or(s.len() - start, |(i, _)| i);
    Ok(CharPage {
        text: &s[start..stop],
        offset,
        end,
        total,
    })
}

/// 非負整数の引数を読む。未指定・null は既定値。
/// 負数や小数を黙って既定値に置き換えると呼び出し側の誤りに気づけないためエラーにする
pub fn usize_arg(args: &Value, name: &str, default: usize) -> Result<usize> {
    match &args[name] {
        Value::Null => Ok(default),
        v => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| anyhow!("'{}' must be a non-negative integer", name)),
    }
}

/// 1 以上の整数の引数を読む。0 件・0 文字の取得は意味がなく、ページングが進まなくなるため拒否する
pub fn positive_usize_arg(args: &Value, name: &str, default: usize) -> Result<usize> {
    match usize_arg(args, name, default)? {
        0 => Err(anyhow!("'{}' must be at least 1", name)),
        n => Ok(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slices_on_char_boundaries() {
        let s = "あいうえおかきくけこ";
        let p = slice_chars(s, 2, 3).unwrap();
        assert_eq!(p.text, "うえお");
        assert_eq!((p.offset, p.end, p.total), (2, 5, 10));
        assert_eq!(p.next_offset(), Some(5));
    }

    #[test]
    fn last_page_has_no_next_offset() {
        let p = slice_chars("abcdé", 3, 100).unwrap();
        assert_eq!(p.text, "dé");
        assert_eq!(p.next_offset(), None);
    }

    #[test]
    fn empty_input_is_single_empty_page() {
        let p = slice_chars("", 0, 10).unwrap();
        assert_eq!(p.text, "");
        assert_eq!(p.next_offset(), None);
    }

    #[test]
    fn offset_past_end_is_error() {
        assert!(slice_chars("abc", 3, 1).is_err());
        assert!(slice_chars("abc", 4, 1).is_err());
    }

    #[test]
    fn huge_max_chars_does_not_overflow() {
        let p = slice_chars("abc", 1, usize::MAX).unwrap();
        assert_eq!(p.text, "bc");
    }

    #[test]
    fn integer_args_are_validated() {
        let a = json!({"n": 5, "neg": -1, "f": 1.5, "s": "3", "z": 0});
        assert_eq!(usize_arg(&a, "n", 9).unwrap(), 5);
        assert_eq!(usize_arg(&a, "missing", 9).unwrap(), 9);
        assert!(usize_arg(&a, "neg", 9).is_err());
        assert!(usize_arg(&a, "f", 9).is_err());
        assert!(usize_arg(&a, "s", 9).is_err());
        assert_eq!(usize_arg(&a, "z", 9).unwrap(), 0);
        assert!(positive_usize_arg(&a, "z", 9).is_err());
    }
}
