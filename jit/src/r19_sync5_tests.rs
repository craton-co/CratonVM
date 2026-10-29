// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 19 wave 5, lane sync5: nested `synchronized` blocks in a
//! self-locking body, and the recursive self-call of a synchronized instance
//! method (SYNCDOOR4-2 of `docs/feature-designs/jit-ir-optimizations.md`),
//! compiled end to end through [`crate::try_compile_request`] with every
//! helper a fake address (the code is never run; the syncdoor3 harness's
//! helpers).
//!
//! The bytecode is javac 25's, byte for byte, of the probe
//! `C:\craton\jr19-ssh\probes\R19Sync5Nest.java` (`R19Sync5Nest$N` /
//! `R19Sync5Nest$R`), with its constant-pool indices; the resolvers below
//! answer those indices as the VM's would.

use crate::r19_syncdoor3_tests::{wired_helpers, Outcome};
use crate::self_lock_census::SelfLockRow as R;
use crate::CachedBytecodeMethod;
use cratonvm_reader::attribute::ExceptionTableEntry;
use std::sync::Arc;

const SELF_CALL: &str = "CRATONVM_JIT_SELF_LOCK_SELF_CALL";
const HANDLERS: &str = "CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS";
const RETRY: &str = "CRATONVM_JIT_SELF_LOCK_BUFFER_RETRY";

fn row(start: u16, end: u16, handler: u16, catch: u16) -> ExceptionTableEntry {
    ExceptionTableEntry {
        start_pc: start,
        end_pc: end,
        handler_pc: handler,
        catch_type: catch,
    }
}

