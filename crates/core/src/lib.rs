pub mod ast;
pub mod document;
pub mod extractor;
pub mod html_to_ast;
pub mod markdown;
pub mod rss_to_ast;
pub mod markdown_to_ast;
pub mod search;

pub use ast::{Document, Block, Inline};
pub use document::*;
pub use extractor::{ContentExtractor, ExtractorConfig, ExtractionError};
pub use html_to_ast::parse_html_to_ast;
pub use markdown::{MarkdownSerializer, MarkdownConfig, HeadingStyle};
pub use rss_to_ast::parse_rss_to_ast;
pub use markdown_to_ast::parse_markdown_to_ast;
pub use document::BrowsePage;
pub use search::{SearchResult, detect_blocked_page, parse_ddg_results};
