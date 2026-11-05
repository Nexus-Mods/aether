#![forbid(unsafe_code)]

//! The injected shim's pure path core: which declared root, if any, an incoming NT path falls
//! under ([`RootMap`]), and its canonicalisation; vpath normalization, DOS wildcards, and the
//! final name a handle reports ([`finalname`]). Case folding is `aether-casefold`'s.

mod cache;
mod canon;
pub mod finalname;
mod nt;
mod rootmap;
#[cfg(test)]
mod tests;
mod volumes;
mod vpath;
mod whiteout;
mod wildcard;

pub use aether_protocol::RootId;

pub use cache::UncachedScope;
pub use canon::{canonicalise, split_stream_suffix, VolumeMap};
pub use nt::{counted_units, nt_to_volume_relative, to_nt, utf16_to_string, CountedErr};
pub use rootmap::RootMap;
pub use volumes::{
    expand_short_component, expand_short_name, resolve_volume_map, resolve_volume_map_for,
};
pub use vpath::{normalize_vpath, PathError};
pub use whiteout::{is_whiteout, WHITEOUT_SUFFIX};
pub use wildcard::wildcard_match;

/// What a [`RootMap`] lookup answers with: which declared root the path fell
/// under, and its folded remainder components beneath that root.
pub type RootHit = (RootId, Vec<String>);

/// [`RootHit`] with its components joined by `/`: the folded vpath under the
/// root (empty for the root itself), shared with [`RootMap`]'s cache, so a
/// path asked about before costs no allocation ([`RootMap::resolve_vpath`]).
pub type RootVpath = (RootId, std::sync::Arc<str>);
