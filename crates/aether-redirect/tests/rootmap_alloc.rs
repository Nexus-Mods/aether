//! Asking whether a path seen before is under a root allocates nothing: the
//! test the shim makes for every file a program touches outside the roots.

use aether_redirect::{RootMap, VolumeMap};

#[global_allocator]
static A: aether_testkit::alloc::Counting = aether_testkit::alloc::Counting;

#[test]
fn contains_of_a_cached_path_allocates_nothing() {
    let map = RootMap::new(r"C:\Games\Skyrim", VolumeMap::empty()).unwrap();
    let outside = r"\??\C:\Windows\System32\kernel32.dll";
    let inside = r"\??\C:\Games\Skyrim\Data\a.esp";
    // The first question computes and caches each answer.
    assert!(!map.contains(outside));
    assert!(map.contains(inside));

    let (answers, n) =
        aether_testkit::alloc::allocations(|| (map.contains(outside), map.contains(inside)));
    assert_eq!(answers, (false, true));
    assert_eq!(n, 0, "allocations for two cached answers");
}
