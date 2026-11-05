pub mod client;
pub mod json_output;
pub mod prompts;

pub use client::{CompletionConfig, LlmError, LlmProvider, Message};
pub use json_output::{normalize_json_output, strip_code_fence};
