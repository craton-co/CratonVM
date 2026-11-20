// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The bare `is_jdk_only()` gate census (interpreter round i6 wave 122, lane
//! L5; the census test of
//! `docs/feature-designs/native-dispatch-architecture.md`).
//!
//! A "bare gate" is a call of `is_jdk_only()` (on a `NativeContext`, a
//! `VmConfig`, a `CompatMode`) that decides behaviour by the mode itself,
//! rather than through a `HotSpotAlignment` row (`alignment_enabled(..)` and
//! friends, which this scan never matches). Each one is a place where
//! `--compatible` may still answer other than HotSpot, or a piece of the
//! strict policy itself (the `SyntheticStub` refusal, the retired-shadow
//! masks). Nobody could state the size of that surface; this census does.
//!
//! The census is a SOURCE scan (host- and feature-independent): every
//! `is_jdk_only()` in `native-builtins/src`, `native-collections/src`,
//! `native-io/src` and `vm/src`, not preceded by an identifier character (so
//! `config_is_jdk_only()` is not counted) and outside `//` comment lines and
//! trailing comments. A definition (`fn is_jdk_only(&self)`) does not match:
//! it has a parameter. [`FROZEN`] holds it per file with zero slack, so a new
//! gate (or a gate turned into an alignment row) is a named line in the diff.
//! Re-freeze by pasting the printed literal (`cargo test -p
//! cratonvm-native-builtins --test jdk_only_gate_census -- --nocapture`), and
//! say on the i49-L6 page why the count moved. A wave that aligns a gate
//! lowers it; a wave that adds one says why it is strict policy or a
//! divergence with a row to come.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `(file, bare gates)`, sorted: the census at the wave-122 freeze (257 gates
/// in 59 files), re-frozen in wave 124 at 258 (`lang_invoke.rs` 3 -> 4: the
/// wave-123 `CRATONVM_VH_REAL_VARFORM` guard; see the i49-L6 page), and
/// re-frozen in JIT round 22 wave 3 (lane review) at 263, the count `dev`
/// `e82bd964e0` already held (rounds 21-22 add none): `vm/src/jit/helpers.rs`
/// 32 -> 34 (COMPAT21-1/2 `thin_helper_answer_settled` and
/// `thin_helper_resolver_yields`: the opt-in `CRATONVM_PURE_BYTECODE` probe's
/// `Intrinsic` arm, which `resolve_dispatch` applies under `--real-jdk` only --
/// strict policy, mirrored); `vm/src/runtime/interpreter/native_override.rs`
/// 3 -> 4 (COMPAT21-4 `forced_native_would_run`, the same probe arm);
/// `vm/src/runtime/interpreter/gc_and_alloc.rs` 8 -> 9 (a test assertion,
/// `queue_wake_ups_follow_the_vms_own_policy`'s `assert!(!..is_jdk_only())`,
/// which the source scan counts). No `--compatible` divergence is added.
/// Re-frozen in JIT round 23 wave 2 (lane L6) at 262: `vm/src/jit/helpers.rs`
/// 34 -> 33. SYNC22-3's OSR-door refusal reads the mode through the door's
/// existing `DirectCallPolicy` (`admit_direct_native_entry` now reads it once
/// for its three questions, one read fewer than before), and its test asserts
/// no mode; the new `--compatible` refusals go through
/// `direct_native_bind_admitted`'s existing gate.
/// Re-frozen in JIT round 23 wave 8 (lane W8-L2) at 263: `vm/src/jit/helpers.rs`
/// 33 -> 34. `CRATONVM_JIT_GATE_POLICY_HALF_MEMO`'s `classified_gate_refuses_split`
/// asks the VM's own `dispatch_policy(vm).is_jdk_only()` once, to decide whether
/// the per-VM gate memo may store the policy half of a capability-classified
/// helper gate; `--compatible` returns before it exactly as before, so no
/// `--compatible` divergence is added.
/// Re-frozen in zt t31 at 258 (on the merged tree, `dev` `ec551c685e`): `dev` itself
/// had moved 263 -> 256 (`dispatch_static.rs` 5 -> 6, `gc_and_alloc.rs` 9 -> 4,
/// `vm_exec.rs` 16 -> 15, `memory/roots.rs` and `runtime/hprof.rs` 1 -> 0), and the zt
/// branch adds two in `classloader_real.rs` (6 -> 8): the zt df rounds' jdk-only
/// `BuiltinClassLoader.findClassInModuleOrNull` registration and the
/// `find_loaded_no_materialize` read, both jdk-only by construction (`--compatible`
/// keeps its own loader bodies).
/// Re-frozen at 258 after three `dev` file splits that added and removed no gate:
/// `00529e979f` `vm/src/jit/helpers.rs` 34 -> `vm/src/jit/helpers/*.rs`
/// (`fast_native.rs` 19, `table.rs` 6, `typecheck.rs` 5, `thread.rs` 3,
/// `dispatch.rs` 1); `2733023f27` `native-builtins/src/lang_invoke.rs` 4 ->
/// `lang_invoke/adapters.rs` 2, `member_name.rs` 1, `var_handles.rs` 1;
/// `1f74a55f2b` `native-collections/src/lib.rs` 10 -> `streams.rs` 5,
/// `hash_set.rs` 2, `arrays.rs` 1, `concurrent_hash_map.rs` 1,
/// `linked_hash_map.rs` 1. Pure relocation: no `--compatible` divergence moves.
const FROZEN: &[(&str, usize)] = &[
    (
        "native-builtins/src/classloader/adopt_add_class_orphan_by_id.rs",
        7,
    ),
    (
        "native-builtins/src/classloader/builtin_classloader_find_resource_as.rs",
        3,
    ),
    (
        "native-builtins/src/classloader/closed_url_classloader_ids.rs",
        1,
    ),
    (
        "native-builtins/src/classloader/ucl_try_define_local_class.rs",
        1,
    ),
    ("native-builtins/src/classloader_real.rs", 9),
    ("native-builtins/src/compat_carriers.rs", 1),
    ("native-builtins/src/generics.rs", 3),
    ("native-builtins/src/http_url_connection.rs", 3),
    ("native-builtins/src/jboss_jdkspecific.rs", 4),
    ("native-builtins/src/jca/kem.rs", 1),
    ("native-builtins/src/jca/provider_chain.rs", 2),
    ("native-builtins/src/keystore.rs", 1),
    (
        "native-builtins/src/lang_class/classloader_packages_synced.rs",
        4,
    ),
    ("native-builtins/src/lang_class/dbg_bb_enabled.rs", 2),
    (
        "native-builtins/src/lang_class/declaring_class_loader_aware.rs",
        3,
    ),
    (
        "native-builtins/src/lang_class/descriptor_to_class_mirror_via.rs",
        2,
    ),
    (
        "native-builtins/src/lang_class/executable_access_refusal_chain.rs",
        1,
    ),
    (
        "native-builtins/src/lang_class/forget_lang_class_keys.rs",
        1,
    ),
    (
        "native-builtins/src/lang_class/native_class_is_annotation_present.rs",
        1,
    ),
    (
        "native-builtins/src/lang_class/wf7_synthesise_entry_class_if.rs",
        4,
    ),
    (
        "native-builtins/src/lang_invoke/adapters/mh_direct_lane_enabled.rs",
        2,
    ),
    (
        "native-builtins/src/lang_invoke/member_name/mh_foreign_door_type.rs",
        1,
    ),
    (
        "native-builtins/src/lang_invoke/var_handles/native_record_support.rs",
        1,
    ),
    ("native-builtins/src/lang_math.rs", 2),
    ("native-builtins/src/lang_reflect.rs", 1),
    ("native-builtins/src/lang_string/decode_string_chars.rs", 1),
    ("native-builtins/src/lang_system/shutdown_state.rs", 3),
    ("native-builtins/src/lib_parts/dbg_toarray_enabled.rs", 1),
    (
        "native-builtins/src/lib_parts/essential_natives_shims_e.rs",
        3,
    ),
    (
        "native-builtins/src/lib_parts/native_printwriter_flush.rs",
        2,
    ),
    (
        "native-builtins/src/lib_parts/surefire_channel_can_use.rs",
        5,
    ),
    ("native-builtins/src/lookup_define.rs", 1),
    ("native-builtins/src/net_phase_e/archive_stamp.rs", 3),
    ("native-builtins/src/net_phase_e/header.rs", 1),
    (
        "native-builtins/src/net_phase_e/register_re4_url_http.rs",
        2,
    ),
    (
        "native-builtins/src/panama/register_pe_memory_segment_on.rs",
        3,
    ),
    (
        "native-builtins/src/phases_early/take_inherited_for_thread_object.rs",
        1,
    ),
    ("native-builtins/src/phases_late.rs", 1),
    ("native-builtins/src/phases_late/beans_jndi.rs", 1),
    (
        "native-builtins/src/phases_late/nio_file/p57_line_separator.rs",
        1,
    ),
    (
        "native-builtins/src/phases_late/nio_file/p57_resolve_paths.rs",
        1,
    ),
    (
        "native-builtins/src/phases_late/nio_file/register_phase57_natives.rs",
        4,
    ),
    (
        "native-builtins/src/phases_late/nio_file/register_phase57_nio_file_a.rs",
        1,
    ),
    (
        "native-builtins/src/phases_late/nio_file/win_filetime_from_millis.rs",
        1,
    ),
    ("native-builtins/src/phases_late/reflect_invoke.rs", 1),
    ("native-builtins/src/reference.rs", 1),
    ("native-builtins/src/reflect_annotations.rs", 4),
    ("native-builtins/src/servlet/keep_across.rs", 1),
    ("native-builtins/src/shared_secrets_bridge.rs", 1),
    (
        "native-builtins/src/shared_secrets_bridge/w20f_per_vm_accessor_tests.rs",
        2,
    ),
    ("native-builtins/src/test_frameworks.rs", 1),
    (
        "native-builtins/src/unsafe_natives_ext/unsafe_offset_is_heap_slot.rs",
        1,
    ),
    ("native-collections/src/arrays.rs", 1),
    (
        "native-collections/src/concurrent_hash_map/chm_key_hash.rs",
        1,
    ),
    ("native-collections/src/hash_set.rs", 2),
    ("native-collections/src/linked_hash_map.rs", 1),
    ("native-collections/src/streams/stream_elements.rs", 4),
    ("native-collections/src/streams/stream_is_linked.rs", 1),
    ("native-io/src/lib_parts/register_nio_natives.rs", 1),
    ("native-io/src/lib_parts/wrap_completed_future.rs", 2),
    ("native-io/src/nio_native.rs", 1),
    ("vm/src/config.rs", 3),
    ("vm/src/config/tests.rs", 8),
    ("vm/src/jit/helpers/direct_helper_gate_witness.rs", 2),
    ("vm/src/jit/helpers/dispatch/jit_invoke_dispatch_body.rs", 1),
    (
        "vm/src/jit/helpers/dispatch/mh_constant_filter_compile_fill.rs",
        1,
    ),
    (
        "vm/src/jit/helpers/dispatch/t1w2_site_access_guard_tests.rs",
        1,
    ),
    (
        "vm/src/jit/helpers/fast_native/jit_concurrent_hashmap_get_direct.rs",
        5,
    ),
    (
        "vm/src/jit/helpers/fast_native/jit_scoped_memory_put_int_direct.rs",
        1,
    ),
    ("vm/src/jit/helpers/fast_native/note_ic_profile_seed.rs", 9),
    (
        "vm/src/jit/helpers/fast_native/varhandle_read_direct_counts.rs",
        10,
    ),
    (
        "vm/src/jit/helpers/nio21_w8_direct_helper_gate_sites_tests.rs",
        2,
    ),
    ("vm/src/jit/helpers/table.rs", 3),
    ("vm/src/jit/helpers/thread/class_was_redefined.rs", 2),
    (
        "vm/src/jit/helpers/thread/direct_static_compiled_callee_entry.rs",
        1,
    ),
    ("vm/src/jit/helpers/typecheck.rs", 5),
    ("vm/src/jit/helpers/unsafe_inline_admitted_mask_tests.rs", 1),
    ("vm/src/memory/ref_discovery.rs", 1),
    ("vm/src/runtime/interpreter/constants.rs", 1),
    (
        "vm/src/runtime/interpreter/constants/i9_l2_class_access_tests.rs",
        1,
    ),
    ("vm/src/runtime/interpreter/dispatch_static.rs", 6),
    ("vm/src/runtime/interpreter/dispatch_virtual.rs", 4),
    (
        "vm/src/runtime/interpreter/dispatch_virtual/nio21_w9_interp_native_call_sites_tests.rs",
        2,
    ),
    (
        "vm/src/runtime/interpreter/exception_dispatch/handler_search_tests.rs",
        2,
    ),
    ("vm/src/runtime/interpreter/execute.rs", 3),
    ("vm/src/runtime/interpreter/execute_entry.rs", 1),
    ("vm/src/runtime/interpreter/field_access.rs", 1),
    (
        "vm/src/runtime/interpreter/gc_and_alloc/gc_notifications_go_to_delivery.rs",
        1,
    ),
    (
        "vm/src/runtime/interpreter/gc_and_alloc/w3d_finalizer_thread_tests.rs",
        3,
    ),
    (
        "vm/src/runtime/interpreter/invoke/gci_ju_stackless_walk_tests.rs",
        1,
    ),
    (
        "vm/src/runtime/interpreter/invoke/nio21_w9_two_vm_redefinition_tests.rs",
        2,
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge/byte_array_field_declarer_admitted.rs",
        1,
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge/compile_optimizing_artifact.rs",
        2,
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge/compile_osr_body.rs",
        7,
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge/try_jit_compile_callee_slow.rs",
        1,
    ),
    ("vm/src/runtime/interpreter/jvmti_events.rs", 2),
    ("vm/src/runtime/interpreter/lambda.rs", 1),
    ("vm/src/runtime/interpreter/native_override.rs", 8),
    ("vm/src/runtime/interpreter/opcodes.rs", 1),
    (
        "vm/src/runtime/invokedynamic/descriptor_param_count_and_last_is.rs",
        1,
    ),
    ("vm/src/runtime/resolve/loader_throw.rs", 1),
    (
        "vm/src/vm/vm_exec/extract_annotations_from_attributes.rs",
        2,
    ),
    (
        "vm/src/vm/vm_exec/gci_i6jd_path_tostring_redirect_tests.rs",
        1,
    ),
    ("vm/src/vm/vm_exec/gen_referent_read_keep_alive.rs", 1),
    ("vm/src/vm/vm_exec/invoke_on_class_shared_inner.rs", 2),
    ("vm/src/vm/vm_exec/invoke_special_shared.rs", 2),
    ("vm/src/vm/vm_exec/method.rs", 6),
    ("vm/src/vm/vm_init/missing_native_entry.rs", 2),
    ("vm/src/vm/vm_init/real_jdk_to_array_typed.rs", 7),
    ("vm/src/vm/vm_init/wait_site.rs", 2),
    ("vm/src/vm/vm_init/zgc_relocation_permitted.rs", 4),
    ("vm/src/vm/vm_launcher.rs", 1),
    ("vm/src/vm/vm_object.rs", 5),
    ("vm/src/vm/vm_util.rs", 4),
    ("vm/src/vm/vm_util/tests.rs", 1),
];

