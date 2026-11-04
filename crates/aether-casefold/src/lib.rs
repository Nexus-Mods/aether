//! Case folding — single source of truth for case-insensitive comparison.
#![forbid(unsafe_code)]

/// Lowercase simple case fold. MVP uses `char::to_lowercase` (Unicode simple
/// folding). This is the single source of truth for case-insensitive matching.
///
/// It is load-bearing across the ring, not merely a convention. The shim folds
/// every vpath component with this function before the vpath is sent
/// (`aether-redirect`'s `RootMap::match_canonical`), so everything that keys,
/// compares, or orders a name on the other side — a director's archive indexes,
/// mount-prefix matching, directory merges, layered/overlay merges and glob
/// routes — has to fold with this same function. `to_ascii_lowercase` was used below the ring until the final
/// review of `feat/real-roots` found the split: `Data/ÜBER/a.esp` crossed as
/// `data/über/a.esp` and every index below was keyed `data/ÜBER/a.esp`, so the
/// file resolved to not-found. `DiskProvider` hid it, because Windows folds
/// Unicode itself.
///
/// If this function's definition ever changes, both sides move together; a
/// change here is a wire-visible change.
///
/// Two properties it does **not** have, both of which have already produced
/// bugs here:
///
/// 1. **Not length-preserving.** `İ` (U+0130) is two bytes and folds to three
///    (`i` + U+0307). Never slice a folded string by an offset measured on the
///    unfolded one — walk components instead. `strip_prefix` and
///    `mount_child_name` both did exactly that before the `feat/real-roots`
///    final review.
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

/// Whether `a` and `b` fold to the same string, `fold(a) == fold(b)`, without
/// building either fold.
pub fn eq_folded(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(b);
    }
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
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

    #[test]
    fn folds_ascii_and_unicode() {
        assert_eq!(fold("FooBAR.ESP"), "foobar.esp");
        assert_eq!(fold("ÄÖÜ"), "äöü");
    }

    #[test]
    fn eq_folded_is_equality_of_the_folds() {
        // Cases a property may not draw: characters outside the BMP, the
        // three sigmas, a fold that is a prefix of the other, a character
        // whose lowercase is two characters.
        let words = [
            "File.txt", "file.TXT", "File.tx", "", "ÄÖÜ", "äöü", "Äö", "ǅ", "ǆ", "İ", "i̇", "ẞ",
            "ß", "ss", "K", "K", "k", "𐐀", "𐐨", "𐐀𐐁", "𐐨𐐩", "Σ", "σ", "ς", "ΣΑΣ", "σας",
        ];
        for a in words {
            for b in words {
                assert_eq!(eq_folded(a, b), fold(a) == fold(b), "{a:?} {b:?}");
            }
        }
    }

    /// `s` with each character upper- or lowercased as `flips` says: another
    /// spelling of the same name, or, where a mapping is not reversible,
    /// another name.
    fn respell(s: &str, flips: &[bool]) -> String {
        s.chars()
            .zip(flips.iter().cycle())
            .flat_map(|(c, up)| {
                let out: Vec<char> = if *up {
                    c.to_uppercase().collect()
                } else {
                    c.to_lowercase().collect()
                };
                out
            })
            .collect()
    }

    proptest::proptest! {
        #[test]
        fn eq_folded_agrees_with_the_folds_for_any_two_strings(a in ".*", b in ".*") {
            proptest::prop_assert_eq!(eq_folded(&a, &b), fold(&a) == fold(&b));
        }

        #[test]
        fn eq_folded_agrees_with_the_folds_for_a_string_and_a_respelling(
            a in ".*",
            flips in proptest::collection::vec(proptest::bool::ANY, 1..8),
        ) {
            let b = respell(&a, &flips);
            proptest::prop_assert_eq!(eq_folded(&a, &b), fold(&a) == fold(&b));
            proptest::prop_assert_eq!(eq_folded(&b, &a), fold(&b) == fold(&a));
        }
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
