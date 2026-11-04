//! `Client::call` adds no allocation of its own: with a reply buffer that
//! has room, a call allocates nothing on the calling thread. The director
//! runs on another thread, which is not counted.

use aether_ipc::ring;
use aether_ipc::{OwnedSeg, RingClient, RingServer, SpinNotifier};
use aether_protocol::{
    AttrResp, ChangedReply, KeyReply, LookupReply, OpenFlags, OpenResp, ReadReply, Request,
    RequestFlags, RootId, Status,
};
use aether_rpc::{serve, Client, Director, Listing, ReadSlot};
use std::sync::atomic::{AtomicBool, Ordering};

#[global_allocator]
static A: aether_testkit::alloc::Counting = aether_testkit::alloc::Counting;

/// The allocations `f` makes on this thread.
fn allocations(f: impl FnOnce()) -> usize {
    aether_testkit::alloc::allocations(f).1
}

/// Answers everything with fixed values.
struct Fixed;

impl Director for Fixed {
    fn get_attr(&mut self, _: RootId, _: &str) -> Result<AttrResp, Status> {
        Ok(AttrResp {
            found: true,
            is_dir: false,
            size: 1,
            mtime: 1,
        })
    }
    fn read_dir(&mut self, _: RootId, path: &str, out: &mut Listing) -> Result<(), Status> {
        out.push(aether_protocol::DirEntry {
            name: path,
            is_dir: false,
            size: 0,
            mtime: 0,
        });
        Ok(())
    }
    fn open(&mut self, _: RootId, _: OpenFlags, _: &str) -> Result<OpenResp, Status> {
        Ok(OpenResp::default())
    }
    fn read(
        &mut self,
        _: ReadSlot<'_>,
        _: RequestFlags,
        _: u64,
        _: u64,
        len: u32,
    ) -> Result<ReadReply<'_>, Status> {
        Ok(ReadReply::Inline(&[7; 64][..len.min(64) as usize]))
    }
    fn write(&mut self, _: u64, _: u64, data: &[u8]) -> Result<u32, Status> {
        Ok(data.len() as u32)
    }
    fn set_attr(&mut self, _: u64, _: u64) -> Result<(), Status> {
        Ok(())
    }
    fn rename(&mut self, _: RootId, _: &str, _: &str) -> Result<(), Status> {
        Ok(())
    }
    fn delete(&mut self, _: RootId, _: &str) -> Result<(), Status> {
        Err(Status::NotFound)
    }
    fn mkdir(&mut self, _: RootId, _: u32, _: &str) -> Result<(), Status> {
        Ok(())
    }
    fn close(&mut self, _: u64) -> Result<(), Status> {
        Ok(())
    }
    fn heartbeat(&mut self) -> Result<(), Status> {
        Ok(())
    }
    fn stored_names(&mut self, _: RootId, _: u32, _: &str) -> Result<&str, Status> {
        Ok("A/B")
    }
    fn reg_lookup(&mut self, _: &str) -> Result<LookupReply, Status> {
        Ok(LookupReply {
            version: 1,
            state: aether_protocol::Lookup::Absent,
            below: false,
        })
    }
    fn reg_key(&mut self, _: &str) -> Result<KeyReply<'_>, Status> {
        Ok(KeyReply {
            version: 1,
            node: None,
        })
    }
    fn reg_set_value(&mut self, _: &str, _: &str, _: u32, _: &[u8]) -> Result<u64, Status> {
        Ok(2)
    }
    fn reg_delete_value(&mut self, _: &str, _: &str) -> Result<u64, Status> {
        Ok(2)
    }
    fn reg_create_key(&mut self, _: &str, _: bool) -> Result<u64, Status> {
        Ok(2)
    }
    fn reg_delete_key(&mut self, _: &str) -> Result<u64, Status> {
        Ok(2)
    }
    fn reg_rename_key(&mut self, _: &str, _: &str) -> Result<u64, Status> {
        Ok(2)
    }
    fn reg_changed(&mut self, _: &str, _: bool, _: u64) -> Result<ChangedReply, Status> {
        Ok(ChangedReply {
            version: 2,
            changed: false,
        })
    }
    fn rename_x(&mut self, _: RootId, _: RootId, _: &str, _: &str) -> Result<(), Status> {
        Ok(())
    }
    fn stage_exec(&mut self, _: RootId, _: &str) -> Result<&str, Status> {
        Ok("C:/real")
    }
}

#[test]
fn a_call_with_a_buffer_that_has_room_allocates_nothing() {
    let owned = OwnedSeg::new(64 * 1024);
    ring::init(owned.seg(), 4, 4096).unwrap();
    let stop = AtomicBool::new(false);
    let reqs = [
        Request::GetAttr {
            root: RootId(1),
            path: "a/b",
        },
        Request::ReadDir {
            root: RootId(1),
            path: "dir",
        },
        Request::Open {
            root: RootId(1),
            flags: OpenFlags::READ,
            path: "a/b",
        },
        Request::Read {
            slot_flags: RequestFlags::empty(),
            fh: 1,
            offset: 0,
            len: 64,
        },
        Request::Write {
            fh: 1,
            offset: 0,
            data: b"data",
        },
        Request::Delete {
            root: RootId(1),
            path: "gone",
        },
        Request::Heartbeat {},
        Request::StoredNames {
            root: RootId(1),
            skip: 0,
            path: "a/b",
        },
        Request::RegLookup { path: "HKLM\\k" },
        Request::RegKey { path: "HKLM\\k" },
        Request::RegSetValue {
            path: "HKLM\\k",
            name: "v",
            ty: 1,
            data: b"x",
        },
        Request::RegChanged {
            path: "HKLM\\k",
            subtree: true,
            since: 0,
        },
        Request::StageExec {
            root: RootId(1),
            path: "a.exe",
        },
    ];
    std::thread::scope(|s| {
        s.spawn(|| {
            let server = RingServer::new(owned.seg(), SpinNotifier).unwrap();
            serve(&server, &mut Fixed, &stop)
        });
        let client = Client::new(RingClient::new(owned.seg(), SpinNotifier).unwrap());
        let mut scratch = Vec::with_capacity(4096);
        let mut seen = 0;
        let n = allocations(|| {
            for _ in 0..3 {
                for req in &reqs {
                    // DELETE is answered NotFound: the error path counts too.
                    let ok = client.call(req, &mut scratch).is_ok();
                    assert_eq!(ok, !matches!(req, Request::Delete { .. }));
                    seen += 1;
                }
            }
        });
        stop.store(true, Ordering::Release);
        assert_eq!(seen, 3 * reqs.len());
        assert_eq!(n, 0, "allocations on the calling thread");
    });
}