/// The crates scanned (their `src` trees).
const CRATES: &[&str] = &["native-builtins", "native-collections", "native-io", "vm"];

const NEEDLE: &str = "is_jdk_only()";

fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The bare gates in one file's `text`.
fn gates_in(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut n = 0;
    while let Some(found) = text[i..].find(NEEDLE) {
        let at = i + found;
        i = at + NEEDLE.len();
        if at > 0 && ident_byte(bytes[at - 1]) {
            continue;
        }
        let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
        if text[line_start..at].contains("//") {
            continue;
        }
        n += 1;
    }
    n
}

fn census() -> BTreeMap<String, usize> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut out = BTreeMap::new();
    for krate in CRATES {
        let base = repo.join(krate);
        let mut files = Vec::new();
        rust_files(&base.join("src"), &mut files);
        assert!(!files.is_empty(), "no sources under {}", base.display());
        for path in files {
            let rel = path
                .strip_prefix(&base)
                .expect("under the crate")
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&path).expect("a source file");
            let text = String::from_utf8_lossy(&bytes);
            let n = gates_in(&text);
            if n > 0 {
                out.insert(format!("{krate}/{rel}"), n);
            }
        }
    }
    out
}

#[test]
fn jdk_only_gate_census_is_frozen() {
    let now = census();
    let total: usize = now.values().sum();
    let mut literal = String::from("const FROZEN: &[(&str, usize)] = &[\n");
    for (file, n) in &now {
        literal.push_str(&format!("    (\"{file}\", {n}),\n"));
    }
    literal.push_str("];\n");
    println!(
        "jdk-only-gate-census: {total} bare gates in {} files",
        now.len()
    );
    println!("{literal}");
    let frozen: BTreeMap<String, usize> = FROZEN.iter().map(|&(f, n)| (f.to_string(), n)).collect();
    if now != frozen {
        let frozen_total: usize = frozen.values().sum();
        let mut diff = vec![format!("total now {total}, frozen {frozen_total}")];
        for (file, n) in &now {
            if frozen.get(file) != Some(n) {
                diff.push(format!("now {file} = {n}, frozen {:?}", frozen.get(file)));
            }
        }
        for (file, n) in &frozen {
            if !now.contains_key(file) {
                diff.push(format!("gone {file} (frozen {n})"));
            }
        }
        panic!(
            "the bare is_jdk_only() gate census moved (see the module doc; re-freeze from the \
             literal printed above and say why on the i49-L6 page):\n{}",
            diff.join("\n")
        );
    }
}

#[test]
fn the_scan_counts_only_bare_calls() {
    let text = "fn a(ctx: &dyn C) -> bool { ctx.is_jdk_only() }\n\
        fn b(cfg: &VmConfig) { if cfg.is_jdk_only() && !shared.config.is_jdk_only() {} }\n\
        // ctx.is_jdk_only() in a comment line\n\
        let x = 1; // a trailing ctx.is_jdk_only()\n\
        fn c() -> bool { config_is_jdk_only() }\n\
        pub fn is_jdk_only(&self) -> bool { true }\n\
        fn d() -> bool { is_jdk_only_yielded_bridge(\"a\", \"b\", \"c\") }\n";
    assert_eq!(gates_in(text), 3);
}

#[test]
fn the_frozen_literal_is_sorted_and_positive() {
    for pair in FROZEN.windows(2) {
        assert!(
            pair[0].0 < pair[1].0,
            "FROZEN out of order at {}",
            pair[1].0
        );
    }
    assert!(FROZEN.iter().all(|&(_, n)| n > 0));
}
