//! The null director: every opcode answered OK with the smallest valid reply.

use aether_protocol::{
    AttrResp, ChangedReply, KeyReply, Lookup, LookupReply, OpenFlags, OpenResp, ReadReply,
    RequestFlags, RootId, Status,
};
use aether_rpc::{Director, Listing, ReadSlot};

/// Answers every opcode `Status::Ok` with nothing in it: GETATTR finds
/// nothing, READDIR lists nothing, OPEN gives handle 1 of size 0, READ gives
/// zero bytes inline, WRITE takes every byte, STORED_NAMES and STAGE_EXEC
/// give an empty string, and the registry answers version 0 with no key.
/// What is left of a round trip is the ring's own cost. Unknown opcodes and
/// undecodable payloads keep the default `Status::BadRequest`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Null;

impl Director for Null {
    fn get_attr(&mut self, _: RootId, _: &str) -> Result<AttrResp, Status> {
        Ok(AttrResp {
            found: false,
            is_dir: false,
            size: 0,
            mtime: 0,
        })
    }
    fn read_dir(&mut self, _: RootId, _: &str, _: &mut Listing) -> Result<(), Status> {
        Ok(())
    }
    fn open(&mut self, _: RootId, _: OpenFlags, _: &str) -> Result<OpenResp, Status> {
        Ok(OpenResp {
            fh: 1,
            ..Default::default()
        })
    }
    fn read(
        &mut self,
        _: ReadSlot<'_>,
        _: RequestFlags,
        _: u64,
        _: u64,
        _: u32,
    ) -> Result<ReadReply<'_>, Status> {
        Ok(ReadReply::Inline(&[]))
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
        Ok(())
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
        Ok("")
    }
    fn reg_lookup(&mut self, _: &str) -> Result<LookupReply, Status> {
        Ok(LookupReply {
            version: 0,
            state: Lookup::Absent,
            below: false,
        })
    }
    fn reg_key(&mut self, _: &str) -> Result<KeyReply<'_>, Status> {
        Ok(KeyReply {
            version: 0,
            node: None,
        })
    }
    fn reg_set_value(&mut self, _: &str, _: &str, _: u32, _: &[u8]) -> Result<u64, Status> {
        Ok(0)
    }
    fn reg_delete_value(&mut self, _: &str, _: &str) -> Result<u64, Status> {
        Ok(0)
    }
    fn reg_create_key(&mut self, _: &str, _: bool) -> Result<u64, Status> {
        Ok(0)
    }
    fn reg_delete_key(&mut self, _: &str) -> Result<u64, Status> {
        Ok(0)
    }
    fn reg_rename_key(&mut self, _: &str, _: &str) -> Result<u64, Status> {
        Ok(0)
    }
    fn reg_changed(&mut self, _: &str, _: bool, _: u64) -> Result<ChangedReply, Status> {
        Ok(ChangedReply {
            version: 0,
            changed: false,
        })
    }
    fn rename_x(&mut self, _: RootId, _: RootId, _: &str, _: &str) -> Result<(), Status> {
        Ok(())
    }
    fn stage_exec(&mut self, _: RootId, _: &str) -> Result<&str, Status> {
        Ok("")
    }
}