fn method(
    class: &str,
    name: &str,
    code: &[u8],
    table: Vec<ExceptionTableEntry>,
    max_stack: u16,
    max_locals: u16,
) -> CachedBytecodeMethod {
    CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(8),
        class_name: Arc::from(class),
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

/// One compile of `cached` (a method of the probe's `$N` or `$R`) under
/// `edits`. Each test names its own class (`r19sync5/<Test>$N`), so the
/// process-global code-buffer hint (keyed by method) starts fresh per test.
fn compile(cached: &CachedBytecodeMethod, edits: &[(&str, Option<&str>)]) -> Outcome {
    let helpers = wired_helpers();
    let cls = cached.class_name.to_string();
    let in_n = cls.ends_with("$N");
    let fields = move |cp: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
        if in_n {
            match cp {
                7 => Some((0, b'L', None)),  // a
                13 => Some((1, b'L', None)), // bb
                16 => Some((2, b'I', None)), // b
                20 => Some((3, b'I', None)), // o
                33 | 42 | 55 | 67 | 76 => Some((4 + usize::from(cp % 5), b'I', None)),
                _ => None,
            }
        } else {
            match cp {
                7 => Some((0, b'I', None)),  // b
                19 => Some((1, b'I', None)), // depthHeld
                31 => Some((2, b'I', None)), // caught
                34 => Some((3, b'L', None)), // peer
                _ => None,
            }
        }
    };
    let statics = move |cp: u16| -> Option<(u32, usize, u8, bool)> {
        match (in_n, cp) {
            (true, 36) | (false, 44) => Some((8, 0, b'L', false)), // N.BOOM
            (true, 45) => Some((9, 0, b'L', false)),               // SENTINEL_LOCK
            (true, 58) => Some((8, 1, b'L', false)),               // N.shape
            _ => None,
        }
    };
    let own = cls.clone();
    let invokes = move |cp: u16| -> Option<(String, String, String)> {
        let m = |c: &str, n: &str, d: &str| Some((c.to_string(), n.to_string(), d.to_string()));
        let rc = own.as_str();
        match (in_n, cp) {
            (true, 23) => m(rc, "helper", "(I)I"),
            (true, 27) | (false, 13) => m("java/lang/Thread", "holdsLock", "(Ljava/lang/Object;)Z"),
            (true, 50) => m("java/lang/Object", "notify", "()V"),
            (true, 62) => m("R19Sync5Nest$Shape", "area", "(I)I"),
            (false, 22) => m(rc, "helper", "(I)I"),
            (false, 26) => m(rc, "rec", "(I)I"),
            (false, 38) => m(rc, "recOther", "(I)I"),
            (false, 50) => m(rc, "recThrow", "(I)I"),
            _ => None,
        }
    };
    // `R` is final: the VM pins its `invokevirtual`s to the declaring class
    // (the owner resolver answers the private/final pin).
    let pinned = cls.clone();
    let owner = move |cp: u16, opcode: u8| -> Option<(String, u32)> {
        (!in_n && opcode == 0xb6 && matches!(cp, 22 | 26 | 38 | 50)).then(|| (pinned.clone(), 8))
    };
    let classes = move |cp: u16| -> Option<String> {
        match (in_n, cp) {
            (true, 40) | (false, 29) => Some("java/lang/ArithmeticException".into()),
            (true, 53) => Some("java/lang/IllegalMonitorStateException".into()),
            (true, 28) | (false, 14) => Some("java/lang/Thread".into()),
            _ => None,
        }
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
    req.cp_static_field_resolver = Some(&statics);
    req.cp_invoke_resolver = Some(&invokes);
    req.cp_invokespecial_owner_resolver = Some(&owner);
    req.direct_helpers = &direct;
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

// ---- javac 25's bytecode of the probe's methods (`R19Sync5Nest$N`) ----

// nestSame(I)I max_stack=4 max_locals=6
const NESTSAME: [u8; 57] = [
    0x2a, 0xb4, 0x00, 0x07, 0x59, 0x4d, 0xc2, 0x2a, 0xb4, 0x00, 0x07, 0x59, 0x4e, 0xc2, 0x2a, 0x59,
    0xb4, 0x00, 0x14, 0x2a, 0x1b, 0xb6, 0x00, 0x17, 0x60, 0xb5, 0x00, 0x14, 0x2d, 0xc3, 0xa7, 0x00,
    0x0a, 0x3a, 0x04, 0x2d, 0xc3, 0x19, 0x04, 0xbf, 0x2c, 0xc3, 0xa7, 0x00, 0x0a, 0x3a, 0x05, 0x2c,
    0xc3, 0x19, 0x05, 0xbf, 0x2a, 0xb4, 0x00, 0x14, 0xac,
];
// nestDiff(I)I max_stack=4 max_locals=6
const NESTDIFF: [u8; 57] = [
    0x2a, 0xb4, 0x00, 0x07, 0x59, 0x4d, 0xc2, 0x2a, 0xb4, 0x00, 0x0d, 0x59, 0x4e, 0xc2, 0x2a, 0x59,
    0xb4, 0x00, 0x14, 0x2a, 0x1b, 0xb6, 0x00, 0x17, 0x60, 0xb5, 0x00, 0x14, 0x2d, 0xc3, 0xa7, 0x00,
    0x0a, 0x3a, 0x04, 0x2d, 0xc3, 0x19, 0x04, 0xbf, 0x2c, 0xc3, 0xa7, 0x00, 0x0a, 0x3a, 0x05, 0x2c,
    0xc3, 0x19, 0x05, 0xbf, 0x2a, 0xb4, 0x00, 0x14, 0xac,
];
// nestDeep(I)I max_stack=4 max_locals=8
const NESTDEEP: [u8; 123] = [
    0x2a, 0xb4, 0x00, 0x07, 0x59, 0x4d, 0xc2, 0x2a, 0xb4, 0x00, 0x0d, 0x59, 0x4e, 0xc2, 0x2a, 0x59,
    0x3a, 0x04, 0xc2, 0x2a, 0x59, 0xb4, 0x00, 0x14, 0x2a, 0x1b, 0xb6, 0x00, 0x17, 0x60, 0xb5, 0x00,
    0x14, 0x1b, 0x11, 0x0f, 0xff, 0x7e, 0x10, 0x11, 0xa0, 0x00, 0x28, 0x2a, 0xb4, 0x00, 0x07, 0xb8,
    0x00, 0x1b, 0x99, 0x00, 0x1e, 0x2a, 0xb4, 0x00, 0x0d, 0xb8, 0x00, 0x1b, 0x99, 0x00, 0x14, 0x2a,
    0xb8, 0x00, 0x1b, 0x99, 0x00, 0x0d, 0x2a, 0x59, 0xb4, 0x00, 0x21, 0x04, 0x60, 0xb5, 0x00, 0x21,
    0x19, 0x04, 0xc3, 0xa7, 0x00, 0x0b, 0x3a, 0x05, 0x19, 0x04, 0xc3, 0x19, 0x05, 0xbf, 0x2d, 0xc3,
    0xa7, 0x00, 0x0a, 0x3a, 0x06, 0x2d, 0xc3, 0x19, 0x06, 0xbf, 0x2c, 0xc3, 0xa7, 0x00, 0x0a, 0x3a,
    0x07, 0x2c, 0xc3, 0x19, 0x07, 0xbf, 0x2a, 0xb4, 0x00, 0x14, 0xac,
];
// nestThrow(I)I max_stack=4 max_locals=6
const NESTTHROW: [u8; 69] = [
    0x2a, 0xb4, 0x00, 0x07, 0x59, 0x4d, 0xc2, 0x2a, 0xb4, 0x00, 0x0d, 0x59, 0x4e, 0xc2, 0x1b, 0x10,
    0x3f, 0x7e, 0x08, 0xa0, 0x00, 0x07, 0xb2, 0x00, 0x24, 0xbf, 0x2a, 0x59, 0xb4, 0x00, 0x14, 0x2a,
    0x1b, 0xb6, 0x00, 0x17, 0x60, 0xb5, 0x00, 0x14, 0x2d, 0xc3, 0xa7, 0x00, 0x0a, 0x3a, 0x04, 0x2d,
    0xc3, 0x19, 0x04, 0xbf, 0x2c, 0xc3, 0xa7, 0x00, 0x0a, 0x3a, 0x05, 0x2c, 0xc3, 0x19, 0x05, 0xbf,
    0x2a, 0xb4, 0x00, 0x14, 0xac,
];
// nestCatch(I)I max_stack=4 max_locals=6
const NESTCATCH: [u8; 118] = [
    0x2a, 0xb4, 0x00, 0x07, 0x59, 0x4d, 0xc2, 0x2a, 0xb4, 0x00, 0x0d, 0x59, 0x4e, 0xc2, 0x1b, 0x10,
    0x3f, 0x7e, 0x10, 0x09, 0xa0, 0x00, 0x07, 0xb2, 0x00, 0x24, 0xbf, 0x2a, 0x59, 0xb4, 0x00, 0x14,
    0x2a, 0x1b, 0xb6, 0x00, 0x17, 0x60, 0xb5, 0x00, 0x14, 0x2d, 0xc3, 0xa7, 0x00, 0x0a, 0x3a, 0x04,
    0x2d, 0xc3, 0x19, 0x04, 0xbf, 0xa7, 0x00, 0x30, 0x4e, 0x2d, 0xb2, 0x00, 0x24, 0xa6, 0x00, 0x28,
    0x2a, 0xb4, 0x00, 0x07, 0xb8, 0x00, 0x1b, 0x99, 0x00, 0x1e, 0x2a, 0xb4, 0x00, 0x0d, 0xb8, 0x00,
    0x1b, 0x9a, 0x00, 0x14, 0x2a, 0xb8, 0x00, 0x1b, 0x99, 0x00, 0x0d, 0x2a, 0x59, 0xb4, 0x00, 0x2a,
    0x04, 0x60, 0xb5, 0x00, 0x2a, 0x2c, 0xc3, 0xa7, 0x00, 0x0a, 0x3a, 0x05, 0x2c, 0xc3, 0x19, 0x05,
    0xbf, 0x2a, 0xb4, 0x00, 0x14, 0xac,
];
// nestImse(I)I max_stack=4 max_locals=7
const NESTIMSE: [u8; 114] = [
    0x2a, 0xb4, 0x00, 0x07, 0x59, 0x4d, 0xc2, 0x2a, 0xb4, 0x00, 0x0d, 0x59, 0x4e, 0xc2, 0x1b, 0x11,
    0x00, 0xff, 0x7e, 0x06, 0xa0, 0x00, 0x09, 0xb2, 0x00, 0x2d, 0xb6, 0x00, 0x32, 0x2a, 0x59, 0xb4,
    0x00, 0x14, 0x2a, 0x1b, 0xb6, 0x00, 0x17, 0x60, 0xb5, 0x00, 0x14, 0xa7, 0x00, 0x2a, 0x3a, 0x04,
    0x2a, 0xb4, 0x00, 0x07, 0xb8, 0x00, 0x1b, 0x99, 0x00, 0x1e, 0x2a, 0xb4, 0x00, 0x0d, 0xb8, 0x00,
    0x1b, 0x99, 0x00, 0x14, 0x2a, 0xb8, 0x00, 0x1b, 0x99, 0x00, 0x0d, 0x2a, 0x59, 0xb4, 0x00, 0x37,
    0x04, 0x60, 0xb5, 0x00, 0x37, 0x2d, 0xc3, 0xa7, 0x00, 0x0a, 0x3a, 0x05, 0x2d, 0xc3, 0x19, 0x05,
    0xbf, 0x2c, 0xc3, 0xa7, 0x00, 0x0a, 0x3a, 0x06, 0x2c, 0xc3, 0x19, 0x06, 0xbf, 0x2a, 0xb4, 0x00,
    0x14, 0xac,
];
// nestDeopt(I)I max_stack=4 max_locals=6
const NESTDEOPT: [u8; 96] = [
    0x2a, 0xb4, 0x00, 0x07, 0x59, 0x4d, 0xc2, 0x2a, 0xb4, 0x00, 0x0d, 0x59, 0x4e, 0xc2, 0x2a, 0x59,
    0xb4, 0x00, 0x14, 0xb2, 0x00, 0x3a, 0x1b, 0xb6, 0x00, 0x3e, 0x60, 0xb5, 0x00, 0x14, 0x2a, 0xb4,
    0x00, 0x07, 0xb8, 0x00, 0x1b, 0x99, 0x00, 0x1e, 0x2a, 0xb4, 0x00, 0x0d, 0xb8, 0x00, 0x1b, 0x99,
    0x00, 0x14, 0x2a, 0xb8, 0x00, 0x1b, 0x99, 0x00, 0x0d, 0x2a, 0x59, 0xb4, 0x00, 0x43, 0x04, 0x60,
    0xb5, 0x00, 0x43, 0x2d, 0xc3, 0xa7, 0x00, 0x0a, 0x3a, 0x04, 0x2d, 0xc3, 0x19, 0x04, 0xbf, 0x2c,
    0xc3, 0xa7, 0x00, 0x0a, 0x3a, 0x05, 0x2c, 0xc3, 0x19, 0x05, 0xbf, 0x2a, 0xb4, 0x00, 0x14, 0xac,
];

fn nested_shapes(n: &str) -> Vec<CachedBytecodeMethod> {
    let pair = |a: u16, b: u16, c: u16, d: u16| vec![row(a, b, c, 0), row(c, c + 4, c, 0)];
    let two = |inner: (u16, u16, u16), outer: (u16, u16, u16)| {
        let mut t = pair(inner.0, inner.1, inner.2, 0);
        t.extend(pair(outer.0, outer.1, outer.2, 0));
        t
    };
    vec![
        method(
            n,
            "nestSame",
            &NESTSAME,
            two((14, 30, 33), (7, 42, 45)),
            4,
            6,
        ),
        method(
            n,
            "nestDiff",
            &NESTDIFF,
            two((14, 30, 33), (7, 42, 45)),
            4,
            6,
        ),
        method(
            n,
            "nestDeep",
            &NESTDEEP,
            vec![
                row(19, 83, 86, 0),
                row(86, 91, 86, 0),
                row(14, 96, 99, 0),
                row(99, 103, 99, 0),
                row(7, 108, 111, 0),
                row(111, 115, 111, 0),
            ],
            4,
            8,
        ),
        method(
            n,
            "nestThrow",
            &NESTTHROW,
            two((14, 42, 45), (7, 54, 57)),
            4,
            6,
        ),
        method(
            n,
            "nestCatch",
            &NESTCATCH,
            vec![
                row(14, 43, 46, 0),
                row(46, 50, 46, 0),
                row(7, 53, 56, 40),
                row(7, 103, 106, 0),
                row(106, 110, 106, 0),
            ],
            4,
            6,
        ),
        method(
            n,
            "nestImse",
            &NESTIMSE,
            vec![
                row(14, 43, 46, 53),
                row(14, 87, 90, 0),
                row(90, 94, 90, 0),
                row(7, 99, 102, 0),
                row(102, 106, 102, 0),
            ],
            4,
            7,
        ),
        method(
            n,
            "nestDeopt",
            &NESTDEOPT,
            two((14, 69, 72), (7, 81, 84)),
            4,
            6,
        ),
    ]
}

// ---- `R19Sync5Nest$R` ----

// rec(I)I max_stack=3 max_locals=3
const REC: [u8; 50] = [
    0x1b, 0x9a, 0x00, 0x1a, 0x2a, 0xb8, 0x00, 0x0d, 0x99, 0x00, 0x0d, 0x2a, 0x59, 0xb4, 0x00, 0x13,
    0x04, 0x60, 0xb5, 0x00, 0x13, 0x2a, 0x04, 0xb6, 0x00, 0x16, 0xac, 0x2a, 0x1b, 0x04, 0x64, 0xb6,
    0x00, 0x1a, 0x04, 0x60, 0xac, 0x4d, 0x2a, 0x59, 0xb4, 0x00, 0x1f, 0x04, 0x60, 0xb5, 0x00, 0x1f,
    0x02, 0xac,
];
// recOther(I)I max_stack=3 max_locals=3
const RECOTHER: [u8; 63] = [
    0x1b, 0x9a, 0x00, 0x24, 0x2a, 0xb8, 0x00, 0x0d, 0x99, 0x00, 0x17, 0x2a, 0xb4, 0x00, 0x22, 0xb8,
    0x00, 0x0d, 0x99, 0x00, 0x0d, 0x2a, 0x59, 0xb4, 0x00, 0x13, 0x04, 0x60, 0xb5, 0x00, 0x13, 0x2a,
    0x04, 0xb6, 0x00, 0x16, 0xac, 0x2a, 0xb4, 0x00, 0x22, 0x1b, 0x04, 0x64, 0xb6, 0x00, 0x26, 0x04,
    0x60, 0xac, 0x4d, 0x2a, 0x59, 0xb4, 0x00, 0x1f, 0x04, 0x60, 0xb5, 0x00, 0x1f, 0x02, 0xac,
];
// recThrow(I)I max_stack=3 max_locals=2 (no table)
const RECTHROW: [u8; 18] = [
    0x1b, 0x9a, 0x00, 0x07, 0xb2, 0x00, 0x2c, 0xbf, 0x2a, 0x1b, 0x04, 0x64, 0xb6, 0x00, 0x32, 0x04,
    0x60, 0xac,
];

fn rec_shapes(rc: &str) -> Vec<CachedBytecodeMethod> {
    vec![
        method(
            rc,
            "rec",
            &REC,
            vec![row(0, 26, 37, 29), row(27, 36, 37, 29)],
            3,
            3,
        ),
        method(
            rc,
            "recOther",
            &RECOTHER,
            vec![row(0, 36, 50, 29), row(37, 49, 50, 29)],
            3,
            3,
        ),
        method(rc, "recThrow", &RECTHROW, vec![], 3, 2),
    ]
}

/// SYNC5-1, the diagnosis: the self-locking body of javac's two-level nested
/// blocks overflows its code-buffer estimate on the FIRST compile, the wrapped
/// retry fits, and that compile publishes a wrapped body (and, in the VM,
/// memoizes a self-lock refusal for good). A second compile, at the size the
/// first measured, self-locks. Three levels and the shapes with a typed catch
/// or a static field happen to fit their (larger-bytecode) estimate.
#[test]
#[cfg(target_arch = "x86_64")]
fn a_nested_body_first_overflows_its_buffer_estimate_without_the_switch() {
    for m in nested_shapes("r19sync5/Diag$N") {
        let first = compile(&m, &[(RETRY, Some("0"))]);
        let second = compile(&m, &[(RETRY, Some("0"))]);
        assert!(first.compiled && second.compiled, "{}", m.method_name);
        assert!(
            second.self_locks,
            "{}: self-locks at the measured size",
            m.method_name
        );
        if matches!(&*m.method_name, "nestSame" | "nestDiff" | "nestThrow") {
            assert!(
                !first.self_locks && first.published == 0,
                "{}: the first compile publishes the wrapped body",
                m.method_name
            );
        }
    }
}

/// SYNC5-1: under `CRATONVM_JIT_SELF_LOCK_BUFFER_RETRY` the FIRST compile of
/// every nested shape (same monitor, two monitors, three levels with `this`,
/// a throw from the inner block, a typed catch between the levels, an IMSE
/// caught inside, a virtual call inside) publishes its self-locking body.
#[test]
#[cfg(target_arch = "x86_64")]
fn the_switch_self_locks_every_nested_shape_on_its_first_compile() {
    for m in nested_shapes("r19sync5/Retry$N") {
        let o = compile(&m, &[(RETRY, Some("1"))]);
        assert!(
            o.compiled && o.self_locks && o.published == 1,
            "{}: self-locking body on the first compile",
            m.method_name
        );
    }
}

/// SYNCDOOR4-2: under `CRATONVM_JIT_SELF_LOCK_SELF_CALL` the recursive
/// self-call of a synchronized instance method (with a table, on another
/// receiver, a throw from the deepest level) is a direct self-call in the
/// self-locking body (a different body than the dispatch one), and the body
/// still self-locks.
#[test]
#[cfg(target_arch = "x86_64")]
fn the_self_call_switch_calls_the_self_locking_entry() {
    for m in rec_shapes("r19sync5/SelfCall$R") {
        let off = compile(&m, &[(SELF_CALL, Some("0"))]);
        let on = compile(&m, &[(SELF_CALL, Some("1"))]);
        assert!(off.self_locks && on.self_locks, "{}", m.method_name);
        assert_ne!(
            off.code_len, on.code_len,
            "{}: the self-call arm",
            m.method_name
        );
    }
}

/// SYNCDOOR4-2, the monitor half: when the self-locking body is refused (the
/// handlers switch off: a table body may not self-lock), the wrapped retry
/// DISPATCHES the self-call -- its entry takes no monitor -- so its body is
/// the switch-off body, length for length. Without `unbind_sync_self_calls`
/// it would CALL its own unlocking entry.
#[test]
#[cfg(target_arch = "x86_64")]
fn a_refused_self_lock_dispatches_its_self_call() {
    for m in rec_shapes("r19sync5/Unbind$R").into_iter().take(2) {
        let off = compile(&m, &[(SELF_CALL, Some("0")), (HANDLERS, Some("0"))]);
        let on = compile(&m, &[(SELF_CALL, Some("1")), (HANDLERS, Some("0"))]);
        assert!(off.compiled && on.compiled, "{}", m.method_name);
        assert!(
            !off.self_locks && !on.self_locks,
            "{}: wrapped",
            m.method_name
        );
        assert_eq!(
            off.code_len, on.code_len,
            "{}: dispatched, not called",
            m.method_name
        );
    }
}

#[test]
fn the_new_switches_default_off() {
    cratonvm_types::flags::with_thread_overrides(
        &[(SELF_CALL, Some("0")), (RETRY, Some("0"))],
        || {
            assert!(!crate::self_lock_self_call_enabled());
            assert!(!crate::self_lock_buffer_retry_enabled());
        },
    );
    cratonvm_types::flags::with_thread_overrides(
        &[(SELF_CALL, Some("1")), (RETRY, Some("1"))],
        || {
            assert!(crate::self_lock_self_call_enabled());
            assert!(crate::self_lock_buffer_retry_enabled());
        },
    );
}
