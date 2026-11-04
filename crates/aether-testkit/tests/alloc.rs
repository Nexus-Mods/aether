//! The shared allocation counter counts what the armed thread allocates and
//! nothing another thread does meanwhile.

use std::sync::{Arc, Barrier};

#[global_allocator]
static A: aether_testkit::alloc::Counting = aether_testkit::alloc::Counting;

#[test]
fn allocations_counts_only_this_thread() {
    let (v, n) = aether_testkit::alloc::allocations(|| Vec::<u8>::with_capacity(8));
    assert_eq!((v.capacity(), n), (8, 1));
    assert_eq!(aether_testkit::alloc::allocations(|| 1 + 1).1, 0);

    // Another thread allocates while this one counts: none of it is counted.
    // A barrier waits without allocating, so the armed thread's own count
    // stays 0.
    let start = Arc::new(Barrier::new(2));
    let end = Arc::new(Barrier::new(2));
    let other = {
        let (start, end) = (start.clone(), end.clone());
        std::thread::spawn(move || {
            start.wait();
            let held: Vec<Vec<u8>> = (0..100).map(|_| vec![0u8; 16]).collect();
            drop(held);
            end.wait();
        })
    };
    let ((), n) = aether_testkit::alloc::allocations(|| {
        start.wait();
        end.wait();
    });
    other.join().unwrap();
    assert_eq!(n, 0, "the other thread's allocations are not this thread's");

    // Counting stops when `f` returns: the result is the caller's, and
    // dropping it is not counted.
    let (s, n) = aether_testkit::alloc::allocations(|| String::from("x"));
    assert_eq!(n, 1);
    drop(s);
    assert_eq!(aether_testkit::alloc::allocations(|| ()).1, 0);
}

#[test]
fn a_panic_inside_leaves_the_thread_uncounted_and_a_nested_call_counts_for_both() {
    use aether_testkit::alloc::allocations;
    let unwound = std::panic::catch_unwind(|| allocations(|| panic!("inside")));
    assert!(unwound.is_err());
    assert_eq!(allocations(|| ()).1, 0);
    // A panic inside an outer call: the outer one keeps counting its own.
    let ((), outer) = allocations(|| {
        let _ = std::panic::catch_unwind(|| allocations(|| panic!("inside")));
        drop(vec![0u8; 8]);
    });
    assert!(outer >= 1, "{outer}");

    let ((inner, a), outer) = allocations(|| {
        let a = vec![0u8; 8];
        (allocations(|| vec![0u8; 8]), a)
    });
    assert_eq!((inner.1, outer), (1, 2));
    drop((inner, a));
}
