//! Asking about a path seen before allocates nothing: whether it is under a
//! root (the test the shim makes for every file a program touches outside the
//! roots), and its vpath (how the shim routes every call under one).

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

#[test]
fn resolve_vpath_of_a_cached_path_allocates_nothing() {
    let map = RootMap::new(r"C:\Games\Skyrim", VolumeMap::empty()).unwrap();
    let inside = r"\??\C:\Games\Skyrim\Data\Textures\Armor\a.dds";
    let root = r"\??\C:\Games\Skyrim";
    let outside = r"\??\C:\Windows\System32\kernel32.dll";
    // The first question computes and caches each answer.
    let first = map.resolve_vpath(inside).expect("under the root");
    assert_eq!(&*first.1, "data/textures/armor/a.dds");
    assert_eq!(&*map.resolve_vpath(root).expect("the root").1, "");
    assert!(map.resolve_vpath(outside).is_none());

    let (answers, n) = aether_testkit::alloc::allocations(|| {
        (
            map.resolve_vpath(inside),
            map.resolve_vpath(root),
            map.resolve_vpath(outside),
        )
    });
    assert_eq!(n, 0, "allocations for three cached answers");
    let (inside_again, root_again, outside_again) = answers;
    assert!(std::sync::Arc::ptr_eq(&first.1, &inside_again.unwrap().1));
    assert_eq!(&*root_again.unwrap().1, "");
    assert!(outside_again.is_none());
    // The components are the joined vpath's.
    assert_eq!(
        map.resolve(inside).unwrap().1,
        ["data", "textures", "armor", "a.dds"]
    );
    assert!(map.resolve(root).unwrap().1.is_empty());
}
