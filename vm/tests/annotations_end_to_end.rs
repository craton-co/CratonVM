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
//! | `StrictRejectsAllocation`  | `@GpuKernel` (STRICT) + allocation      | `Ineligible(UnsupportedReturnType)` |
//! | `AdmitAllocation`          | `@GpuKernel(admit=ALLOW_ALLOCATION)`    | `Ineligible(UnsupportedReturnType)` (identical; see note) |
//! | `AdmitDivByZero`           | `@GpuKernel(admit=ALLOW_DIV_BY_ZERO)`   | `Eligible` |
//! | `AdmitMathSqrt`            | `@GpuKernel(admit=ALLOW_INTRINSIC_CALLS)` | `Eligible` |
//! | `ExcludedKernel`           | `@GpuExclude`                           | `Blacklisted` (device-gated — see below) |
//! | `ExcludedAndKernel`        | `@GpuExclude` + `@GpuKernel`            | `Blacklisted` (device-gated — see below) |
//! | `WarmupTwo`                | `@EnableGpuAsync(warmup=2)`             | 2 cache entries |
//!
//! Because a box without a CUDA driver leaves `OffloadCache::ctx`
//! `None`, every `lookup_or_compile` returns `Skip` before reaching
//! the analyzer. To assert the verdict-side semantics the analyzer is
//! called directly (`jit_cuda::analyze`) where the cache would
//! short-circuit.
//!
//! AUDIT 2026-09-21: every test here used to be `#[ignore]`d, pending
//! Items 3/4/5/6 and the fixtures. Those landed; the `#[ignore]`s did
//! not come off, and under them the file rotted in three separate ways
//! — `load_methods` never force-decoded the lazy `Code` attribute, so
//! every verdict assertion answered `Rejected(NoCode)` no matter what
//! was asked; two tests looked up a method named `doubled` and two more
//! one named `add`, neither of which any fixture has ever declared
//! (`mapSquare` and `vectorAdd`); and one asserted `Eligible` for a
//! shape the emitter cannot lower, which was the bug it should have
//! caught. All four now run by default.
//!
//! `warmup_class_compiles_two` stays `#[ignore]`d and is the one place
//! the original reason still holds: it is a structural no-op without a
//! device and an explicit "API not yet available" panic with one, so
//! `OffloadCache::warmup_class` / `entry_count_for_class` really do
//! have to land before it can assert anything. Remaining guesses at an
//! API name are flagged with `// PHASE1-GUESS:`.
//!
//! Device-gated assertions: `lookup_or_compile` answers `Skip` on
//! `ctx.is_none()` before it decodes an annotation, so the
//! `@GpuExclude` short-circuit — and therefore `Blacklisted` — is
//! unreachable on a no-driver box. The two exclude tests assert the
//! annotation half unconditionally and the outcome half against
//! `has_device()`; see `excluded_kernel_blacklisted`.

#![cfg(feature = "gpu-offload")]

use std::path::PathBuf;

use cratonvm_reader::attribute::Attribute;
use cratonvm_reader::class_reader::read_class;
use cratonvm_reader::constant_pool::ConstantPool;
use cratonvm_reader::method::ClassFileMethod;

use cratonvm_vm::classloading::ClassId;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::runtime::offload::{LookupOutcome, OffloadCache};

