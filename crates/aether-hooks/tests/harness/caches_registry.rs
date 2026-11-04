//! The registry answer cache (`RegClient::cached`, `crates/aether-hooks/src/regclient.rs`)
//! and the enumeration lists kept per key handle (`regquery.rs`), request by request, as the
//! recorder saw them, against the registry generation the director publishes in the ring
//! header (`docs/wire.md` 4.5).
//!
//! The rule: REG_LOOKUP and REG_KEY answers are kept per folded path under the generation read
//! before asking, and used only while the header still holds it; generation 0 caches nothing;
//! a failed read is never kept; 4096 entries per map, then the map starts over. A director
//! moves the generation on every registry write before its reply, so a write by any process
//! makes every process's answers unusable before the writer hears back.
//!
//! Most tests drive the process's client (`regclient::lookup`, `regclient::key`) with no hooks
//! installed, so nothing else in the process sends a request. The tests that go through the
//! hooks make their keys in the overlay only, under `HKCU\Software\<name>-<pid>`: a key whose
//! real counterpart does not exist is created by `REG_CREATE_KEY` and never reaches the real
//! registry.
#![allow(unsafe_code)] // Win32 calls

use std::time::Duration;

use crate::fakedirector::{self, Fault, Match, Options, Served};
use crate::reg;
use aether_hooks::regclient::{self, RegClient};
use aether_protocol::{Opcode, Reply, Status};
use aether_refdirector::harness::{Mark, OwnedReplyBytes};
use aether_registry::Lookup;
use fakedirector::RegGen;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, KEY_READ, REG_DWORD, REG_OPENED_EXISTING_KEY,
    REG_OPTION_NON_VOLATILE,
};

/// How long a test waits for a held reply to be held. Only a bound: the wait ends when the
/// fault fires.
const HELD: Duration = Duration::from_secs(10);

const ERROR_NO_MORE_ITEMS: u32 = 259;
const STATUS_UNSUCCESSFUL: i32 = 0xC000_0001u32 as i32;

#[link(name = "ntdll")]
extern "system" {
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

/// The director with its registry, and the process's client on it; no hooks.
fn client(reg_gen: RegGen) -> (&'static Served, std::path::PathBuf) {
    let (served, root) = reg::start_director_with(
        "caches-registry",
        Options {
            arena_len: 0,
            reg_gen,
            ..Options::default()
        },
    );
    // What an install with every registry detour in records; the client needs a director only.
    regclient::detours_installed(&[]);
    (served, root)
}

/// The director with its registry, and the shim's hooks.
fn hooks(reg_gen: RegGen) -> &'static Served {
    reg::install_hooks_with(
        "caches-registry-hooks",
        Options {
            arena_len: 0,
            reg_gen,
            ..Options::default()
        },
    )
}

fn key_path(test: &str) -> String {
    format!(r"\Registry\Machine\Software\AetherRegCacheTest\{test}")
}

/// The opcodes recorded since `m`.
fn ops(s: &Served, m: Mark) -> Vec<Option<Opcode>> {
    s.recorder.ops_since(m)
}

