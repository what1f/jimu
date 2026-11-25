//! HTTP tools and transport-independent web page extraction.

mod fetch_tool;
mod http;
mod policy;
pub mod web2md;

pub use fetch_tool::WebFetchTool;
pub use policy::{HostResolver, WebFetchConfig};
