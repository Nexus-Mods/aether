//! Root-scoped addressing. Every path on the wire is a `(root, relative
//! path)` pair, so one director can serve several roots and still tell
//! `[1, "foo/bar"]` from `[0, "foo/bar"]`.

/// Identifies one virtualized filesystem location within a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RootId(pub u32);

impl RootId {
    /// The root every single-root session uses.
    pub const DEFAULT: RootId = RootId(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_root_is_root_zero() {
        assert_eq!(RootId::DEFAULT, RootId(0));
    }
}