// PHASE1-GUESS: the annotation-aware analyzer entry point. Items 3/4
// land this as `jit_cuda::analyzer::analyze_with_annotations` (taking
// the method *and* its parsed annotations). The bare `analyze` import
// is the current name; we re-export through `jit_cuda` so both work.
use jit_cuda::annotations::{read_method_annotations, MethodAnnotations};
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
    let mut cf = read_class(&bytes)
        .unwrap_or_else(|e| panic!("failed to parse fixture {}: {e:?}", path.display()));
    // AUDIT 2026-09-21: the reader keeps method attributes lazy
    // (`LazyAttribute::Raw`) and `ClassFileMethod::code()` only returns
    // an ALREADY-decoded `Code`, so without this every fixture method
    // looks bodiless and the analyzer answers `Rejected(NoCode)` — for
    // any fixture, under any hint, which would have made every verdict
    // assertion in this file vacuous. `#[ignore]` hid it. Mirrors
    // `jit-cuda/src/test_support.rs`'s loader, including its reason for
    // decoding only `Code`: force-decoding every attribute can hit a
    // `ByteView` range panic on some annotation attributes, and the
    // analyzer needs nothing but the body.
    let cp = &cf.constant_pool;
    for method in cf.methods.iter_mut() {
        for attr in method.attributes.iter_mut() {
            if attr.name() == "Code" {
                let _ = attr.decode(cp);
            }
        }
    }
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

/// Read a method's GPU annotations exactly the way the cache does.
///
/// `OffloadCache::lookup_or_compile` calls `decode_method_attrs` +
/// `jit_cuda::annotations::read_method_annotations`; the former is
/// `pub(crate)` in `cratonvm_vm`, so this integration test reproduces
/// its three lines rather than reaching for it. Only the two
/// annotation attributes are decoded, not every attribute — see
/// `load_methods` above for why that distinction is worth keeping.
fn method_annotations(method: &ClassFileMethod, cp: &ConstantPool) -> MethodAnnotations {
    let decoded: Vec<Attribute> = method
        .attributes
        .iter()
        .filter(|la| {
            matches!(
                la.name(),
                "RuntimeVisibleAnnotations" | "RuntimeInvisibleAnnotations"
            )
        })
        .filter_map(|la| {
            let mut cloned = la.clone();
            cloned.decode(cp).ok().cloned()
        })
        .collect();
    read_method_annotations(&decoded, cp)
}

