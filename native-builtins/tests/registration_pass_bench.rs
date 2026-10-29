// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The boot-time native registration pass, as a lever: its CENSUS (the
//! equivalence oracle for any speed change to `NativeMethodRegistry::register`)
//! and its COST, for both `--compatible` and `--jdk-only` over a real JDK.
//!
//! Phase 5 fast boot (`p5-natreg`): `register` cost ~10 µs per registration on
//! the build host's `--compatible` Hello (~8.5% of the boot thread), two orders
//! of magnitude above a hash insert. A speed change there must not move a
//! single registration, kind, callback or provenance row -- a previous
//! "fast boot" that skipped whole domains printed nothing for Hello. So this
//! file measures and prints, and the comparison is done across trees:
//!
//! ```text
//! CRATONVM_REGBENCH_OUT=<dir> cargo test --release -p cratonvm-native-builtins \
//!     --test registration_pass_bench -- --nocapture --test-threads=1
//! ```
//!
//! writes `<dir>/census-compatible.txt` and `<dir>/census-jdk-only.txt`; run it
//! on the base tree and on the change and `diff` the two directories. Each test
//! also prints the census's FNV-1a digest and the line count, so a quick look
//! at two logs is enough when the digests agree. The census is not committed:
//! it carries `registered_by` file:line for every row, so any unrelated edit to
//! a registrar moves it.
//!
//! The registry is configured the way `vm_init` configures it before its first
//! `register_*` pass (capability set from the environment, compatibility mode,
//! the graduated alignment rows, `real_jdk`, JIT on, JDK 25, JDK-driven
//! continuations, type-annotation bytes), then replayed through the shared
//! model of `vm_init`'s real-JDK arm (`common/vm_init_boot_path.rs`).
//!
//! Timing: the FIRST pass in the process is reported separately (`cold`: it
//! pays every process-wide `OnceLock` the predicates build, which a real boot
//! pays too), then the minimum of [`WARM_ROUNDS`] further passes.

#[path = "common/vm_init_boot_path.rs"]
mod boot_path;

use boot_path::vm_init_real_jdk_boot_path;
use cratonvm_native_api::{NativeMethodRegistry, VmId};
use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::HotSpotAlignments;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WARM_ROUNDS: usize = 9;

fn configured_registry(mode: CompatibilityMode) -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    r.set_capabilities(Arc::new(cratonvm_native_api::CapabilitySet::from_env(
        VmId::from_raw(1),
    )));
    r.set_compatibility_mode(mode);
    r.set_hotspot_alignments(HotSpotAlignments::from_env().with_graduated_rows());
    r.set_real_jdk(true);
    r.set_jit_enabled(true);
    r.set_jdk_feature(Some(25));
    r.set_jdk_continuations(true);
    r.set_type_annotation_bytes_real(true);
    r
}

fn build(mode: CompatibilityMode) -> (NativeMethodRegistry, Duration) {
    let mut r = configured_registry(mode);
    let t = Instant::now();
    vm_init_real_jdk_boot_path(&mut r);
    let dt = t.elapsed();
    (r, dt)
}

/// One line per registration (census order, i.e. registration order), then
/// the refusals, then the class-agnostic `(method, descriptor)` index. The
/// callback is named by the census row that first owned it, which is stable
/// across builds where a function address is not.
fn canonical_census(r: &NativeMethodRegistry) -> String {
    let census = r.census();
    let mut cb_name: HashMap<usize, String> = HashMap::new();
    let mut slot_cb = Vec::with_capacity(census.len());
    for (i, e) in census.iter().enumerate() {
        let cb = if e.owns_slot {
            r.resolve_id(&e.class, &e.name, &e.descriptor)
                .and_then(|id| r.callback_of(id))
                .map(|cb| cb as usize)
        } else {
            None
        };
        if let Some(cb) = cb {
            cb_name.entry(cb).or_insert_with(|| format!("cb@{i}"));
        }
        slot_cb.push(cb);
    }
    let mut out = String::new();
    let _ = writeln!(out, "generation {} len {}", r.generation(), r.len());
    for (i, e) in census.iter().enumerate() {
        let cb = slot_cb[i]
            .map(|cb| cb_name[&cb].clone())
            .unwrap_or_default();
        let leaf = if e.owns_slot {
            r.resolve_id(&e.class, &e.name, &e.descriptor)
                .map(|id| r.is_leaf_id(id))
                .unwrap_or(false)
        } else {
            false
        };
        let by_md = r
            .find_by_method_descriptor(&e.name, &e.descriptor)
            .map(|cb| {
                cb_name
                    .get(&(cb as usize))
                    .cloned()
                    .unwrap_or_else(|| "cb@superseded".to_string())
            })
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{i}\t{}.{}{}\t{:?}\tby={}\tover={:?}\tstated={}\tchosen={}\towns={}\tleaf={}\t{cb}\tmd={by_md}",
            e.class,
            e.name,
            e.descriptor,
            e.kind,
            e.registered_by.as_deref().unwrap_or("-"),
            e.overwrote,
            e.kind_stated,
            e.kind_chosen,
            e.owns_slot,
            leaf,
        );
    }
    for v in r.refused_registrations() {
        let _ = writeln!(out, "refused\t{v:?}");
    }
    out
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn run(mode: CompatibilityMode, tag: &str) {
    let (first, cold) = build(mode);
    let census = canonical_census(&first);
    let regs = first.census().len();
    // `CRATONVM_REGBENCH_ROUNDS=N` (default [`WARM_ROUNDS`]): on a shared host,
    // run with two values of N under `perf stat -e instructions:u` and divide
    // the difference by the difference in N -- the census and the cold pass
    // cancel out, leaving the instructions of one registration pass.
    let rounds = std::env::var("CRATONVM_REGBENCH_ROUNDS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(WARM_ROUNDS);
    let mut warm = Duration::MAX;
    for _ in 0..rounds {
        let (r, dt) = build(mode);
        warm = warm.min(dt);
        // Every round must build the same registry, or the timing is of
        // something else (cheap checks: no census per round; not
        // `generation()`, which carries a per-registry epoch).
        assert_eq!(
            (r.len(), r.refused_registrations().len()),
            (first.len(), first.refused_registrations().len()),
            "{tag}: round built a different registry"
        );
    }
    if let Some(dir) = cratonvm_types::flags::runtime_var_os("CRATONVM_REGBENCH_OUT") {
        let _ = std::fs::create_dir_all(&dir);
        let path = std::path::Path::new(&dir).join(format!("census-{tag}.txt"));
        std::fs::write(&path, &census).expect("write census");
        println!("[regbench] {tag}: census written to {}", path.display());
    }
    println!(
        "[regbench] {tag}: {regs} registrations, {} refused, census lines {} fnv {:016x}; \
         cold {:.3} ms, warm(min of {rounds}) {:.3} ms = {:.0} ns/registration",
        first.refused_registrations().len(),
        census.lines().count(),
        fnv1a(&census),
        cold.as_secs_f64() * 1e3,
        warm.as_secs_f64() * 1e3,
        warm.as_secs_f64() * 1e9 / regs.max(1) as f64,
    );
    assert!(
        regs > 1000,
        "{tag}: the replay registered almost nothing ({regs})"
    );
}

#[test]
fn registration_pass_compatible() {
    run(CompatibilityMode::Compatible, "compatible");
}

#[test]
fn registration_pass_jdk_only() {
    run(CompatibilityMode::JdkOnly, "jdk-only");
}
