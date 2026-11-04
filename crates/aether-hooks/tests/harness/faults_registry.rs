//! Generated faults never crash or hang the shim's registry calls
//! (`docs/wire.md` 3.7 and 4.4): the property of [`crate::faults_file`] over
//! a registry read (`RegQueryValueExW`, REG_KEY) and write (`NtSetValueKey`,
//! REG_SET_VALUE), with nothing cached, so every read asks the director.

use crate::fault_table::{self, reg_call, Ctx, HookedOp, Seen, DEADLINE};
use crate::faults_file::{cases, check, every_cut, DirectorReplies, Op, Subject, KINDS};
use std::time::Duration;

fn reg_op(ctx: &'static Ctx, op: Op, i: usize) -> Seen {
    match op {
        // A few value names, again and again: the key's values all cross in
        // one REG_KEY reply, which a value per write would grow past
        // `payload_cap`, and every read of the key would then fail
        // (wire.md 4.4).
        Op::Call(h) => reg_call(ctx, h, i % 8),
        Op::WriteChunks(_) => unreachable!("a registry call writes no file"),
    }
}

/// The registry read and write.
const OPS: [Op; 2] = [Op::Call(HookedOp::RegRead), Op::Call(HookedOp::RegWrite)];

fn subject(ops: &[Op]) -> &'static Subject {
    let director = DirectorReplies::default();
    let (served, ctx) = fault_table::registry_setup(Some(director.tap()));
    Subject::new(
        served,
        ctx,
        reg_op,
        DEADLINE + Duration::from_secs(2),
        ops,
        director,
    )
}

#[test]
fn generated_faults_never_crash_or_hang_a_registry_call() {
    isolate!();
    // Every kind but the bulk lie, which no registry reply can carry.
    let kinds: Vec<&str> = KINDS.into_iter().filter(|k| *k != "bulk lie").collect();
    check("registry calls", subject(&OPS), cases(1), &kinds);
}

/// Every cut of a registry read's and write's reply: none decodes to
/// another answer (wire.md 3.2), so each is the table's garbage cell.
#[test]
fn every_cut_of_a_registry_reply_is_the_table_s() {
    isolate!();
    let s = subject(&[]);
    let wrong: Vec<String> = OPS.iter().flat_map(|&op| every_cut(s, op)).collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
