//! Sections over director files whose bytes the director did not give: the
//! counter set that says why a mapped read failed or a process died of
//! `STATUS_IN_PAGE_ERROR` (0xC0000006).
//!
//! Ungated, as the child refusals are: zero in a healthy run, one relaxed
//! `fetch_add` when one happens, and a launch that died of an in-page error
//! needs the number although stats were off. Each kind has a label, which
//! [`section_failure_count`] and the report use:
//!
//! | Label | Counted when |
//! |---|---|
//! | `in-page-error` | a demand-paged fault became `STATUS_IN_PAGE_ERROR` (every try failed) |
//! | `eager-fill` | a data section filled at creation could not be filled whole, so it failed |
//! | `image-read` | an image section's read failed or came back short every try, so it failed |
//!
//! A failed demand-paged fill that a retry then filled is in the fills'
//! `failed` count ([`super::note_fill_end`]), not here.

use super::*;
use std::sync::atomic::AtomicUsize;

/// What could not be filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum SectionFailure {
    InPageError = 0,
    EagerFill = 1,
    ImageRead = 2,
}

/// Number of [`SectionFailure`]s.
pub(super) const SECTION_FAILURE_N: usize = 3;

impl SectionFailure {
    /// Every kind, in id order.
    const ALL: [SectionFailure; SECTION_FAILURE_N] = [
        SectionFailure::InPageError,
        SectionFailure::EagerFill,
        SectionFailure::ImageRead,
    ];

    /// The label the count and the report use.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            SectionFailure::InPageError => "in-page-error",
            SectionFailure::EagerFill => "eager-fill",
            SectionFailure::ImageRead => "image-read",
        }
    }
}

pub(super) static SECTION_FAILURES: [AtomicU64; SECTION_FAILURE_N] =
    [const { AtomicU64::new(0) }; SECTION_FAILURE_N];
/// The address of the last in-page error raised, 0 for none.
pub(super) static LAST_IN_PAGE_ADDR: AtomicUsize = AtomicUsize::new(0);

/// Record a section the shim could not fill.
pub(crate) fn note_section_failure(kind: SectionFailure) {
    SECTION_FAILURES[kind as usize].fetch_add(1, Ordering::Relaxed);
}

/// Record an in-page error raised for the fault at `addr`.
pub(crate) fn note_in_page_error(addr: usize) {
    note_section_failure(SectionFailure::InPageError);
    LAST_IN_PAGE_ADDR.store(addr, Ordering::Relaxed);
}

/// How many failures of the kind `label` names (see the module's table);
/// 0 for a label that names none.
pub fn section_failure_count(label: &str) -> u64 {
    SectionFailure::ALL
        .iter()
        .find(|k| k.label() == label)
        .map_or(0, |k| SECTION_FAILURES[*k as usize].load(Ordering::Relaxed))
}

/// The counts, by id.
pub(super) fn section_failures_snapshot() -> [u64; SECTION_FAILURE_N] {
    core::array::from_fn(|i| SECTION_FAILURES[i].load(Ordering::Relaxed))
}

/// Section failures, rendered after the refused children: like those, one
/// of these explains a game that stopped (an in-page error ends a process
/// that does not handle it).
pub(super) fn render_section_failures(snap: &Snapshot) -> String {
    if snap.section_failures.iter().all(|&c| c == 0) {
        return String::new();
    }
    let mut s =
        String::from("SECTION FILLS FAILED: the director did not give a mapped file's bytes.\n");
    for k in SectionFailure::ALL {
        let c = snap.section_failures[k as usize];
        if c > 0 {
            s.push_str(&format!("  {:<32} {c:>8}\n", k.label()));
        }
    }
    if snap.last_in_page_addr != 0 {
        s.push_str(&format!(
            "  last in-page error at {:#x}\n",
            snap.last_in_page_addr
        ));
    }
    s
}
