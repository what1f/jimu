// Generic extraction adapted from web2md-swift and Defuddle. See ../LICENSE-MIT.

mod constants;
mod content_patterns;
mod defuddle;
mod metadata;
mod removals;
pub(super) mod standardize;

pub(super) use defuddle::extract;
