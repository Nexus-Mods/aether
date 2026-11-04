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

/// [`fold`] into `out`, which is cleared first: the same string, in a buffer
/// the caller keeps, so a fold into a buffer that has grown allocates nothing.
pub fn fold_into(s: &str, out: &mut String) {
    out.clear();
    if s.is_ascii() {
        out.push_str(s);
        out.make_ascii_lowercase();
    } else {
        out.extend(s.chars().flat_map(char::to_lowercase));
    }
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
    fn fold_into_gives_the_fold() {
        let mut out = String::from("left over");
        for s in ["", "FooBAR.ESP", "ÄÖÜ", "İstanbul", r"data\SKSE\Plugins"] {
            fold_into(s, &mut out);
            assert_eq!(out, fold(s), "{s:?}");
        }
    }

    /// So a name already folded compares as itself: `eq_folded(a, fold(b))` is
    /// `fold(a) == fold(b)`.
    #[test]
    fn fold_is_idempotent_over_every_char() {
        for c in (0..=char::MAX as u32).filter_map(char::from_u32) {
            let once = fold(&c.to_string());
            assert_eq!(fold(&once), once, "{c:?} (U+{:04X})", c as u32);
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
