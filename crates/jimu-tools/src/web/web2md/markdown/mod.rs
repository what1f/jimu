// Markdown conversion adapted from web2md-swift and Defuddle. See ../LICENSE-MIT.

mod math;
mod render;
mod syntax;

pub(super) use render::convert;
pub(super) use syntax::{destination, escape, escape_text, fence, image, inline_code, link, quote};
