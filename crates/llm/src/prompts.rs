use stillo_core::document::MarkdownDocument;
use crate::client::Message;

const MAX_CONTENT_CHARS: usize = 20_000;

pub fn summarize_prompt(doc: &MarkdownDocument) -> Vec<Message> {
    vec![
        Message::system(
            "You are a precise summarizer. Respond in the same language as the document. Be concise.",
        ),
        Message::user(format!(
            "以下のWebページを3〜5文で要約してください。\n\nURL: {}\n\n{}",
            doc.source_url,
            truncate(&doc.content, MAX_CONTENT_CHARS),
        )),
    ]
}

pub fn qa_prompt(question: &str, doc: &MarkdownDocument) -> Vec<Message> {
    vec![
        Message::system(
            "Answer questions about the provided web page content. Be direct and cite the relevant parts. Respond in the same language as the question.",
        ),
        Message::user(format!(
            "以下のWebページについて質問に答えてください。\n\n質問: {}\n\nURL: {}\n\n{}",
            question,
            doc.source_url,
            truncate(&doc.content, MAX_CONTENT_CHARS),
        )),
    ]
}

pub fn extract_prompt(fields: &str, doc: &MarkdownDocument) -> Vec<Message> {
    vec![
        Message::system(
            "Extract structured information from the web page. Return JSON only, no explanation.",
        ),
        Message::user(format!(
            "以下のフィールドをJSON形式で抽出してください: {}\n\nURL: {}\n\n{}",
            fields,
            doc.source_url,
            truncate(&doc.content, MAX_CONTENT_CHARS),
        )),
    ]
}

fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}
