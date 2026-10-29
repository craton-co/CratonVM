// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 19 wave 3, lane syncdoor3 (SYNCDOOR3-1 of
//! `docs/feature-designs/jit-ir-optimizations.md`): the self-locking
//! body of a synchronized method WITH an exception table, compiled end to end
//! through [`crate::try_compile_request`] with every helper wired to a fake
//! address (the code is never run).
//!
//! The shapes are javac's bytecode of the `R16SyncDoorCalls` callees
//! (`guarded`, `thrower`, `blocked`) plus a table-free control (`plain`).
//! Since interpreter round i2 wave 56 turned
//! `CRATONVM_JIT_SELF_LOCK_MODE_EXIT_HANDOVER` on, every call in a self-locking
//! body is followed by a reason-7 mode exit that names the hold; the
//! table-free check admits it, the handlers check did not, so the three table
//! shapes lost their self-locking body (and their compiled callers went back
//! to the ~700 ns helper door). These tests pin that diagnosis, the
//! `CRATONVM_JIT_SELF_LOCK_HANDLERS_MODE_EXITS` fix, and its controls.

use crate::self_lock_census::SelfLockRow as R;
use crate::{CachedBytecodeMethod, JitRuntimeHelpers};
use std::sync::Arc;

const SWITCH: &str = "CRATONVM_JIT_SELF_LOCK_HANDLERS_MODE_EXITS";
const MODE_EXIT_HANDOVER: &str = "CRATONVM_JIT_SELF_LOCK_MODE_EXIT_HANDOVER";
const EXC_HANDOVER: &str = "CRATONVM_JIT_SELF_LOCK_EXC_HANDOVER";

pub(crate) fn method(
    name: &str,
    code: &[u8],
    table: Vec<cratonvm_reader::attribute::ExceptionTableEntry>,
    max_stack: u16,
    max_locals: u16,
) -> CachedBytecodeMethod {
    CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(7),
        class_name: Arc::from("R16SyncDoorCalls$C"),
        method_name: Arc::from(name),
        method_descriptor: Arc::from("(I)I"),
        source_file: None,
        // The VM's padded form (`padded_bytecode`): two trailing zeros.
        code: Arc::from([code, &[0u8, 0u8][..]].concat().as_slice()),
        exception_table: Arc::from(table.as_slice()),
        max_stack,
        max_locals,
        num_params: 1,
        is_synchronized: true,
        is_static: false,
        force_native_cache: std::sync::OnceLock::new(),
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
    }
}

/// Every helper slot a distinct fake, nonzero value (never executed); the
/// in-thread offsets small.
pub(crate) fn wired_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a
    // `usize`, so all-zero is a valid value.
    let mut h: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    let n = std::mem::size_of::<JitRuntimeHelpers>() / std::mem::size_of::<usize>();
    // SAFETY: as above; the struct is exactly `n` `usize`s, borrowed mutably
    // for the length of this slice only.
    let words = unsafe {
        std::slice::from_raw_parts_mut((&mut h as *mut JitRuntimeHelpers).cast::<usize>(), n)
    };
    for (i, w) in words.iter_mut().enumerate() {
        *w = 0x4000_0000 + i * 0x40;
    }
    h.tlab_cursor_offset_in_thread = 0x10;
    h.tlab_end_offset_in_thread = 0x18;
    h.class_id_offset_in_obj = 8;
    h.shadow_stack_offset_in_thread = 0x20;
    h.monitor_block_offset_in_thread = 0x28;
    h.jit_stack_floor_offset_in_thread = 0x30;
    h.ref_store_post_skip_mask = 0;
    h
}

/// What one compile produced: did a body come out, did it self-lock, and the
/// census rows the compile counted.
pub(crate) struct Outcome {
    pub(crate) compiled: bool,
    pub(crate) self_locks: bool,
    pub(crate) published: u64,
    pub(crate) handler_exits: u64,
    pub(crate) mode_exits: u64,
    pub(crate) blocks_shape: u64,
    /// The body's machine-code length (`0`: none); read by lane sync5's tests.
    pub(crate) code_len: usize,
}

