use anyhow::{anyhow, Result};
use serde_json::Value;
use stillo_core::{ContentExtractor, ExtractorConfig, MarkdownConfig, MarkdownSerializer};
use stillo_llm::{prompts, CompletionConfig, LlmError, LlmProvider};

pub async fn run(args: &Value) -> Result<String> {
    let url = super::parse_http_url(args)?;

    let fields = match &args["fields"] {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        Value::String(s) => s.clone(),
        _ => return Err(anyhow::anyhow!("'fields' must be an array or string")),
    };

    // プロバイダー未設定ならページ取得より前に失敗させ、無駄なネットワークアクセスを避ける
    let llm = LlmProvider::from_env()?;

    let fetcher = super::mcp_fetcher();
    let extractor = ContentExtractor::new(ExtractorConfig::default());
    let raw = fetcher.fetch(&url).await?;
    let content = extractor.extract(&raw)?;
    let serializer = MarkdownSerializer::new(MarkdownConfig::default());
    let doc = serializer.serialize(&content, chrono::Utc::now());

    let messages = prompts::extract_prompt(&fields, &doc);
    let result = match llm
        .complete(messages, &CompletionConfig::extraction())
        .await
    {
        Ok(r) => r,
        Err(e @ LlmError::Truncated { .. }) => {
            return Err(anyhow!(
                "{}: the JSON result was cut off; request fewer fields",
                e
            ))
        }
        Err(e) => return Err(e.into()),
    };

    format_extraction(&result)
}

/// LLM の抽出結果を JSON として検証し整形する。
/// パースできない出力を正常結果として返すと、呼び出し側が壊れた JSON を正しい抽出結果と誤認するためエラーにする
fn format_extraction(output: &str) -> Result<String> {
    let body = strip_code_fence(output);
    match serde_json::from_str::<Value>(body) {
        Ok(v) => Ok(serde_json::to_string_pretty(&v)?),
        Err(e) => Err(anyhow!(
            "LLM output is not valid JSON ({}). Raw output:\n{}",
            e,
            output
        )),
    }
}

/// "Return JSON only" と指示しても多くのモデルが ```json ... ``` で囲んで返すため、外側の 1 組だけ取り除く
fn strip_code_fence(s: &str) -> &str {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_json_is_pretty_printed() {
        let out = format_extraction(r#"{"title":"x"}"#).unwrap();
        assert_eq!(out, "{\n  \"title\": \"x\"\n}");
    }

    #[test]
    fn fenced_json_is_accepted() {
        let out = format_extraction("```json\n{\"a\": 1}\n```").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn non_json_output_is_error() {
        let err = format_extraction("Sure! The title is x.").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "{err}");
    }

    #[test]
    fn truncated_json_is_error() {
        assert!(format_extraction(r#"{"title": "x", "auth"#).is_err());
    }
}
