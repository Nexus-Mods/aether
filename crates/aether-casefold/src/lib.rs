//! Case folding — single source of truth for case-insensitive comparison.
#![forbid(unsafe_code)]

/// The Unicode version the fold's mapping comes from: the standard library's
/// of the pinned toolchain. Part of the wire (`docs/wire.md` 3.2).
pub const UNICODE_VERSION: (u8, u8, u8) = (17, 0, 0);

/// The case fold: every character replaced by its full Unicode lowercase
/// mapping (`char::to_lowercase`, which includes the unconditional
/// multi-character mappings of `SpecialCasing.txt`), one character at a time,
/// from Unicode 17.0.0, the version of the pinned toolchain's standard library
/// ([`UNICODE_VERSION`]). This is the single source of truth for
/// case-insensitive matching.
///
/// It is load-bearing across the ring, not merely a convention. The shim folds
/// every vpath component with this function before the vpath is sent
/// (`aether-redirect`'s `RootMap::match_canonical`), so everything that keys,
/// compares, or orders a name on the other side (a director's archive indexes,
/// mount-prefix matching, directory merges, layered/overlay merges and glob
/// routes) has to fold with this same function. An ASCII-only lowercase below
/// the ring would key `Data/ÜBER/a.esp` apart from the `data/über/a.esp` the
/// shim sends, and the file would resolve to not-found.
///
/// If this function's definition ever changes, both sides move together; a
/// change here is a wire-visible change (`docs/wire.md` 3.2).
///
/// Two properties it does **not** have:
///
/// 1. **Not length-preserving.** `İ` (U+0130) is two bytes and folds to three
///    (`i` + U+0307). Never slice a folded string by an offset measured on the
///    unfolded one; walk components instead.
/// 2. **Not NTFS-case-equivalent.** That same `İ` folds to a genuinely
///    different name, not a case variant of the input. So "NTFS is
///    case-insensitive, therefore the folded spelling names the same file" is
///    **not** a sound argument, and must not be used to wave through a change
///    to any spelling that reaches the filesystem.
///
/// **The ASCII path is the same function, faster.** `char::to_lowercase` of an
/// ASCII character is its ASCII lowercase, so an all-ASCII string — nearly
/// every path a game asks for — folds with one pass over its bytes instead of
/// a decode and a Unicode table lookup per character. The output is identical
/// (`ascii_fast_path_matches_the_unicode_fold` holds it to that), so this is
/// not the wire-visible kind of change the paragraph above warns about.
pub fn fold(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    fold_unicode(s)
}

/// [`fold`]'s definition: `char::to_lowercase` of every character.
fn fold_unicode(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// Case-insensitive comparison. Fold-equal strings compare `Equal`; callers that
/// need stable output rely on a stable sort. (Directory siblings are keyed by
/// folded name, so a case-only collision can never occur among them.)
pub fn cmp_ci(a: &str, b: &str) -> std::cmp::Ordering {
    fold(a).cmp(&fold(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    /// The fold is the standard library's lowercase mapping, so its Unicode
    /// version is the toolchain's. A toolchain with another one changes the
    /// wire: this fails until `UNICODE_VERSION` and `docs/wire.md` 3.2 say so.
    #[test]
    fn the_fold_is_unicode_17() {
        assert_eq!(char::UNICODE_VERSION, UNICODE_VERSION);
    }

    #[test]
    fn folds_ascii_and_unicode() {
        assert_eq!(fold("FooBAR.ESP"), "foobar.esp");
        assert_eq!(fold("ÄÖÜ"), "äöü");
    }

    #[test]
    fn ascii_fast_path_matches_the_unicode_fold() {
        // Every ASCII character, alone and inside a string, folds to what the
        // definition gives.
        let all: String = (0u8..128).map(char::from).collect();
        assert_eq!(fold(&all), fold_unicode(&all));
        for c in (0u8..128).map(char::from) {
            let one = c.to_string();
            assert_eq!(fold(&one), fold_unicode(&one), "{c:?}");
        }
        for s in [
            "",
            "Data/Meshes/Actors/Character/FaceGenData/FaceGeom/Skyrim.esm/0001A696.NIF",
            "Plugin Number 00042.ESP",
            "data\\SKSE\\Plugins\\Version-1-5-97-0.BIN",
        ] {
            assert!(s.is_ascii());
            assert_eq!(fold(s), fold_unicode(s), "{s:?}");
        }
        // One non-ASCII character anywhere takes the definition's path.
        for s in ["Data/ÜBER/a.ESP", "İstanbul", "ASCII then Ä", "ǅ"] {
            assert_eq!(fold(s), fold_unicode(s), "{s:?}");
        }
        assert_eq!(fold("İ"), "i\u{307}");
    }

    #[test]
    fn cmp_is_case_insensitive() {
        assert_eq!(cmp_ci("apple", "APPLE"), Ordering::Equal);
        assert_eq!(cmp_ci("Apple", "banana"), Ordering::Less);
        assert_eq!(cmp_ci("Banana", "apple"), Ordering::Greater);
    }

    #[test]
    fn cmp_ascending_not_reverse() {
        // Regression guard for the USVFS reverse-alphabetical bug.
        let mut v = vec!["Zebra", "apple", "Mango"];
        v.sort_by(|a, b| cmp_ci(a, b));
        assert_eq!(v, vec!["apple", "Mango", "Zebra"]);
    }

    #[test]
    fn cmp_fold_equal_is_equal() {
        // Names differing only by case fold-compare Equal. (They can never be
        // directory siblings, since children are keyed by folded name.)
        assert_eq!(cmp_ci("abc", "abc"), Ordering::Equal);
        assert_eq!(cmp_ci("ABC", "abc"), Ordering::Equal);
    }
}
