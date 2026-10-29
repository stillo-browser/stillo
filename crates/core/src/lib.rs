pub mod ast;
pub mod document;
pub mod extractor;
pub mod html_to_ast;
pub mod markdown;
pub mod markdown_to_ast;
pub mod rss_to_ast;
pub mod sanitize;
pub mod search;

pub use ast::{Block, Document, Inline};
pub use document::BrowsePage;
pub use document::*;
pub use extractor::{ContentExtractor, ExtractionError, ExtractorConfig};
pub use html_to_ast::parse_html_to_ast;
pub use markdown::{HeadingStyle, MarkdownConfig, MarkdownSerializer};
pub use markdown_to_ast::parse_markdown_to_ast;
pub use rss_to_ast::parse_rss_to_ast;
pub use sanitize::{is_navigable_url, sanitize_for_tui, strip_control_chars};
pub use search::{
    detect_blocked_page, parse_ddg_results, parse_ddg_results_checked, DdgParseError, SearchResult,
};
