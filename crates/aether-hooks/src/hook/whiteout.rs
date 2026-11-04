//! The shim's whiteout markers in a director listing.

use std::collections::HashSet;

use aether_casefold::fold;
use aether_ntlayout::DirItem;
use aether_redirect::is_whiteout;

/// Remove the shim's whiteout markers from a directory listing, and with each one
/// the name it hides.
///
/// A shim spells a whiteout `<name>.__vfs_wh__` ([`aether_redirect::WHITEOUT_SUFFIX`]);
/// a director's overlay spells it `.wh.<name>`. A write layer may hold shim-spelled markers,
/// and the director has no reason to hide a spelling it does not use, so `client.readdir`
/// hands them back as ordinary zero-byte files. Passed straight to the game, a marker would be
/// a phantom `<file>.__vfs_wh__` entry *and* would leave the file it hides still listed. Both
/// halves, from one marker.
///
/// The shim writes no markers itself, but a write layer may hold them, so
/// the listing honours them.
///
/// ## Why the shim never spelled whiteouts `.wh.<name>`
///
/// A director's overlay may answer whether a name is whited out from a
/// **cached per-directory index**, on the grounds that it is the only writer
/// of `.wh.` markers in its own upper. A second writer of `.wh.` into that
/// upper would leave a listing and an open disagreeing about whether a file
/// exists.
///
/// ## Ordering constraint at the call site
///
/// Callers must apply this **before** any wildcard filter. `*.esp` does not
/// match `gone.esp.__vfs_wh__`, so filtering first drops the marker on its own
/// and leaves `gone.esp` listed — the hiding half silently lost for exactly
/// the queries a game makes most.
///
/// Pure and in-memory: the marker names are already in the listing, so nothing
/// here touches the filesystem, and enumeration pays one pass over entries it
/// was going to copy anyway.
pub(crate) fn strip_whiteout_markers(items: Vec<DirItem>) -> Vec<DirItem> {
    // Two passes, because a marker may sort after the name it hides. A set
    // rather than a scan of a `Vec`: a mod that removes a few hundred files
    // from one directory is an ordinary thing to do, and that is the case
    // where the quadratic version would show up.
    let hidden: HashSet<String> = items
        .iter()
        .filter_map(|i| is_whiteout(&i.name).map(fold))
        .collect();
    if hidden.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|i| !hidden.contains(&fold(&i.name)) && is_whiteout(&i.name).is_none())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str) -> DirItem {
        DirItem {
            name: name.into(),
            is_dir: false,
            size: 0,
            mtime: 0,
        }
    }

    /// What the director hands back for a directory whose write layer holds a marker an older
    /// shim wrote: the deleted file (still in the read layers) *and* the marker. Both go, in
    /// either order and in any letter case, and unrelated entries stay.
    #[test]
    fn a_marker_and_the_name_it_hides_are_dropped_and_nothing_else_is() {
        let marker = format!("gone.esp{}", aether_redirect::WHITEOUT_SUFFIX);
        for listing in [
            vec![item("GONE.esp"), item(&marker), item("kept.esp")],
            vec![item(&marker), item("kept.esp"), item("gone.esp")],
        ] {
            let out = strip_whiteout_markers(listing);
            let names: Vec<&str> = out.iter().map(|i| i.name.as_str()).collect();
            assert_eq!(names, ["kept.esp"], "{names:?}");
        }
        let untouched = strip_whiteout_markers(vec![item("a.esp"), item("b.esp")]);
        assert_eq!(
            untouched.len(),
            2,
            "a listing with no marker is returned as it is"
        );
    }
}
