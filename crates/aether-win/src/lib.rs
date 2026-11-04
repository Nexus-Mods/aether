//! Windows platform layer: cross-process shared memory backing a `aether_ipc::SharedSeg`,
//! and raw Win32 volume/path lookups for canonicalisation (`aether-redirect`).
//! A kill-on-close job for a process tree and a wake event opened with the
//! shim's rights (`job`). Unsafe is confined to `mapping`, `event_notifier`,
//! `job` and `volumes`.

mod event_notifier;
#[cfg(windows)]
mod job;
mod mapping;
mod volumes;

pub use event_notifier::EventNotifier;
#[cfg(windows)]
pub use job::{KillOnCloseJob, WakeEvent};
pub use mapping::SharedMapping;
pub use volumes::{
    drive_mappings, expand_long_path, final_path_for_handle, final_path_for_open,
    is_device_namespace_name, long_name_in, reparse_point_target, short_path_name, DriveMapping,
};
