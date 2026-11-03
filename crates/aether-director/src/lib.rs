//! The director **kernel**: one provider per root, a ring server the injected
//! shim talks to, and the leaf/staging pieces a host composes around them.
//!
//! **This is not a host API.** Session lifecycle — roots, composition, serve,
//! launch — was `vfs-embed`'s (now deleted); Cairn's host is the Clojure
//! director (`src/aether/director/`). This crate is the kernel the shim's
//! tests run against.
//!
//! What is here:
//! * [`Director`] — the root → provider table and the handle namespace
//! * [`ipc::IpcServe`] — the shared-memory ring + workers
//! * `DiskProvider` / `MountGraph` live in `aether-compose`
//! * [`stage`] — putting a launch image on real disk for `CreateProcess`
//! * [`io_stats`] — process-wide counters, including rejected writes
//!
//! The `Provider` trait lives in [`aether_protocol`] so zip stays free of host deps.

#![deny(unsafe_code)]

pub mod director;
pub mod io_stats;
// The shared-memory ring is how an injected shim reaches this kernel — on
// Windows directly, and under Proton from inside Wine, where the ring lives in
// a real file both sides mmap by path (`IpcServe::start_file_backed`). So `ipc`
// is no longer gated at the module: the OS handles it holds are chosen *inside*
// the file by `RingMapping`, the same shape the earlier increment used for
// `ring_dispatch`. Only the named-section half (`IpcServe::start`, the event
// notifier, the `VFS_RING_SECTION` handshake) is `#[cfg(windows)]` now.
// `ring_dispatch` remains ungated for the reason it always was: it is portable
// protocol translation on top of the transport, depending only on
// `aether-protocol`, `aether-ipc`, `aether-compose` and `Director`.
pub mod ipc;
pub mod registry;
pub mod ring_dispatch;
pub mod stage;

pub use aether_provider::{
    DirEntry, Handle, Provider, RootId, Stat, KIND_DIR, KIND_FILE, OPEN_READ, OPEN_WRITE,
};
pub use director::{Director, ExecStager, OpenInfo};
pub use io_stats::{
    mark_launch as io_mark_launch, reset as io_stats_reset, snapshot_report as io_stats_report,
};
pub use ipc::IpcServe;
pub use registry::{RegistryGenSink, RegistryGeneration};

#[cfg(test)]
mod tests {
    use super::*;
    use aether_compose::DiskProvider;
    use std::io::Write;
    use std::sync::Arc;

    #[test]
    fn disk_provider_open_read() {
        let dir = aether_testkit::scratch_path("vfs-dir-disk");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("hello.txt");
        {
            let mut f = std::fs::File::create(&file).unwrap();
            f.write_all(b"hello-director").unwrap();
        }
        let d = Director::new();
        d.mount(RootId::DEFAULT, Arc::new(DiskProvider::new(&dir)))
            .unwrap();
        let st = d.getattr(RootId::DEFAULT, "hello.txt").unwrap().unwrap();
        assert_eq!(st.kind, KIND_FILE);
        assert_eq!(st.size, 14);
        let (fh, size, is_dir) = d.open(RootId::DEFAULT, "hello.txt", OPEN_READ).unwrap();
        assert!(!is_dir && size == 14);
        let mut buf = [0u8; 32];
        let n = d.read(fh, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello-director");
        d.close(fh).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Named distinctly from `director::tests::unmount_drops_visibility` —
    // this crate-root smoke test and that one exercised the identical
    // behavior under the identical leaf name after both were independently
    // renamed from `clear_mounts_drops_visibility`, which is exactly the
    // kind of thing that makes a test count silently miscounted by one.
    #[test]
    fn bare_director_unmount_drops_visibility() {
        let dir = aether_testkit::scratch_path("vfs-clear");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("x.txt"), b"x").unwrap();
        let d = Director::new();
        d.mount(RootId::DEFAULT, Arc::new(DiskProvider::new(&dir)))
            .unwrap();
        assert!(d.getattr(RootId::DEFAULT, "x.txt").unwrap().is_some());
        d.unmount(RootId::DEFAULT).unwrap();
        assert!(d.getattr(RootId::DEFAULT, "x.txt").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
