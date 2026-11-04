//! A hit in the spelling cache allocates nothing.

use aether_redirect::finalname::NameCache;

#[global_allocator]
static A: aether_testkit::alloc::Counting = aether_testkit::alloc::Counting;

fn under(path: &str) -> Vec<String> {
    path.split('/').map(str::to_string).collect()
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn a_hit_allocates_nothing() {
    let mut c = NameCache::new(1_000, 64);
    let font = under("data/interface/fonts/a.ttf");
    c.remember(
        0,
        &font,
        0,
        &names(&["Data", "Interface", "Fonts", "A.ttf"]),
        0,
    );
    let dir = under("data/interface");

    let mut seen = 0usize;
    let (known, n) = aether_testkit::alloc::allocations(|| {
        c.leading_with(0, &font, 10, |name| seen += name.len())
    });
    assert_eq!((known, n), (4, 0), "a whole hit");
    assert_eq!(seen, "DataInterfaceFontsA.ttf".len());

    let (known, n) = aether_testkit::alloc::allocations(|| c.leading_with(0, &dir, 10, |_| ()));
    assert_eq!((known, n), (2, 0), "a directory above a remembered path");

    let miss = under("data/other/b.esp");
    let (known, n) = aether_testkit::alloc::allocations(|| c.leading_with(0, &miss, 10, |_| ()));
    assert_eq!((known, n), (1, 0), "a partial hit");

    let (spelled, n) = aether_testkit::alloc::allocations(|| {
        (0..font.len())
            .map(|i| c.spelling(0, &font, i).map_or(0, str::len))
            .sum::<usize>()
    });
    assert_eq!(
        (spelled, n),
        ("DataInterfaceFontsA.ttf".len(), 0),
        "each spelling, borrowed"
    );
}
