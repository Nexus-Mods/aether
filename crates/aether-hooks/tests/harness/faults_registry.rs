//! Generated faults never crash or hang the shim's registry calls
//! (`docs/wire.md` 3.7 and 4.4): the property of [`crate::faults_file`] over
//! a registry read (`RegQueryValueExW`, REG_KEY) and write (`NtSetValueKey`,
//! REG_SET_VALUE), with nothing cached, so every read asks the director.

use crate::fault_table::{self, reg_call, Ctx, HookedOp, Seen, DEADLINE};
use crate::faults_file::{cases, check, Op, Subject};
use std::time::Duration;

fn reg_op(ctx: &'static Ctx, op: Op, i: usize) -> Seen {
    match op {
        // A few value names, again and again: the key's values all cross in
        // one REG_KEY reply, which a value per write would grow past
        // `payload_cap` (and the read then falls back, as an overflow does).
        Op::Call(h) => reg_call(ctx, h, i % 8),
        Op::WriteChunks(_) => unreachable!("a registry call writes no file"),
    }
}

#[test]
fn generated_faults_never_crash_or_hang_a_registry_call() {
    isolate!();
    let (served, ctx) = fault_table::registry_setup();
    let s = Subject::new(
        served,
        ctx,
        reg_op,
        DEADLINE + Duration::from_secs(2),
        &[Op::Call(HookedOp::RegRead), Op::Call(HookedOp::RegWrite)],
    );
    check("registry calls", s, cases(1));
}