fn dword(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

#[test]
fn a_repeated_lookup_and_key_read_cost_zero_round_trips() {
    isolate!();
    let (s, _root) = client(RegGen::Reference);
    assert_ne!(regclient::generation(), 0, "the reference publishes one");
    let k = key_path("Repeat");

    let m = s.recorder.mark();
    assert_eq!(regclient::lookup(&k), Ok((Lookup::Absent, false)));
    assert_eq!(regclient::key(&k), Ok(None));
    assert_eq!(
        ops(s, m),
        [Some(Opcode::RegLookup), Some(Opcode::RegKey)],
        "each first read crosses once"
    );

    let m = s.recorder.mark();
    for _ in 0..3 {
        assert_eq!(regclient::lookup(&k), Ok((Lookup::Absent, false)));
        assert_eq!(regclient::key(&k), Ok(None));
        // Another spelling of the same path is the same entry.
        assert_eq!(regclient::key(&k.to_uppercase()), Ok(None));
    }
    assert_eq!(s.recorder.total_since(m), 0, "every repeat was a hit");
}

#[test]
fn at_generation_zero_nothing_is_cached() {
    isolate!();
    let (s, _root) = client(RegGen::Fixed(0));
    assert_eq!(regclient::generation(), 0);
    let k = key_path("GenZero");

    let m = s.recorder.mark();
    for _ in 0..5 {
        assert_eq!(regclient::key(&k), Ok(None));
    }
    assert_eq!(ops(s, m), vec![Some(Opcode::RegKey); 5]);

    let m = s.recorder.mark();
    for _ in 0..5 {
        assert_eq!(regclient::lookup(&k), Ok((Lookup::Absent, false)));
    }
    assert_eq!(ops(s, m), vec![Some(Opcode::RegLookup); 5]);
}

#[test]
fn a_generation_bump_from_outside_refetches() {
    isolate!();
    let (s, _root) = client(RegGen::Reference);
    let k = key_path("Bump");
    regclient::key(&k).unwrap();
    let m = s.recorder.mark();
    regclient::key(&k).unwrap();
    assert_eq!(s.recorder.total_since(m), 0, "cached");

    // Another process's write moves the header, and nothing else reaches this one.
    s.publish_reg_generation(regclient::generation() + 1);
    let m = s.recorder.mark();
    assert_eq!(regclient::key(&k), Ok(None));
    assert_eq!(ops(s, m), [Some(Opcode::RegKey)], "the next read crosses");
    let m = s.recorder.mark();
    assert_eq!(regclient::key(&k), Ok(None));
    assert_eq!(s.recorder.total_since(m), 0, "the one after hits");
}

#[test]
fn an_answer_in_flight_while_the_generation_moves_is_not_used_after_it() {
    isolate!();
    let (s, _root) = client(RegGen::Reference);
    let k = key_path("InFlight");
    let before = regclient::generation();
    let held = s.faults.arm(
        Match {
            op: Some(Opcode::RegKey),
            path_prefix: Some(k.clone()),
            nth: Some(1),
            ..Match::default()
        },
        Fault::Delay(HELD),
    );

    let m = s.recorder.mark();
    let reader = std::thread::spawn({
        let k = k.clone();
        move || regclient::key(&k)
    });
    // The director has answered (the delay holds its reply), and the reader has not heard.
    assert!(s.faults.wait_applied(held, 1, HELD), "the reply is held");
    s.publish_reg_generation(before + 1);
    s.faults.release();
    assert_eq!(
        reader.join().unwrap(),
        Ok(None),
        "the caller gets the answer"
    );
    assert_eq!(ops(s, m), [Some(Opcode::RegKey)]);

    // It was kept under the generation read before asking, which the header has moved past.
    let m = s.recorder.mark();
    assert_eq!(regclient::key(&k), Ok(None));
    assert_eq!(ops(s, m), [Some(Opcode::RegKey)], "the next read crosses");
    let m = s.recorder.mark();
    regclient::key(&k).unwrap();
    assert_eq!(s.recorder.total_since(m), 0, "the one after hits");
}

#[test]
fn a_failed_read_is_not_cached() {
    isolate!();
    let (s, _root) = client(RegGen::Reference);
    let k = key_path("Failed");
    regclient::set_value(&k, "v", REG_DWORD, &dword(7)).unwrap();
    let refused = s.faults.arm(
        Match {
            op: Some(Opcode::RegKey),
            path_prefix: Some(k.clone()),
            times: Some(1),
            ..Match::default()
        },
        Fault::Status(Status::NotSupported),
    );

    let m = s.recorder.mark();
    assert_eq!(regclient::key(&k), Err(Status::NotSupported));
    let node = regclient::key(&k).unwrap().expect("the director's node");
    assert_eq!(node.values[0].data, dword(7), "the second answer is served");
    let sent = s.recorder.since(m);
    assert_eq!(
        sent.iter().map(|r| (r.op(), r.status)).collect::<Vec<_>>(),
        [
            (Some(Opcode::RegKey), Status::NotSupported.code()),
            (Some(Opcode::RegKey), Status::Ok.code()),
        ]
    );
    assert_eq!(s.faults.applied(refused), 1);

    let m = s.recorder.mark();
    assert_eq!(regclient::key(&k).unwrap().unwrap().values.len(), 1);
    assert_eq!(s.recorder.total_since(m), 0, "the third read hits");
}

#[test]
fn a_write_through_this_process_moves_the_generation_before_its_reply() {
    isolate!();
    let (s, _root) = client(RegGen::Reference);
    let k = key_path("OwnWrite");
    assert_eq!(regclient::key(&k), Ok(None));
    let before = regclient::generation();

    regclient::set_value(&k, "w", REG_DWORD, &dword(3)).unwrap();
    // Read with nothing sent since the reply: the header moved before it.
    assert!(regclient::generation() > before);
    let m = s.recorder.mark();
    let node = regclient::key(&k).unwrap().expect("the written node");
    assert_eq!(node.values[0].name, "w");
    assert_eq!(ops(s, m), [Some(Opcode::RegKey)], "the next read crosses");
}

#[test]
fn the_cache_starts_over_past_its_bound() {
    isolate!();
    let (s, _root) = client(RegGen::Reference);
    let k = |i: usize| key_path(&format!(r"Bound\K{i}"));

    let m = s.recorder.mark();
    for i in 0..4096 {
        regclient::lookup(&k(i)).unwrap();
    }
    assert_eq!(s.recorder.count_since(m, Opcode::RegLookup), 4096);
    // 4096 entries are held.
    let m = s.recorder.mark();
    regclient::lookup(&k(0)).unwrap();
    regclient::lookup(&k(4095)).unwrap();
    assert_eq!(s.recorder.total_since(m), 0, "the bound holds 4096");

    // The 4097th starts the map over: the first is asked again.
    let m = s.recorder.mark();
    regclient::lookup(&k(4096)).unwrap();
    regclient::lookup(&k(0)).unwrap();
    assert_eq!(
        ops(s, m),
        [Some(Opcode::RegLookup), Some(Opcode::RegLookup)]
    );
    let m = s.recorder.mark();
    regclient::lookup(&k(4096)).unwrap();
    assert_eq!(
        s.recorder.total_since(m),
        0,
        "the one that started it over is kept"
    );
}

/// S16: a director that moves the generation only after it has replied to a write leaves
/// every other process serving stale answers until it does. wire.md 4.5 requires the move
/// before the reply for this reason; conformance C9 checks directors make it.
#[test]
fn a_director_that_bumps_after_replying_leaves_a_second_client_stale() {
    isolate!();
    let (s, root) = client(RegGen::Manual);
    s.publish_reg_generation(1);
    let k = key_path("Stale");
    let a = regclient::global().unwrap();
    let other = fakedirector::second_client(&root);
    let b = RegClient::new(&other);

    assert_eq!(a.key(&k), Ok(None));
    // B writes. The director replied and has not published yet.
    b.set_value(&k, "late", REG_DWORD, &dword(1)).unwrap();
    assert_eq!(regclient::generation(), 1);
    let m = s.recorder.mark();
    assert_eq!(a.key(&k), Ok(None), "A's cached answer, stale");
    assert_eq!(s.recorder.total_since(m), 0);

    // The bump, late: A asks again and sees B's write.
    s.publish_reg_generation(2);
    let m = s.recorder.mark();
    let node = a.key(&k).unwrap().expect("B's write");
    assert_eq!(node.values[0].name, "late");
    assert_eq!(ops(s, m), [Some(Opcode::RegKey)]);
}

// ---- Through the hooks ----

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// This process's overlay-only key `HKCU\Software\<name>-<pid>`, and its canonical path.
fn overlay_key(name: &str) -> (String, String) {
    let sub = format!(r"Software\{name}-{}", std::process::id());
    let canon = format!(r"\Registry\User\<CurrentUser>\{sub}");
    (sub, canon)
}

/// `RegCreateKeyExW(HKCU, sub)`: the Win32 error and, on success, the key and disposition.
fn create(sub: &str) -> (u32, HKEY, u32) {
    let mut k: HKEY = std::ptr::null_mut();
    let mut disp = 0u32;
    let st = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(sub).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_ALL_ACCESS,
            std::ptr::null(),
            &mut k,
            &mut disp,
        )
    };
    (st, k, disp)
}

