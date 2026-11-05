//! LLM の JSON 出力の正規化。CLI (`extract --format json`) と MCP (`extract_structured`) で
//! 同じ検証を通すために 1 箇所にまとめる。

use crate::client::LlmError;

/// ```json ... ``` のフェンスを外す。外側の 1 組だけを取り除く。
/// "Return JSON only" と指示しても多くのモデルがフェンスで囲んで返すため。
pub fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let Some(inner) = rest.strip_suffix("```") else {
        return t;
    };
    // 開始フェンスの言語指定（json 等）は最初の改行までなので読み飛ばす
    match inner.split_once('\n') {
        Some((_lang, body)) => body.trim(),
        None => inner.trim(),
    }
}

/// LLM の出力を JSON として検証し、整形した文字列を返す。
/// パースできない出力を成功として返すと、呼び出し側が壊れたデータを正しい抽出結果と誤認するためエラーにする。
/// エラー文には生出力の先頭を含める（原因調査用）が、端末へ流れる経路があるため制御文字は除去する。
pub fn normalize_json_output(output: &str) -> Result<String, LlmError> {
    let body = strip_code_fence(output);
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => Ok(serde_json::to_string_pretty(&value)
            .unwrap_or_else(|_| serde_json::to_string(&value).unwrap_or_else(|_| body.to_owned()))),
        Err(e) => {
            let cleaned = stillo_core::strip_control_chars(output);
            let preview: String = cleaned.chars().take(500).collect();
            Err(LlmError::Parse(format!(
                "LLM output is not valid JSON ({}). Raw output ({} chars):\n{}",
                e,
                output.chars().count(),
                preview
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_json_is_formatted() {
        let out = normalize_json_output(r#"{"title":"x"}"#).unwrap();
        assert_eq!(out, "{\n  \"title\": \"x\"\n}");
    }

    #[test]
    fn fenced_json_is_accepted() {
        let out = normalize_json_output("```json\n{\"a\": 1}\n```").unwrap();
        assert_eq!(out, "{\n  \"a\": 1\n}");
    }

    #[test]
    fn prose_is_rejected_with_raw_output() {
        let err = normalize_json_output("Here is the data: nope").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "{err}");
        assert!(err.to_string().contains("Here is the data"), "{err}");
    }

    /// エラー文は端末に流れるため、生出力の制御文字は除去してから載せる。
    #[test]
    fn error_preview_strips_control_characters() {
        let err = normalize_json_output("bad\u{1b}[31mred\u{07}")
            .unwrap_err()
            .to_string();
        assert!(!err.contains('\u{1b}'), "{err:?}");
        assert!(!err.contains('\u{07}'), "{err:?}");
        assert!(err.contains("red"), "{err}");
    }

    #[test]
    fn fence_without_closing_is_treated_as_text() {
        let err = normalize_json_output("```json\n{\"a\":1}").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "{err}");
    }
}
