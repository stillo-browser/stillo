pub mod http;
mod net_guard;
pub mod search;
pub mod spa;

pub use http::{HttpConfig, HttpFetcher, DEFAULT_MAX_BODY_BYTES, USER_AGENT};
pub use search::{
    results_to_markdown, web_search, web_search_with_timeout, SearchBackend, SearchError,
};
pub use spa::SpaDelegationChain;
