//! `Null` answers every opcode OK with the smallest valid reply.

use aether_protocol::{
    AttrResp, ChangedReply, KeyReply, Lookup, LookupReply, OpenFlags, OpenResp, ReadReply,
    RequestFlags, RootId, Status,
};
use aether_refdirector::Null;
use aether_rpc::{Director, Listing, ReadSlot};

const R0: RootId = RootId(0);

#[test]
fn every_file_op_is_ok_and_minimal() {
    let mut d = Null;
    assert_eq!(
        d.get_attr(R0, "a"),
        Ok(AttrResp {
            found: false,
            is_dir: false,
            size: 0,
            mtime: 0
        })
    );
    let mut l = Listing::default();
    assert_eq!(d.read_dir(R0, "a", &mut l), Ok(()));
    assert!(l.is_empty());
    assert_eq!(
        d.open(R0, OpenFlags::READ, "a"),
        Ok(OpenResp {
            fh: 1,
            ..Default::default()
        })
    );
    let at = ReadSlot {
        slot: 0,
        arena: None,
        inline_cap: usize::MAX,
    };
    assert_eq!(
        d.read(at, RequestFlags::READ_BULK, 1, 0, 100),
        Ok(ReadReply::Inline(&[]))
    );
    assert_eq!(d.write(1, 0, b"abc"), Ok(3));
    assert_eq!(d.set_attr(1, 9), Ok(()));
    assert_eq!(d.rename(R0, "a", "b"), Ok(()));
    assert_eq!(d.delete(R0, "a"), Ok(()));
    assert_eq!(d.mkdir(R0, 0, "a"), Ok(()));
    assert_eq!(d.close(1), Ok(()));
    assert_eq!(d.heartbeat(), Ok(()));
    assert_eq!(d.stored_names(R0, 0, "a"), Ok(""));
    assert_eq!(d.rename_x(R0, RootId(1), "a", "b"), Ok(()));
    assert_eq!(d.stage_exec(R0, "a.exe"), Ok(""));
}

#[test]
fn every_registry_op_is_ok_and_minimal() {
    let mut d = Null;
    let k = "\\Registry\\Machine\\A";
    assert_eq!(
        d.reg_lookup(k),
        Ok(LookupReply {
            version: 0,
            state: Lookup::Absent,
            below: false
        })
    );
    assert_eq!(
        d.reg_key(k),
        Ok(KeyReply {
            version: 0,
            node: None
        })
    );
    assert_eq!(d.reg_set_value(k, "v", 1, b"x"), Ok(0));
    assert_eq!(d.reg_delete_value(k, "v"), Ok(0));
    assert_eq!(d.reg_create_key(k, false), Ok(0));
    assert_eq!(d.reg_delete_key(k), Ok(0));
    assert_eq!(d.reg_rename_key(k, "B"), Ok(0));
    assert_eq!(
        d.reg_changed(k, true, 0),
        Ok(ChangedReply {
            version: 0,
            changed: false
        })
    );
}

#[test]
fn unknown_and_undecodable_requests_keep_the_framing_rules() {
    let mut d = Null;
    assert_eq!(d.unknown(4), Status::BadRequest);
    assert_eq!(
        d.undecodable(aether_protocol::Opcode::Close, 0, &[]),
        Status::BadRequest
    );
}