fn open(sub: &str) -> HKEY {
    let mut k: HKEY = std::ptr::null_mut();
    let st = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(sub).as_ptr(), 0, KEY_READ, &mut k) };
    assert_eq!(st, 0, "open {sub}");
    k
}

fn set_dword(k: HKEY, name: &str, v: u32) {
    let st = unsafe { RegSetValueExW(k, wide(name).as_ptr(), 0, REG_DWORD, dword(v).as_ptr(), 4) };
    assert_eq!(st, 0, "set {name}");
}

fn query_dword(k: HKEY, name: &str) -> Result<u32, u32> {
    let mut data = [0u8; 4];
    let mut len = 4u32;
    let mut ty = 0u32;
    let st = unsafe {
        RegQueryValueExW(
            k,
            wide(name).as_ptr(),
            std::ptr::null(),
            &mut ty,
            data.as_mut_ptr(),
            &mut len,
        )
    };
    if st != 0 {
        return Err(st);
    }
    assert_eq!((ty, len), (REG_DWORD, 4));
    Ok(u32::from_le_bytes(data))
}

/// The name of value `index`, or the Win32 error.
fn enum_value(k: HKEY, index: u32) -> Result<String, u32> {
    let mut name = [0u16; 64];
    let mut len = name.len() as u32;
    let st = unsafe {
        RegEnumValueW(
            k,
            index,
            name.as_mut_ptr(),
            &mut len,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    match st {
        0 => Ok(String::from_utf16_lossy(&name[..len as usize])),
        e => Err(e),
    }
}

/// An overlay-only key with `values`, made through the hooks.
fn overlay_key_with(name: &str, values: &[(&str, u32)]) -> (String, String) {
    let (sub, canon) = overlay_key(name);
    let (st, k, _) = create(&sub);
    assert_eq!(st, 0, "create {sub}");
    for (n, v) in values {
        set_dword(k, n, *v);
    }
    unsafe { RegCloseKey(k) };
    (sub, canon)
}

#[test]
fn a_repeated_value_query_through_the_hooks_costs_zero_round_trips() {
    isolate!();
    let s = hooks(RegGen::Reference);
    let (sub, _) = overlay_key_with("AetherRegCacheQuery", &[("fov", 90)]);

    let k = open(&sub);
    assert_eq!(query_dword(k, "fov"), Ok(90));
    let m = s.recorder.mark();
    assert_eq!(query_dword(k, "fov"), Ok(90));
    assert_eq!(s.recorder.total_since(m), 0, "the second query is a hit");
    unsafe { RegCloseKey(k) };

    // So are another open of the key and its query.
    let m = s.recorder.mark();
    let k = open(&sub);
    assert_eq!(query_dword(k, "fov"), Ok(90));
    unsafe { RegCloseKey(k) };
    assert_eq!(s.recorder.total_since(m), 0);
}

/// A key's value enumeration is merged once and its later indexes cost nothing while the
/// generation holds; when another process writes, the next index is merged again and shows
/// the write.
#[test]
fn an_enumeration_reuses_its_list_until_the_generation_moves() {
    isolate!();
    let s = hooks(RegGen::Reference);
    let (sub, canon) = overlay_key_with("AetherRegCacheEnum", &[("a", 1), ("b", 2), ("c", 3)]);
    let k = open(&sub);
    assert_eq!(enum_value(k, 0).as_deref(), Ok("a"));

    let m = s.recorder.mark();
    assert_eq!(enum_value(k, 1).as_deref(), Ok("b"));
    assert_eq!(enum_value(k, 2).as_deref(), Ok("c"));
    assert_eq!(enum_value(k, 3), Err(ERROR_NO_MORE_ITEMS));
    assert_eq!(s.recorder.total_since(m), 0, "later indexes reuse the list");

    // Another process adds a value: the reference moves the generation before replying.
    let other = fakedirector::second_client(&std::env::temp_dir());
    RegClient::new(&other)
        .set_value(&canon, "d", REG_DWORD, &dword(4))
        .unwrap();
    let m = s.recorder.mark();
    assert_eq!(enum_value(k, 3).as_deref(), Ok("d"), "merged again");
    // The handle's key is looked up again, and its node fetched for the merge.
    assert_eq!(ops(s, m), [Some(Opcode::RegLookup), Some(Opcode::RegKey)]);
    let m = s.recorder.mark();
    assert_eq!(enum_value(k, 4), Err(ERROR_NO_MORE_ITEMS));
    assert_eq!(s.recorder.total_since(m), 0);
    unsafe { RegCloseKey(k) };
}

#[test]
fn at_generation_zero_every_enumeration_index_asks_again() {
    isolate!();
    let s = hooks(RegGen::Fixed(0));
    let (sub, _) = overlay_key_with("AetherRegCacheEnumZero", &[("a", 1), ("b", 2)]);
    let k = open(&sub);
    assert_eq!(enum_value(k, 0).as_deref(), Ok("a"));
    let m = s.recorder.mark();
    assert_eq!(enum_value(k, 1).as_deref(), Ok("b"));
    // The handle's key looked up and its node fetched, as for every index.
    assert_eq!(
        ops(s, m),
        [Some(Opcode::RegLookup), Some(Opcode::RegKey)],
        "the list is merged again"
    );
    unsafe { RegCloseKey(k) };
}

/// The ops after the one REG_CREATE_KEY recorded since `m`.
fn after_create(s: &Served, m: Mark) -> Vec<(Option<Opcode>, i32, Option<Lookup>)> {
    let sent = s.recorder.since(m);
    let creates: Vec<usize> = (0..sent.len())
        .filter(|&i| sent[i].op() == Some(Opcode::RegCreateKey))
        .collect();
    assert_eq!(creates.len(), 1, "one create: {sent:?}");
    sent[creates[0] + 1..]
        .iter()
        .map(|r| {
            let state = match r.reply() {
                Some(Reply::RegLookup(l)) => Some(l.state),
                _ => None,
            };
            (r.op(), r.status, state)
        })
        .collect()
}

/// S14: another process created the key first (REG_CREATE_KEY answered `ST_EXISTS`), but the
/// lookup after it says absent. wire.md 4.4: a fresh REG_LOOKUP, then `STATUS_UNSUCCESSFUL`.
#[test]
fn a_create_answered_exists_then_lookup_absent_fails_unsuccessful() {
    isolate!();
    let s = hooks(RegGen::Reference);
    let (sub, canon) = overlay_key("AetherRegCacheExistsAbsent");
    let exists = s.faults.arm(
        Match {
            op: Some(Opcode::RegCreateKey),
            ..Match::default()
        },
        Fault::Status(Status::Exists),
    );

    // One `NtCreateKey`, so one create on the ring (`RegCreateKeyExW` tries twice).
    let m = s.recorder.mark();
    assert_eq!(nt_create(&sub), STATUS_UNSUCCESSFUL);
    assert_eq!(s.faults.applied(exists), 1);
    assert_eq!(
        after_create(s, m),
        [(
            Some(Opcode::RegLookup),
            Status::Ok.code(),
            Some(Lookup::Absent)
        )],
        "one fresh lookup after the create, and nothing else"
    );
    assert_eq!(regclient::lookup(&canon), Ok((Lookup::Absent, false)));

    // What a Win32 caller sees.
    let (st, _, _) = create(&sub);
    assert_eq!(st, unsafe { RtlNtStatusToDosError(STATUS_UNSUCCESSFUL) });
}

/// `NtCreateKey` of `HKCU\<sub>` by its absolute NT name, KEY_ALL_ACCESS; the key it opens is
/// closed.
fn nt_create(sub: &str) -> i32 {
    #[link(name = "ntdll")]
    extern "system" {
        fn NtCreateKey(
            key: *mut isize,
            access: u32,
            oa: *const reg::ObjectAttributes,
            title_index: u32,
            class: *const reg::UnicodeString,
            options: u32,
            disposition: *mut u32,
        ) -> i32;
    }
    let name = format!(r"\REGISTRY\USER\{}\{sub}", reg::user_sid());
    let mut h = 0isize;
    let mut disp = 0u32;
    let st = reg::with_oa(0, &name, |oa| unsafe {
        NtCreateKey(
            &mut h,
            KEY_ALL_ACCESS,
            oa,
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            &mut disp,
        )
    });
    if st >= 0 {
        reg::close(h);
    }
    st
}

/// The director created the key and answers `ST_EXISTS`, as it would to the process that lost
/// the race: the reference moves the generation for the create it made.
fn created_but_exists(r: &mut OwnedReplyBytes) {
    if r.op() == Some(Opcode::RegCreateKey) && r.status == Status::Ok.code() {
        r.status = Status::Exists.code();
        r.payload.clear();
    }
}

/// S14, the other way: the key was created by someone else first, so the lookup after the
/// `ST_EXISTS` says present and the create opens it.
#[test]
fn a_create_answered_exists_then_lookup_present_opens_the_key() {
    isolate!();
    let s = hooks(RegGen::Reference);
    let (sub, _) = overlay_key("AetherRegCacheExistsPresent");
    let raced = s.faults.arm(
        Match {
            op: Some(Opcode::RegCreateKey),
            ..Match::default()
        },
        Fault::Rewrite(created_but_exists),
    );

    let m = s.recorder.mark();
    let (st, k, disp) = create(&sub);
    assert_eq!(st, 0, "opened");
    assert_eq!(disp, REG_OPENED_EXISTING_KEY);
    assert_eq!(s.faults.applied(raced), 1);
    assert_eq!(
        after_create(s, m),
        [(
            Some(Opcode::RegLookup),
            Status::Ok.code(),
            Some(Lookup::Present { created: true })
        )],
        "one fresh lookup after the create"
    );
    set_dword(k, "x", 5);
    assert_eq!(query_dword(k, "x"), Ok(5));
    unsafe { RegCloseKey(k) };
}