/// The cache-side half of the two `@GpuExclude` tests.
///
/// `Blacklisted` is only reachable with a device: `lookup_or_compile`
/// answers `Skip` on `ctx.is_none()` before it decodes an annotation.
/// What holds on every box is that an excluded method never compiles
/// to a kernel.
fn assert_excluded_outcome(
    class_name: &str,
    idx: u16,
    method: &ClassFileMethod,
    cp: &ConstantPool,
    fixture: &str,
) {
    let cache = cache_with_gpu_requested();
    let outcome = cache.lookup_or_compile(TEST_CLASS_ID, class_name, idx, method, cp);
    match (&outcome, cache.has_device()) {
        (LookupOutcome::Blacklisted, true) => {}
        (LookupOutcome::Skip, false) => {
            eprintln!(
                "[annotations] {fixture}: no CUDA device — cache short-circuits to Skip \
                 before the @GpuExclude check; annotation half asserted above"
            );
        }
        (other, has_device) => panic!(
            "@GpuExclude method {fixture}.vectorAdd: got {} with has_device = {has_device}",
            lookup_outcome_label(other)
        ),
    }
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
///
/// AUDIT 2026-09-21: this test used to assert `Eligible`, and that is
/// exactly what the bug was. The analyzer admitted the `newarray`;
/// `jit-cuda/src/lowering/emit.rs` has no arm for it, so the method
/// was lowered, refused with "opcode 0xbc not implemented in
/// lowering", and blacklisted — one wasted analyze->lower round-trip
/// per annotated method. The hint is now accepted and inert:
/// allocation rejects under every hint, so `AdmitAllocation` and
/// `StrictRejectsAllocation` — the same body, different `admit` —
/// must now reach the SAME verdict.
///
/// That verdict is `UnsupportedReturnType`, not `Allocation`. The
/// fixture is `int[] mapSquare(int[])`, and closing the allocation
/// hole surfaced the same defect one layer out: an array RETURN kind
/// is equally unlowerable — `areturn` (0xB0) has no emitter arm
/// either — and needs no allocation to get there, so it is rejected
/// from the descriptor, before the body is scanned. What this test
/// pins is that the hint changes nothing; which of the two rules
/// fires first is pinned by `jit_cuda`'s
/// `array_return_rejected_before_the_body_is_scanned`. See
/// `jit_cuda::analyzer::Reason::UnsupportedReturnType` and
/// `Reason::Allocation` for what lowering either would take — a VM
/// marshaller change, not an emitter one — and
/// `docs/gpu/annotations.md` for the user-facing statement.
///
/// On a no-device box the cache reports `Skip` *before* reaching the
/// analyzer, so the verdict is asserted against the analyzer
/// directly; the cache-side arm only pins that a rejected method
/// never becomes a `Hit`.
#[test]
fn admit_allocation_is_rejected_despite_the_hint() {
    if !fixture_exists("AdmitAllocation") {
        eprintln!(
            "[annotations] skipping admit_allocation_is_rejected_despite_the_hint: \
             fixture missing"
        );
        return;
    }
    let (methods, class_name, cp) = load_methods("AdmitAllocation");
    let (_idx, method) = method_by_name(&methods, "mapSquare");

    // `analyze` is the strict entry point, and that is now the point:
    // the annotation-aware one has to agree with it on this fixture.
    match analyze(method) {
        OffloadVerdict::Rejected(Reason::UnsupportedReturnType) => {}
        OffloadVerdict::Rejected(other) => panic!(
            "expected Rejected(UnsupportedReturnType) under ALLOW_ALLOCATION, got \n             Rejected({:?})",
            other
        ),
        OffloadVerdict::Eligible(_) => panic!(
            "expected Rejected(UnsupportedReturnType) under ALLOW_ALLOCATION, got Eligible — the \
             hint must not admit a shape the emitter cannot lower"
        ),
    }

    // Cache-side: a rejected method must never surface as a Hit, with
    // or without a device. On a no-device box the cache short-circuits
    // to Skip before the analyzer even runs.
    let cache = cache_with_gpu_requested();
    if let LookupOutcome::Hit(_) =
        cache.lookup_or_compile(TEST_CLASS_ID, &class_name, 0, method, &cp)
    {
        panic!(
            "AdmitAllocation must not compile to a kernel (has_device = {})",
            cache.has_device()
        );
    }
}

/// `ExcludedKernel.vectorAdd` is `@GpuExclude` on an analyzer-eligible
/// method. The cache must record it in the blacklist and answer
/// `Blacklisted` without ever handing the method to the analyzer.
///
/// AUDIT 2026-09-21: this asked for a method named `add`, which no
/// fixture has ever declared — the same guessed-ahead-of-the-fixture
/// name that `doubled` was for the allocation pair above. Renamed to
/// `vectorAdd`, which is what `ExcludedKernel.java` actually declares.
///
/// The rename alone does not make the cache-side assertion reachable.
/// `lookup_or_compile` returns `Skip` on `ctx.is_none()` *before* it
/// decodes any annotation, so on a no-driver box the `@GpuExclude`
/// short-circuit is dead code and the old unconditional
/// `expected Blacklisted` could only ever fail here — that, not the
/// annotation-aware cache (which landed), is what the `#[ignore]` was
/// really standing in for. The in-module tests in
/// `vm/src/runtime/offload.rs` (`excluded_method_returns_blacklisted`,
/// `excluded_short_circuits_analyzer`) already gate on `has_device()`
/// for exactly this reason.
///
/// So the test is split into the half that needs a device and the half
/// that does not, and runs unconditionally:
///
/// - device-free: the fixture really does carry `@GpuExclude` and
///   `read_method_annotations` — the exact input the short-circuit
///   consumes — really does see it. This is what catches a fixture or
///   annotation-parser regression, and it is what the old test only
///   *looked* like it was checking.
/// - device-only: the outcome is `Blacklisted`. Without a device the
///   outcome is `Skip`, and the one thing that must hold either way is
///   that an excluded method is never a `Hit`.
#[test]
fn excluded_kernel_blacklisted() {
    if !fixture_exists("ExcludedKernel") {
        eprintln!("[annotations] skipping excluded_kernel_blacklisted: fixture missing");
        return;
    }
    let (methods, class_name, cp) = load_methods("ExcludedKernel");
    let (idx, method) = method_by_name(&methods, "vectorAdd");

    let anns = method_annotations(method, &cp);
    let exclude = anns
        .gpu_exclude
        .as_ref()
        .expect("ExcludedKernel.vectorAdd must carry @GpuExclude");
    assert_eq!(exclude.reason, "branchy; CPU is faster");

    assert_excluded_outcome(&class_name, idx, method, &cp, "ExcludedKernel");
}

/// `ExcludedAndKernel.vectorAdd` has both `@GpuExclude` and
/// `@GpuKernel`. The contract is that `@GpuExclude` wins.
///
/// AUDIT 2026-09-21: same `add` -> `vectorAdd` rename and same split as
/// [`excluded_kernel_blacklisted`]. The device-free half matters more
/// here than there: precedence is only a question at all when BOTH
/// annotations are present, so a fixture that silently lost one of them
/// would leave the surviving assertion true for the wrong reason.
#[test]
fn excluded_and_kernel_exclude_wins() {
    if !fixture_exists("ExcludedAndKernel") {
        eprintln!("[annotations] skipping excluded_and_kernel_exclude_wins: fixture missing");
        return;
    }
    let (methods, class_name, cp) = load_methods("ExcludedAndKernel");
    let (idx, method) = method_by_name(&methods, "vectorAdd");

    let anns = method_annotations(method, &cp);
    assert!(
        anns.gpu_kernel.is_some(),
        "ExcludedAndKernel.vectorAdd must carry @GpuKernel — without it there is \
         no precedence left to test"
    );
    let exclude = anns
        .gpu_exclude
        .as_ref()
        .expect("ExcludedAndKernel.vectorAdd must carry @GpuExclude");
    assert_eq!(exclude.reason, "overrides @GpuKernel");

    assert_excluded_outcome(&class_name, idx, method, &cp, "ExcludedAndKernel");
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
/// method that allocates. The analyzer must reject it — strict-mode
/// `@GpuKernel` loosens nothing.
///
/// AUDIT 2026-09-21: the reason is `UnsupportedReturnType`, not
/// `Allocation`. The fixture is `int[] mapSquare(int[])` and an array
/// return kind is rejected from the descriptor before the body is
/// scanned. This is the STRICT half of
/// [`admit_allocation_is_rejected_despite_the_hint`] — same body, and
/// now demonstrably the same verdict.
#[test]
fn strict_rejects_allocation() {
    if !fixture_exists("StrictRejectsAllocation") {
        eprintln!("[annotations] skipping strict_rejects_allocation: fixture missing");
        return;
    }
    let (methods, _class_name, _cp) = load_methods("StrictRejectsAllocation");
    // The fixture's method is `mapSquare`; this said `doubled` (the
    // name used in the `docs/gpu/annotations.md` example, never in the
    // fixture) from the day it was written, which `#[ignore]` hid.
    let (_idx, method) = method_by_name(&methods, "mapSquare");

    // The bare `analyze` is equivalent for STRICT, so the assertion
    // is stable whether or not the annotation is read.
    match analyze(method) {
        OffloadVerdict::Rejected(Reason::UnsupportedReturnType) => {}
        OffloadVerdict::Rejected(other) => panic!(
            "expected Rejected(UnsupportedReturnType) under STRICT, got Rejected({:?})",
            other
        ),
        OffloadVerdict::Eligible(_) => {
            panic!("expected Rejected(UnsupportedReturnType) under STRICT, got Eligible")
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
