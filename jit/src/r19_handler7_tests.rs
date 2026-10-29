// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 19 wave 7, lane handler7: why the compiled handler entry
//! (`CRATONVM_JIT_IR_COMPILED_HANDLER_ENTRY`) never engaged on
//! `R19Handler5Catch` / `R19Handler6Catch`, pinned end to end through
//! [`crate::try_compile_request`] on javac's bytecode of the probe kernels
//! (every helper a fake address; the code is never run).

use crate::r19_syncdoor3_tests::wired_helpers;
use crate::{CachedBytecodeMethod, CompiledMethod};
use std::sync::Arc;

const ENTRY: &str = "CRATONVM_JIT_IR_COMPILED_HANDLER_ENTRY";
const SINK: &str = "CRATONVM_JIT_IR_CATCH_ARM_SINK_RESULT_USERS";

/// `static long kVoid(int n) { long c = 0; for (int i = 0; i < n; i++) { try
/// { fVoid(i); } catch (Bad e) { c++; note(0, e); } } return c; }`,
/// `[9, 13) -> 16, Bad` (cp 7). cp 58 `fVoid(I)V`, cp 62 `note(ILThrowable;)V`.
pub(crate) const KVOID: [u8; 36] = [
    0x09, 0x40, 0x03, 0x3e, 0x1d, 0x1a, 0xa2, 0x00, 0x1c, 0x1d, 0xb8, 0x00, 0x3a, 0xa7, 0x00, 0x0f,
    0x3a, 0x04, 0x1f, 0x0a, 0x61, 0x40, 0x03, 0x19, 0x04, 0xb8, 0x00, 0x3e, 0x84, 0x03, 0x01, 0xa7,
    0xff, 0xe5, 0x1f, 0xad,
];

/// `static long kValue(int n)`: `s += fValue(i)` (cp 66 `fValue(I)I`),
/// `[13, 22) -> 25, Bad`; ends `return s * 31 + c` (cp 70 the long 31).
pub(crate) const KVALUE: [u8; 51] = [
    0x09, 0x40, 0x09, 0x42, 0x03, 0x36, 0x05, 0x15, 0x05, 0x1a, 0xa2, 0x00, 0x21, 0x1f, 0x15, 0x05,
    0xb8, 0x00, 0x42, 0x85, 0x61, 0x40, 0xa7, 0x00, 0x0f, 0x3a, 0x06, 0x21, 0x0a, 0x61, 0x42, 0x04,
    0x19, 0x06, 0xb8, 0x00, 0x3e, 0x84, 0x05, 0x01, 0xa7, 0xff, 0xdf, 0x1f, 0x14, 0x00, 0x46, 0x69,
    0x21, 0x61, 0xad,
];

pub(crate) fn kernel(
    name: &str,
    code: &[u8],
    table: Vec<cratonvm_reader::attribute::ExceptionTableEntry>,
    max_locals: u16,
) -> CachedBytecodeMethod {
    let mut m = crate::r19_syncdoor3_tests::method(name, code, table, 4, max_locals);
    m.class_name = Arc::from("R19Handler6Catch");
    m.method_descriptor = Arc::from("(I)J");
    m.is_synchronized = false;
    m.is_static = true;
    m
}

pub(crate) fn bad_row(
    start: u16,
    end: u16,
    handler: u16,
) -> cratonvm_reader::attribute::ExceptionTableEntry {
    crate::r19_syncdoor3_tests::row(start, end, handler, 7)
}

