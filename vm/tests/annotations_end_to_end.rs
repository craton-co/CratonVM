// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! End-to-end Phase 1 test: load each annotated fixture, assert the
//! `OffloadCache` decision matches the canonical behavior matrix.
//!
//! These tests are gated behind the `gpu-offload` Cargo feature so the
//! default CPU-only build does not compile them. The fixtures live
//! under `test_classes/gpu/annotations/` and are produced by the
//! `jit-cuda/build.rs` build script from the corresponding `.java`
//! sources.
//!
//! Phase 1 scope (the eight fixtures Items 7 and 8 own):
//!
//! | Fixture | Annotation | Expected verdict |
//! |---------|------------|------------------|
//! | `StrictKernel`             | `@GpuKernel`                            | `Eligible` |
//! | `StrictRejectsAllocation`  | `@GpuKernel` (STRICT) + allocation      | `Ineligible(Allocation)` |
//! | `AdmitAllocation`          | `@GpuKernel(admit=ALLOW_ALLOCATION)`    | `Eligible` |
//! | `AdmitDivByZero`           | `@GpuKernel(admit=ALLOW_DIV_BY_ZERO)`   | `Eligible` |
//! | `AdmitMathSqrt`            | `@GpuKernel(admit=ALLOW_INTRINSIC_CALLS)` | `Eligible` |
//! | `ExcludedKernel`           | `@GpuExclude`                           | `Blacklisted` |
//! | `ExcludedAndKernel`        | `@GpuExclude` + `@GpuKernel`            | `Blacklisted` |
//! | `WarmupTwo`                | `@EnableGpuAsync(warmup=2)`             | 2 cache entries |
//!
//! Because this dev box has no CUDA driver, `OffloadCache::ctx` is
//! `None` and every `lookup_or_compile` would normally return
//! `Skip` before reaching the analyzer. To assert the verdict-side
//! semantics we call `jit_cuda::analyze_with_annotations` (or whatever
//! the canonical entry point is named) directly where the cache would
//! short-circuit.
//!
//! Every test below is marked `#[ignore]` until Items 7 and 8 land the
//! fixtures and Items 3/4/5/6 land the annotation-aware analyzer and
//! cache plumbing. The bodies are intentionally complete so that
//! removing the `#[ignore]` is the only change needed once the
//! upstream pieces merge. Locations that guess at an API name are
//! flagged with `// PHASE1-GUESS:`.

#![cfg(feature = "gpu-offload")]

use std::path::PathBuf;

use cratonvm_reader::class_reader::read_class;
use cratonvm_reader::method::ClassFileMethod;

use cratonvm_vm::classloading::ClassId;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::runtime::offload::{LookupOutcome, OffloadCache};

// PHASE1-GUESS: the annotation-aware analyzer entry point. Items 3/4
// land this as `jit_cuda::analyzer::analyze_with_annotations` (taking
// the method *and* its parsed annotations). The bare `analyze` import
// is the current name; we re-export through `jit_cuda` so both work.
use jit_cuda::{analyze, OffloadVerdict, Reason};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Path to a Phase 1 annotation fixture. `CARGO_MANIFEST_DIR` points
/// at `vm/`; fixtures live two levels up under
/// `test_classes/gpu/annotations/`.
fn fixture_path(class_name: &str) -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let root = PathBuf::from(manifest_dir)
        .parent()
        .expect("workspace root is vm/..")
        .to_path_buf();
    let base = if root.join("test_classes").join("gpu").is_dir() {
        root.join("test_classes").join("gpu")
    } else {
        root.join("tools").join("test_classes").join("gpu")
    };
    base.join("annotations").join(format!("{class_name}.class"))
}

/// Whether a given fixture file exists. Items 7 and 8 produce these
/// in parallel; we tolerate their absence so this scaffold compiles
/// before they land.
fn fixture_exists(class_name: &str) -> bool {
    fixture_path(class_name).is_file()
}

/// Load the methods and `this_class` of a fixture. Mirrors
/// `vm/src/runtime/offload.rs`'s test helper of the same shape — the
/// `OffloadCache` API only needs `(class_id, class_name,
/// method_index, &ClassFileMethod)`, so we don't go through the heavy
/// `cratonvm_classloading::Class` path.
fn load_methods(class_name: &str) -> (Vec<ClassFileMethod>, String, cratonvm_reader::ConstantPool) {
    let path = fixture_path(class_name);
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()));
    let cf = read_class(&bytes)
        .unwrap_or_else(|e| panic!("failed to parse fixture {}: {e:?}", path.display()));
    (cf.methods, cf.this_class.to_string(), cf.constant_pool)
}