pub(crate) fn compile(cached: &CachedBytecodeMethod, edits: &[(&str, Option<&str>)]) -> Outcome {
    let helpers = wired_helpers();
    // cp 2 `a:I`, 4 `b:I` / `caught:I`, 6 `other:Ljava/lang/Object;`, 8 `o:I`.
    let fields = |cp: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
        match cp {
            2 => Some((0, b'I', None)),
            4 => Some((1, b'I', None)),
            6 => Some((2, b'L', None)),
            8 => Some((3, b'I', None)),
            _ => None,
        }
    };
    // cp 3 `static helper(I)I`, 9 `helper2(I)I`.
    let invokes = |cp: u16| -> Option<(String, String, String)> {
        match cp {
            3 => Some(("R16SyncDoorCalls$C".into(), "helper".into(), "(I)I".into())),
            9 => Some(("R16SyncDoorCalls$C".into(), "helper2".into(), "(I)I".into())),
            // Lane sync5: a recursive self-call (`rec`).
            10 => Some(("R16SyncDoorCalls$C".into(), "rec".into(), "(I)I".into())),
            _ => None,
        }
    };
    let classes = |cp: u16| -> Option<String> {
        (cp == 5).then(|| "java/lang/ArithmeticException".to_string())
    };
    let direct = crate::DirectHelperTable {
        monitor_enter: 0x5000_0000,
        monitor_exit: 0x5000_0040,
        ..crate::DirectHelperTable::EMPTY
    };
    let verdicts = crate::JitVerdictRegistry::default();
    let retries = crate::jfr_compile_decision::DeferredNewRetries::default();
    let mut req = crate::compile_request::CompileRequest::new(
        cached,
        &helpers,
        crate::compile_request::CompileRealm {
            verdicts: &verdicts,
            deferred_new_retries: &retries,
            jdk_only_sinks: crate::jdk_only_sink::process_jdk_only_sinks(),
        },
    );
    req.cp_class_name_resolver = Some(&classes);
    req.cp_field_resolver = Some(&fields);
    req.cp_invoke_resolver = Some(&invokes);
    req.direct_helpers = &direct;
    // Lane sync5: the loader identity a self-call needs (only `rec` has one).
    req.self_call_identity_stable = true;
    let out =
        cratonvm_types::flags::with_thread_overrides(edits, || crate::try_compile_request(&req));
    let census = verdicts.self_lock_census();
    Outcome {
        compiled: out.is_some(),
        self_locks: out.as_ref().is_some_and(|c| c.self_locks_monitor),
        published: census.get(R::BodyPublished),
        handler_exits: census.get(R::BodyRefusedHandlerExits),
        mode_exits: census.get(R::BodyRefusedHandlerModeExits),
        blocks_shape: census.get(R::BodyRefusedBlocksShape),
        code_len: out.as_ref().map_or(0, |c| c.code_len()),
    }
}

pub(crate) fn row(
    start: u16,
    end: u16,
    handler: u16,
    catch: u16,
) -> cratonvm_reader::attribute::ExceptionTableEntry {
    cratonvm_reader::attribute::ExceptionTableEntry {
        start_pc: start,
        end_pc: end,
        handler_pc: handler,
        catch_type: catch,
    }
}

/// `synchronized int guarded(int x) { try { a += helper(x); return a; }
/// catch (ArithmeticException e) { caught++; return -1; } }`; the first 18
/// bytes alone (`a += helper(x); return a;`) are the table-free control.
const GUARDED: [u8; 31] = [
    0x2a, 0x59, 0xb4, 0x00, 0x02, 0x1b, 0xb8, 0x00, 0x03, 0x60, 0xb5, 0x00, 0x02, 0x2a, 0xb4, 0x00,
    0x02, 0xac, 0x4d, 0x2a, 0x59, 0xb4, 0x00, 0x04, 0x04, 0x60, 0xb5, 0x00, 0x04, 0x02, 0xac,
];

/// `synchronized int thrower(int x) { try { a += helper(x); } finally { b++; }
/// return a; }`: a catch-any row.
const THROWER: [u8; 44] = [
    0x2a, 0x59, 0xb4, 0x00, 0x02, 0x1b, 0xb8, 0x00, 0x03, 0x60, 0xb5, 0x00, 0x02, 0x2a, 0x59, 0xb4,
    0x00, 0x04, 0x04, 0x60, 0xb5, 0x00, 0x04, 0xa7, 0x00, 0x10, 0x4d, 0x2a, 0x59, 0xb4, 0x00, 0x04,
    0x04, 0x60, 0xb5, 0x00, 0x04, 0x2c, 0xbf, 0x2a, 0xb4, 0x00, 0x02, 0xac,
];

