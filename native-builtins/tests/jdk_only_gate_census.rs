// SPDX-License-Identifier: Apache-2.0
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
/// `Intrinsic` arm, which `resolve_dispatch` applies under `--jdk-only` only --
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
    ("native-builtins/src/classloader.rs", 12),
    ("native-builtins/src/classloader_real.rs", 8),
    ("native-builtins/src/compat_carriers.rs", 1),
    ("native-builtins/src/generics.rs", 3),
    ("native-builtins/src/http_url_connection.rs", 3),
    ("native-builtins/src/jboss_jdkspecific.rs", 1),
    ("native-builtins/src/jca/kem.rs", 1),
    ("native-builtins/src/jca/provider_chain.rs", 2),
    ("native-builtins/src/keystore.rs", 1),
    ("native-builtins/src/lang_class.rs", 17),
    ("native-builtins/src/lang_invoke/adapters.rs", 2),
    ("native-builtins/src/lang_invoke/member_name.rs", 1),
    ("native-builtins/src/lang_invoke/var_handles.rs", 1),
    ("native-builtins/src/lang_math.rs", 3),
    ("native-builtins/src/lang_reflect.rs", 1),
    ("native-builtins/src/lang_string.rs", 1),
    ("native-builtins/src/lang_system.rs", 3),
    ("native-builtins/src/lib.rs", 11),
    ("native-builtins/src/lookup_define.rs", 1),
    ("native-builtins/src/net_phase_e.rs", 6),
    ("native-builtins/src/panama.rs", 3),
    ("native-builtins/src/phases_early.rs", 1),
    ("native-builtins/src/phases_late.rs", 1),
    ("native-builtins/src/phases_late/beans_jndi.rs", 1),
    ("native-builtins/src/phases_late/nio_file.rs", 9),
    ("native-builtins/src/phases_late/reflect_invoke.rs", 1),
    ("native-builtins/src/reference.rs", 1),
    ("native-builtins/src/reflect_annotations.rs", 5),
    ("native-builtins/src/servlet.rs", 1),
    ("native-builtins/src/shared_secrets_bridge.rs", 3),
    ("native-builtins/src/test_frameworks.rs", 1),
    ("native-builtins/src/unsafe_natives_ext.rs", 1),
    ("native-collections/src/arrays.rs", 1),
    ("native-collections/src/concurrent_hash_map.rs", 1),
    ("native-collections/src/hash_set.rs", 2),
    ("native-collections/src/linked_hash_map.rs", 1),
    ("native-collections/src/streams.rs", 5),
    ("native-io/src/lib.rs", 3),
    ("native-io/src/nio_native.rs", 1),
    ("vm/src/config.rs", 11),
    ("vm/src/jit/helpers/dispatch.rs", 1),
    ("vm/src/jit/helpers/fast_native.rs", 19),
    ("vm/src/jit/helpers/table.rs", 6),
    ("vm/src/jit/helpers/thread.rs", 3),
    ("vm/src/jit/helpers/typecheck.rs", 5),
    ("vm/src/native/jni.rs", 2),
    ("vm/src/runtime/exceptions.rs", 1),
    ("vm/src/runtime/interpreter.rs", 4),
    ("vm/src/runtime/interpreter/constants.rs", 2),
    ("vm/src/runtime/interpreter/dispatch_static.rs", 6),
    ("vm/src/runtime/interpreter/dispatch_virtual.rs", 4),
    ("vm/src/runtime/interpreter/exception_dispatch.rs", 2),
    ("vm/src/runtime/interpreter/execute_entry.rs", 1),
    ("vm/src/runtime/interpreter/field_access.rs", 1),
    ("vm/src/runtime/interpreter/gc_and_alloc.rs", 4),
    ("vm/src/runtime/interpreter/invoke.rs", 4),
    ("vm/src/runtime/interpreter/jit_bridge.rs", 12),
    ("vm/src/runtime/interpreter/jvmti_events.rs", 2),
    ("vm/src/runtime/interpreter/native_override.rs", 4),
    ("vm/src/runtime/interpreter/opcodes.rs", 1),
    ("vm/src/runtime/invokedynamic.rs", 1),
    ("vm/src/runtime/resolve/loader_throw.rs", 1),
    ("vm/src/vm/vm_exec.rs", 15),
    ("vm/src/vm/vm_init.rs", 14),
    ("vm/src/vm/vm_launcher.rs", 1),
    ("vm/src/vm/vm_object.rs", 5),
    ("vm/src/vm/vm_util.rs", 5),
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
    println!("jdk-only-gate-census: {total} bare gates in {} files", now.len());
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
        assert!(pair[0].0 < pair[1].0, "FROZEN out of order at {}", pair[1].0);
    }
    assert!(FROZEN.iter().all(|&(_, n)| n > 0));
}