/// One optimizing compile of `cached` (the request the C1->C2 supersede door
/// makes: `optimize` and the default-ON IR value/call gates).
pub(crate) fn compile_c2(
    cached: &CachedBytecodeMethod,
    edits: &[(&str, Option<&str>)],
) -> Option<CompiledMethod> {
    // Only the helpers the IR call/handler lowering names; every other slot
    // zero (a nonzero fake can be read as a table address by the lowering).
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`,
    // so all-zero is a valid value; the body is never run.
    let mut helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    helpers.invoke_dispatch = 0x1111_2222_3333_4440;
    helpers.dispatch_threw = 0x1111_2222_3333_4442;
    helpers.service_callee_deopt = 0x1111_2222_3333_4443;
    helpers.local_handler_lookup = 0x1111_2222_3333_4450;
    let _ = wired_helpers;
    let invokes = |cp: u16| -> Option<(String, String, String)> {
        match cp {
            58 => Some(("R19Handler6Catch".into(), "fVoid".into(), "(I)V".into())),
            62 => Some((
                "R19Handler6Catch".into(),
                "note".into(),
                "(ILjava/lang/Throwable;)V".into(),
            )),
            66 => Some(("R19Handler6Catch".into(), "fValue".into(), "(I)I".into())),
            _ => None,
        }
    };
    let classes =
        |cp: u16| -> Option<String> { (cp == 7).then(|| "R19Handler6Catch$Bad".to_string()) };
    let ldc2w = |cp: u16| -> Option<(i64, bool)> { (cp == 70).then_some((31, false)) };
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
    req.cp_invoke_resolver = Some(&invokes);
    req.cp_ldc2w_resolver = Some(&ldc2w);
    req.optimize = true;
    req.ir_emit_calls = true;
    req.ir_emit_special_calls = true;
    req.ir_emit_long = true;
    req.ir_emit_virtual_calls = true;
    req.ir_emit_fp = true;
    cratonvm_types::flags::with_thread_overrides(edits, || crate::try_compile_request(&req))
}

/// The supersede door's own predicate: the probe kernels ARE eligible for
/// the optimizing tier by bytecode.
#[test]
fn the_probe_kernels_pass_the_c2_upgrade_scan() {
    let mut code = KVOID.to_vec();
    code.extend_from_slice(&[0, 0]);
    assert!(crate::c2_upgrade_would_engage(
        &code,
        KVOID.len(),
        "(I)J",
        true,
        true,
        true
    ));
    let mut code = KVALUE.to_vec();
    code.extend_from_slice(&[0, 0]);
    assert!(crate::c2_upgrade_would_engage(
        &code,
        KVALUE.len(),
        "(I)J",
        true,
        true,
        true
    ));
}

/// `(published an optimizing body, compiled handler probes wired)`.
fn outcome(cm: Option<CompiledMethod>) -> (bool, usize) {
    cm.map_or((false, 0), |cm| {
        (cm.used_ir_backend, cm._jit_local_handler_sites.len())
    })
}

/// Once a kernel DOES reach the optimizing tier, nothing downstream refuses
/// it: under the default (evidence) acceptance gate the body publishes in
/// every arm, and the probe is wired exactly where waves 5 and 6 designed it:
/// `kVoid` (value unused) with the entry switch alone, `kValue` (`s += f(i)`)
/// only with the result-user split (without it the census names
/// `tail-after-call`). So the wave-6 flat A/B was the tiering (the kernels
/// never got here; `jit-r19-handler7-proposals-RETIRED-20261009.md` section 1), not the arms.
#[test]
fn the_probe_kernels_wire_their_probe_once_they_reach_the_optimizing_tier() {
    let v = kernel("kVoid", &KVOID, vec![bad_row(9, 13, 16)], 5);
    assert_eq!(outcome(compile_c2(&v, &[])), (true, 0), "kVoid, arm A");
    assert_eq!(
        outcome(compile_c2(&v, &[(ENTRY, Some("1"))])),
        (true, 1),
        "kVoid, arm B"
    );
    let k = kernel("kValue", &KVALUE, vec![bad_row(13, 22, 25)], 7);
    // JIT round 23 wave 3b (lane L1, `CRATONVM_JIT_IR_LOOP_CARRIED_REGS`,
    // default ON): the production pipeline's equal-depth sink (integers and
    // longs, never FP) moves `s += fValue(i)`'s result users (`i2l`, `ladd`)
    // out of the call's block into its normal successor -- the placement the
    // result-user split of arm C makes, held to the same dominance check
    // (`ir_schedule::placement_is_sound`) -- so `catch_arm_wire`'s
    // `tail-after-call` clause no longer applies and the probe is wired with
    // the entry switch alone. With the mark killed, arm B refuses as it did.
    assert_eq!(
        outcome(compile_c2(
            &k,
            &[(ENTRY, Some("1")), ("CRATONVM_JIT_IR_LOOP_CARRIED_REGS", Some("0"))]
        )),
        (true, 0),
        "kValue, arm B without the loop-carried mark: the result user after the call refuses the wire"
    );
    assert_eq!(
        outcome(compile_c2(&k, &[(ENTRY, Some("1"))])),
        (true, 1),
        "kValue, arm B: the loop-carried sink moved the result users off the call's block"
    );
    assert_eq!(
        outcome(compile_c2(&k, &[(ENTRY, Some("1")), (SINK, Some("1"))])),
        (true, 1),
        "kValue, arm C"
    );
}

/// SYNCDOOR4-3 (`CRATONVM_JIT_BC_CALLEE_STAND_DOWN_MEMO`): a recorded hit is
/// tagged, so no miss reader (`jit_probe_is_current`) ever takes it for a
/// miss at any generation, and only a hit recorded under the switch carries
/// the stand-down tag the bytecode-callee doors stand down on.
#[test]
fn a_found_probe_memo_never_reads_as_a_miss() {
    use cratonvm_jit_api::{JIT_PROBE_FOUND_TAG, JIT_PROBE_STAND_DOWN_TAG};
    let m = kernel("kVoid", &KVOID, vec![bad_row(9, 13, 16)], 5);
    m.record_jit_probe_miss(5);
    assert!(m.jit_probe_is_current(5));
    m.record_jit_probe_found(5, false);
    assert!(!m.jit_probe_is_current(5), "a hit is not a miss");
    assert_eq!(m.jit_probe_word(), 5 | JIT_PROBE_FOUND_TAG);
    m.record_jit_probe_found(5, true);
    assert_eq!(
        m.jit_probe_word(),
        5 | JIT_PROBE_FOUND_TAG | JIT_PROBE_STAND_DOWN_TAG
    );
    assert!(!m.jit_probe_is_current(5));
    assert!(!m.jit_probe_is_current(6), "a newer generation re-probes");
    m.record_jit_probe_miss(6);
    assert!(m.jit_probe_is_current(6));
}