/// `synchronized int blocked(int x) { synchronized (other) { o += helper2(x); }
/// return o; }`: a structured block and javac's two catch-any rows.
const BLOCKED: [u8; 36] = [
    0x2a, 0xb4, 0x00, 0x06, 0x59, 0x4d, 0xc2, 0x2a, 0x59, 0xb4, 0x00, 0x08, 0x2a, 0x1b, 0xb6, 0x00,
    0x09, 0x60, 0xb5, 0x00, 0x08, 0x2c, 0xc3, 0xa7, 0x00, 0x08, 0x4e, 0x2c, 0xc3, 0x2d, 0xbf, 0x2a,
    0xb4, 0x00, 0x08, 0xac,
];

fn plain() -> CachedBytecodeMethod {
    method("plain", &GUARDED[..18], vec![], 3, 2)
}

/// The three table shapes of `R16SyncDoorCalls`.
fn table_shapes() -> Vec<CachedBytecodeMethod> {
    vec![
        method("guarded", &GUARDED, vec![row(0, 17, 18, 5)], 3, 3),
        method("thrower", &THROWER, vec![row(0, 13, 26, 0)], 3, 3),
        method(
            "blocked",
            &BLOCKED,
            vec![row(7, 23, 26, 0), row(26, 29, 26, 0)],
            4,
            4,
        ),
    ]
}

/// The diagnosis: by default each table shape compiles, but WITHOUT its
/// self-locking body, refused for its mode exits alone (the census names it);
/// the table-free control self-locks.
#[test]
#[cfg(target_arch = "x86_64")]
fn table_bodies_are_refused_only_for_their_mode_exits_by_default() {
    // The switch is default ON since JIT round 19; its kill word restores the
    // diagnosed (pre-fix) default.
    let off = [(SWITCH, Some("0"))];
    let p = compile(&plain(), &off);
    assert!(
        p.compiled && p.self_locks && p.published == 1,
        "table-free control"
    );
    for m in table_shapes() {
        let o = compile(&m, &off);
        assert!(o.compiled, "{}: the wrapped retry compiles", m.method_name);
        assert!(!o.self_locks, "{}: no self-locking body", m.method_name);
        assert_eq!(o.published, 0, "{}", m.method_name);
        assert_eq!(
            (o.mode_exits, o.handler_exits, o.blocks_shape),
            (1, 0, 0),
            "{}: refused for its mode exits alone",
            m.method_name
        );
    }
}

/// SYNCDOOR3-1: under the switch every table shape gets its self-locking body
/// (the mode exits that name the hold are admitted); the table-free control is
/// unchanged.
#[test]
#[cfg(target_arch = "x86_64")]
fn the_switch_admits_mode_exits_that_hand_the_hold_over() {
    let on = [(SWITCH, Some("1"))];
    let p = compile(&plain(), &on);
    assert!(p.compiled && p.self_locks, "table-free control");
    for m in table_shapes() {
        let o = compile(&m, &on);
        assert!(
            o.compiled && o.self_locks && o.published == 1,
            "{}: self-locking body under the switch",
            m.method_name
        );
    }
}

/// Positive control: with no mode exit planted
/// (`CRATONVM_JIT_SELF_LOCK_MODE_EXIT_HANDOVER=0`) the table shapes self-lock
/// without the switch, which is the round-18 state the `_HANDLERS` /
/// `_BLOCKS` flips were measured in.
#[test]
#[cfg(target_arch = "x86_64")]
fn without_mode_exits_the_table_bodies_self_lock_as_in_round_18() {
    let no_mode_exits = [(MODE_EXIT_HANDOVER, Some("0"))];
    for m in table_shapes() {
        let o = compile(&m, &no_mode_exits);
        assert!(o.self_locks && o.published == 1, "{}", m.method_name);
    }
}

/// Kill control: the switch admits nothing the other rules refuse. With the
/// exceptional hand-over off, the block method is refused its self-locking
/// body (its monitor bytecode is not admitted), and nothing is counted as a
/// mode-exit refusal.
#[test]
#[cfg(target_arch = "x86_64")]
fn the_switch_lifts_no_other_rule() {
    let shapes = table_shapes();
    assert!(
        !shapes.is_empty(),
        "table_shapes names the blocked method last"
    );
    let blocked = &shapes[shapes.len() - 1];
    let o = compile(blocked, &[(SWITCH, Some("1")), (EXC_HANDOVER, Some("0"))]);
    assert!(o.compiled && !o.self_locks);
    assert_eq!((o.published, o.mode_exits), (0, 0));
}