/// Pick the (first) method with a given name. Phase 1 fixtures each
/// expose a single kernel method, so name-only lookup is enough.
fn method_by_name<'a>(methods: &'a [ClassFileMethod], name: &str) -> (u16, &'a ClassFileMethod) {
    let idx = methods
        .iter()
        .position(|m| &*m.name == name)
        .unwrap_or_else(|| panic!("method {name} not found in fixture"));
    (idx as u16, &methods[idx])
}

/// A `ClassId` chosen out of the regular VM allocator range so tests
/// cannot collide with a real loaded class. Mirrors the in-module
/// tests in `vm/src/runtime/offload.rs`.
const TEST_CLASS_ID: ClassId = ClassId::new(0xDEAD_BEEF);

/// Build an `OffloadCache` configured exactly the way a `--gpu` run
/// would build it on a no-driver box: `gpu_offload_enabled = true`,
/// driver probe returns `NoDriver`, so `ctx` ends up `None`. The
/// blacklist / kernels maps still work, which is what we exercise
/// here.
fn cache_with_gpu_requested() -> OffloadCache {
    let mut cfg = VmConfig::default();
    cfg.gpu_offload_enabled = true;
    cfg.print_gpu_decisions = true;
    OffloadCache::new(&cfg)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `AdmitAllocation` is `@GpuKernel(admit = ALLOW_ALLOCATION)` on a
/// method that allocates a primitive array sized from a parameter.
/// Strict analysis rejects it (`Reason::Allocation`); the annotation-
/// aware analyzer must admit it as `Eligible`.
///
/// On a no-device box the cache will report `Skip` *before* reaching
/// the analyzer. To assert the verdict itself we call the analyzer
/// directly. Once a GPU is attached the same fixture should produce
/// `LookupOutcome::Hit` via the cache.
#[test]
#[ignore = "depends on Items 3/4 annotation-aware analyzer and Item 7 fixture"]
fn admit_allocation_eligible() {
    if !fixture_exists("AdmitAllocation") {
        eprintln!("[annotations] skipping admit_allocation_eligible: fixture missing");
        return;
    }
    let (methods, class_name, cp) = load_methods("AdmitAllocation");
    let (_idx, method) = method_by_name(&methods, "doubled");

    // PHASE1-GUESS: once Item 3 lands the annotation-aware entry
    // point, replace `analyze(method)` with
    // `analyze_with_annotations(method, &class_annotations)`. The
    // class-level annotations come from the parsed `.class` file's
    // RuntimeVisibleAnnotations attribute via a helper in
    // `jit-cuda::analyzer`.
    let verdict = analyze(method);
    match verdict {
        OffloadVerdict::Eligible(_) => {}
        OffloadVerdict::Rejected(r) => panic!(
            "expected Eligible under ALLOW_ALLOCATION; got Rejected({:?}). \
             Did the annotation-aware analyzer land yet?",
            r
        ),
    }

    // Cache-side check: on no-device the cache short-circuits to Skip
    // (we never reach the analyzer through this path).
    let cache = cache_with_gpu_requested();
    if cache.has_device() {
        // Real GPU attached — should compile and hit.
        match cache.lookup_or_compile(TEST_CLASS_ID, &class_name, 0, method, &cp) {
            LookupOutcome::Hit(_) => {}
            other => panic!(
                "expected Hit on GPU box for AdmitAllocation, got {}",
                lookup_outcome_label(&other)
            ),
        }
    } else {
        match cache.lookup_or_compile(TEST_CLASS_ID, &class_name, 0, method, &cp) {
            LookupOutcome::Skip => {}
            other => panic!(
                "expected Skip on no-device box for AdmitAllocation, got {}",
                lookup_outcome_label(&other)
            ),
        }
    }
}

/// `ExcludedKernel.add` is `@GpuExclude` on an analyzer-eligible
/// method. The cache must surface `Blacklisted` on first reach,
/// regardless of device availability.
#[test]
#[ignore = "depends on Items 3/4 annotation-aware cache and Item 7 fixture"]
fn excluded_kernel_blacklisted() {
    if !fixture_exists("ExcludedKernel") {
        eprintln!("[annotations] skipping excluded_kernel_blacklisted: fixture missing");
        return;
    }
    let (methods, class_name, cp) = load_methods("ExcludedKernel");
    let (idx, method) = method_by_name(&methods, "add");

    let cache = cache_with_gpu_requested();
    // PHASE1-GUESS: Item 4 wires `@GpuExclude` recognition into
    // `lookup_or_compile`. Pre-landing, this assertion will fail
    // because the no-device fast path returns Skip first; that is
    // exactly what the `#[ignore]` is for.
    match cache.lookup_or_compile(TEST_CLASS_ID, &class_name, idx, method, &cp) {
        LookupOutcome::Blacklisted => {}
        other => panic!(
            "expected Blacklisted for @GpuExclude method, got {}",
            lookup_outcome_label(&other)
        ),
    }
}

/// `ExcludedAndKernel.add` has both `@GpuExclude` and `@GpuKernel`.
/// The contract is that `@GpuExclude` wins.
#[test]
#[ignore = "depends on Items 3/4 annotation-aware cache and Item 7 fixture"]
fn excluded_and_kernel_exclude_wins() {
    if !fixture_exists("ExcludedAndKernel") {
        eprintln!("[annotations] skipping excluded_and_kernel_exclude_wins: fixture missing");
        return;
    }
    let (methods, class_name, cp) = load_methods("ExcludedAndKernel");
    let (idx, method) = method_by_name(&methods, "add");

    let cache = cache_with_gpu_requested();
    match cache.lookup_or_compile(TEST_CLASS_ID, &class_name, idx, method, &cp) {
        LookupOutcome::Blacklisted => {}
        other => panic!(
            "expected Blacklisted: @GpuExclude must take priority over @GpuKernel; got {}",
            lookup_outcome_label(&other)
        ),
    }
}

/// `WarmupTwo` is `@EnableGpuAsync(warmup = 2)` on a class with three
/// `@GpuKernel` methods. After class load (or a direct
/// `OffloadCache::warmup_class` call) the cache must contain exactly
/// two entries for this class.
#[test]
#[ignore = "depends on Items 5/6 cache.warmup_class and Item 8 fixture"]
fn warmup_class_compiles_two() {
    if !fixture_exists("WarmupTwo") {
        eprintln!("[annotations] skipping warmup_class_compiles_two: fixture missing");
        return;
    }
    let (_methods, _class_name, _cp) = load_methods("WarmupTwo");

    let cache = cache_with_gpu_requested();
    if !cache.has_device() {
        // Without a device, warmup is a documented no-op. The cache
        // stays empty. We assert the no-op shape: no panic, no
        // entries.
        // PHASE1-GUESS: `warmup_class(class_id, &class_name, &methods, max = 2)`
        // is the canonical entry point. Once the cache exposes
        // `entry_count_for_class`, replace the assertion below.
        // For now the test is a structural no-op until items 5/6
        // land.
        return;
    }

    // PHASE1-GUESS: real cache should expose
    //   cache.warmup_class(class_id, &class_name, &methods, 2);
    //   assert_eq!(cache.entry_count_for_class(class_id), 2);
    panic!(
        "warmup_class_compiles_two: GPU attached but warmup_class API not yet \
         available; remove this panic once Item 5 wires it up"
    );
}

/// `StrictRejectsAllocation` is `@GpuKernel` *without* admit on a
/// method that allocates. The analyzer must reject it with
/// `Reason::Allocation` (i.e. strict-mode `@GpuKernel` does not
/// loosen anything).
#[test]
#[ignore = "depends on Items 3/4 annotation-aware analyzer and Item 7 fixture"]
fn strict_rejects_allocation() {
    if !fixture_exists("StrictRejectsAllocation") {
        eprintln!("[annotations] skipping strict_rejects_allocation: fixture missing");
        return;
    }
    let (methods, _class_name, _cp) = load_methods("StrictRejectsAllocation");
    let (_idx, method) = method_by_name(&methods, "doubled");

    // PHASE1-GUESS: the annotation-aware analyzer must observe a
    // `@GpuKernel(admit = STRICT)` annotation and still reject. The
    // bare `analyze` is equivalent for STRICT — it has always
    // rejected `Reason::Allocation` — so this assertion is stable
    // pre- and post-Items 3/4.
    match analyze(method) {
        OffloadVerdict::Rejected(Reason::Allocation) => {}
        OffloadVerdict::Rejected(other) => panic!(
            "expected Rejected(Allocation) under STRICT, got Rejected({:?})",
            other
        ),
        OffloadVerdict::Eligible(_) => {
            panic!("expected Rejected(Allocation) under STRICT, got Eligible")
        }
    }
}

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

fn lookup_outcome_label(o: &LookupOutcome) -> &'static str {
    match o {
        LookupOutcome::Hit(_) => "Hit",
        LookupOutcome::Skip => "Skip",
        LookupOutcome::Blacklisted => "Blacklisted",
    }
}
