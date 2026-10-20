// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Byte-for-byte snapshots of the PTX every eligible fixture lowers to.
//!
//! # Why this exists
//!
//! `docs/gpu/lowering-architecture.md` describes this crate as a second
//! Java bytecode front-end that "emits target text directly, with no
//! intermediate representation", and proposes replacing the body
//! `String` with an instruction list so a second target can be rendered
//! from the same lowering. That refactor rewrites every one of the ~194
//! emission sites in `lowering/emit.rs`.
//!
//! The existing tests cannot carry that change. They are `contains`
//! assertions over fragments — `assert!(text.contains("mad.wide.s32
//! %rd4, %r3, 4, %rd1;"))` — so they pin the instructions somebody
//! thought to write an assertion for and say nothing about the rest of
//! the module: register numbering, declaration order, the prologue
//! splice point, whitespace, the header. A refactor can keep every
//! `contains` green while moving the text underneath it.
//!
//! So this module records the WHOLE rendered module for every fixture
//! the analyzer admits, and compares it byte for byte. The refactor is
//! correct exactly when these files do not move. That is the entire
//! claim being made here; it is not a test of whether the PTX is
//! *right*, only of whether it is *unchanged*.
//!
//! # Not a synthetic fixture set
//!
//! The inputs are the same real `.class` files `build.rs` compiles from
//! `tools/test_classes/gpu/*.java` — see `test_support`'s standing rule
//! against synthetic bytecode. Only the *expected output* is recorded.
//!
//! # Discovered, not listed
//!
//! The fixture list is the fixture directory. A hand-maintained list
//! would be a fourth thing that has to agree with the other three (see
//! that same doc's "FOUR lists, not one"), and it would silently stop
//! covering a fixture somebody added a kernel to. Every `.class` in the
//! directory is parsed, every method is offered to the analyzer, and
//! everything it admits is lowered and recorded.
//!
//! # Refusals are recorded too
//!
//! A method the analyzer admits and the emitter then refuses is the
//! analyze→lower round trip `analyzer.rs` warns about ("a method
//! admitted here only to be rejected by `walk` wastes an analyze→lower
//! round-trip and pollutes the per-method blacklist"). Those go in
//! `manifest.txt` with the refusal message rather than being dropped,
//! so the refusal SET is pinned as tightly as the accepted one: a
//! refactor that quietly starts refusing a kernel fails here.
//!
//! # Blessing
//!
//! `GPU_GOLDEN_BLESS=1 cargo test -p cratonvm-jit-cuda golden` rewrites
//! every file and the manifest, then still asserts — so a bless run
//! that cannot lower a fixture at all still fails.
//!
//! The variable is deliberately NOT named `CRATONVM_*`: it is read by
//! `std::env::var` under `#[cfg(test)]` and never by a shipped binary,
//! and the four-file flag-surface obligation in CONTRIBUTING.md is for
//! knobs the VM reads at run time. Adding a test-only name to
//! `types/tests/flag-surface.txt` would claim a runtime knob exists.

#![cfg(test)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::analyzer::{analyze_with_annotations_and_pool, OffloadVerdict};
use crate::annotations::{
    read_method_annotations, AdmissionFlags, GpuKernelAttrs, MethodAnnotations,
};
use crate::lowering::lower_method_with_pool;

/// The compute capability every golden is rendered for.
///
/// Fixed rather than swept: `target.rs` already owns the question of
/// which `.version`/`.target` pair a capability produces and tests it
/// directly (`every_rendered_header_is_self_consistent`). Sweeping it
/// here would multiply the file count by the target table to re-test
/// something else's invariant. `sm_75` is the card every measurement in
/// `docs/gpu/` was taken on, so it is the one whose text is worth
/// pinning.
const GOLDEN_SM: (u32, u32) = (7, 5);

/// What lowering did with one method the analyzer admitted.
enum Outcome {
    /// Rendered PTX, recorded as its own `.ptx` file.
    Lowered(String),
    /// Admitted by the analyzer, refused by the emitter. Recorded in
    /// the manifest only — there is no text to keep.
    Refused(String),
}

struct Fixture {
    /// `Class__method__descriptor`, the golden file's stem.
    stem: String,
    /// The class file's major version, so a javac upgrade is
    /// distinguishable from a lowering regression.
    major: u16,
    outcome: Outcome,
}

/// The budgets the if-conversion characterisation sweeps.
///
/// `speculation_cost` scores a candidate and the budget is the cut-off,
/// so the set of budgets at which a fixture's text changes IS that
/// function's decision boundary for that fixture — recorded as data,
/// which is what makes a reimplementation provably equivalent rather
/// than assumed so.
///
/// The gate is `cost <= budget` where `cost` is the two arms' scores
/// summed, so the sweep is dense at the bottom — where one instruction
/// of difference moves the answer — and sparse above. `8` is
/// `DEFAULT_IF_CONVERSION_BUDGET` (the value `CRATONVM_GPU_IF_CONVERT=1`
/// selects); `16` is one expensive instruction's weight; `1024` admits
/// everything the refusal set does not already reject, so a fixture
/// unchanged at `1024` is one `speculation_cost` REFUSED outright.
/// That is a different fact from "too expensive" and worth being able
/// to read off the manifest.
///
/// Budgets deciding alike share one golden — see
/// [`if_conversion_profile`].
const IF_CONVERT_BUDGETS: [u32; 8] = [1, 2, 3, 4, 8, 16, 64, 1024];

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("JIT_CUDA_FIXTURE_DIR"))
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("ptx")
}

fn blessing() -> bool {
    std::env::var("GPU_GOLDEN_BLESS").is_ok_and(|v| v == "1")
}

/// Turn a method descriptor into something that is a legal file name on
/// every platform this repo builds on. `(`, `)`, `[`, `/` and `;` are
/// either illegal on Windows or awkward in a shell.
fn sanitise(descriptor: &str) -> String {
    descriptor
        .chars()
        .map(|c| match c {
            '(' | ')' | '[' | '/' | ';' => '_',
            other => other,
        })
        .collect()
}

/// Every `.class` under the fixture directory, **recursively**, sorted
/// so the manifest is stable across filesystems that do not enumerate
/// in order.
///
/// Recursion is not incidental. `build.rs` compiles
/// `tools/test_classes/gpu/annotations/*.java` — which declare
/// `package gpu.annotations;` — into a nested `gpu/annotations/`
/// subtree, and those are the ONLY fixtures carrying a real
/// `@GpuKernel(admit = ...)` / `@GpuExclude`. A flat `read_dir` silently
/// skipped all nine and the harness still reported 128 files of
/// coverage. Exactly the failure this module's own doc warns about, met
/// on the first run.
fn fixture_classes() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("failed to list fixture dir {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("fixture dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "class") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&fixture_dir(), &mut out);
    out.sort();
    out
}

/// Every admission loosening at once.
///
/// Used for the second pass over methods the annotation-faithful pass
/// rejects. `EligibleMathKernel`, `EligibleFrem` and `FloatRemainder`
/// carry no annotation at all by design — their own header comments say
/// the hint is supplied by the Rust test code so the fixture needs no
/// annotation classpath — so without this pass the `Math`/`StrictMath`
/// intrinsic table and the `frem`/`drem` identity, two of the three
/// hardest things this emitter lowers, have no golden.
const ALL_LOOSENINGS: AdmissionFlags = AdmissionFlags {
    allocation: true,
    unguarded_integer_division: true,
    approximate_float_remainder: true,
    intrinsic_calls: true,
};

/// `real` with every admission loosening turned on, but its
/// `@GpuExclude` left exactly as written.
///
/// Dropping the exclusion would be a bug, not a widening: `@GpuExclude`
/// is the developer saying "never offload this", and the first run of
/// the loosened pass duly produced goldens for `ExcludedKernel` and
/// `ExcludedAndKernel` — kernels no VM will ever lower. A golden for
/// unreachable code is worse than no golden, because it looks like
/// coverage.
fn loosened_annotations(real: &MethodAnnotations) -> MethodAnnotations {
    MethodAnnotations {
        gpu_kernel: Some(GpuKernelAttrs {
            admit: ALL_LOOSENINGS,
            ..real.gpu_kernel.clone().unwrap_or_default()
        }),
        gpu_exclude: real.gpu_exclude.clone(),
    }
}

/// Parse one class and lower every method the analyzer admits.
///
/// Mirrors `analyzer::load_method_with_pool`'s force-decode dance: the
/// reader leaves every attribute a `LazyAttribute::Raw`, and
/// `ClassFileMethod::code()` returns `None` until `Code` has been
/// decoded — without this the analyzer would reject every fixture with
/// `Reason::NoCode` and this file would record an empty manifest while
/// reporting success.
fn lower_class(path: &Path) -> Vec<Fixture> {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()));
    let mut class = cratonvm_reader::class_reader::read_class(&bytes)
        .unwrap_or_else(|e| panic!("failed to parse fixture {}: {e:?}", path.display()));

    {
        let cp = &class.constant_pool;
        for method in class.methods.iter_mut() {
            for attr in method.attributes.iter_mut() {
                if attr.name() == "Code" {
                    let _ = attr.decode(cp);
                }
            }
        }
    }

    // The class name is the path relative to the fixture root, minus
    // `.class`, in internal form — `EligibleVectorAdd`, or
    // `gpu/annotations/AdmitMathSqrt`. That is what the VM passes to
    // `lower_method_with_pool`, and `mangle` folds it into the entry
    // point, so taking only the file stem would record a kernel under a
    // name no launch would ever resolve.
    let class_name = path
        .strip_prefix(fixture_dir())
        .unwrap_or(path)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let major = class.version.major;

    // Partial moves: the decode borrow above has ended, so both fields
    // can leave `class` independently.
    let cp = class.constant_pool;
    let methods = class.methods;

    let mut out = Vec::new();
    for method in methods {
        // Feed the analyzer the method's `@GpuKernel` annotations, the
        // way the VM does. Without them the admission loosenings
        // (`ALLOW_DIV_BY_ZERO`, `ALLOW_INTRINSIC_CALLS`) are never on,
        // and `frem`/`drem`, the `Math`/`StrictMath` intrinsic table
        // and the half-precision path — the three hardest things this
        // emitter lowers — are absent from the golden set while it
        // still reports 128 files of coverage. That is the shape of
        // vacuous coverage `docs/gpu/lowering-architecture.md` warns
        // about, so the harness takes the VM's path, not the shortest
        // one.
        let decoded = decode_method_attrs(&method.attributes, &cp);
        let anns = read_method_annotations(&decoded, &cp);
        // Two passes. First the method as written, which is what the VM
        // sees. Then, only if that rejects, the same method with every
        // loosening on — recorded under a `@loosened` suffix so the
        // manifest never claims the VM admits something it does not.
        let (sig, suffix) = match analyze_with_annotations_and_pool(&method, &anns, &cp) {
            OffloadVerdict::Eligible(sig) => (sig, ""),
            OffloadVerdict::Rejected(_) => {
                match analyze_with_annotations_and_pool(&method, &loosened_annotations(&anns), &cp)
                {
                    OffloadVerdict::Eligible(sig) => (sig, "@loosened"),
                    OffloadVerdict::Rejected(_) => continue,
                }
            }
        };
        let stem = format!(
            "{}__{}__{}{suffix}",
            class_name.replace('/', "_"),
            method.name,
            sanitise(&method.descriptor)
        );
        let outcome =
            match lower_method_with_pool(&class_name, &method, &cp, &sig, GOLDEN_SM.0, GOLDEN_SM.1)
            {
                Ok(module) => Outcome::Lowered(module.render()),
                Err(e) => Outcome::Refused(e.to_string()),
            };
        if let Outcome::Lowered(base) = &outcome {
            for (budgets, converted) in if_conversion_profile(&class_name, &method, &cp, &sig, base)
            {
                out.push(Fixture {
                    stem: format!("{stem}@ifconv{budgets}"),
                    major,
                    outcome: converted,
                });
            }
        }
        out.push(Fixture {
            stem,
            major,
            outcome,
        });
    }
    out
}

/// Which budgets in [`IF_CONVERT_BUDGETS`] change this method's
/// rendered PTX, and record the changed text as its own golden.
///
/// # Why a sweep rather than a value
///
/// `speculation_cost` (`lowering/emit.rs`) scores a branch-to-`selp`
/// candidate by scanning the emitted TEXT — `str::contains` over a
/// weight table keyed on mnemonic substrings, plus a refusal set of
/// substrings (`bra `, `ld.global`, `atom.`, `ret;`, a leading `@`, a
/// trailing `:`). It is one of the four text scanners the IR refactor
/// retires, and reimplementing it as a `matches!` over instruction
/// variants is the change most likely to silently alter a decision.
///
/// It cannot be tested directly: it is a private `fn` taking `&str`,
/// and after the refactor there will be no `&str` to give it. What is
/// observable is its EFFECT — whether a candidate was converted — and
/// the budget is the cut-off it is compared against. So sweeping the
/// budget and recording where the output changes pins the function's
/// decision boundary in terms that survive the refactor.
///
/// The transform is off by default and its budget is latched in a
/// `OnceLock`, so this drives `lower_method_with_pool_impl` directly
/// rather than through the environment: a process can only ever read
/// that variable once, which would cap this sweep at one value and make
/// the test order-dependent besides.
fn if_conversion_profile(
    class_name: &str,
    method: &cratonvm_reader::method::ClassFileMethod,
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    sig: &crate::signature::KernelSignature,
    base: &str,
) -> Vec<(String, Outcome)> {
    // Key on the rendered text (or the refusal message) so budgets that
    // decide alike collapse to one entry. The first sweep recorded four
    // byte-identical copies of every converted fixture: waste, and a
    // worse record besides, because the point is WHERE the decision
    // changes and four identical files say nothing about that.
    let mut groups: Vec<(String, Outcome, Vec<u32>)> = Vec::new();
    for budget in IF_CONVERT_BUDGETS {
        let lowered = crate::lowering::lower_method_with_pool_impl(
            class_name,
            method,
            Some(cp),
            sig,
            GOLDEN_SM.0,
            GOLDEN_SM.1,
            Some(budget),
        );
        let (key, outcome) = match lowered {
            // Identical to the default lowering: nothing was converted
            // at this budget, so there is nothing to characterise.
            Ok(module) if module.render() == base => continue,
            Ok(module) => {
                let text = module.render();
                (text.clone(), Outcome::Lowered(text))
            }
            // A refusal under a budget the default lowering accepted is
            // itself a behaviour change, so it is recorded rather than
            // skipped.
            Err(e) => {
                let msg = e.to_string();
                (format!("refused:{msg}"), Outcome::Refused(msg))
            }
        };
        match groups.iter_mut().find(|(k, _, _)| *k == key) {
            Some((_, _, budgets)) => budgets.push(budget),
            None => groups.push((key, outcome, vec![budget])),
        }
    }
    groups
        .into_iter()
        .map(|(_, outcome, budgets)| {
            let label = budgets
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join("-");
            (label, outcome)
        })
        .collect()
}

/// Decode each lazy attribute so `read_method_annotations` can read
/// it, dropping any that fail.
///
/// A clone of `vm::runtime::offload::decode_method_attrs`, which is in
/// a crate this one does not (and must not) depend on. Best-effort
/// decoding is deliberate and matches that function: `test_support`
/// records that force-decoding every attribute can hit a `ByteView`
/// range panic on certain annotation attributes, and an annotation the
/// reader cannot decode should cost the kernel its loosening, not the
/// whole run.
fn decode_method_attrs(
    attrs: &[cratonvm_reader::attribute::LazyAttribute],
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Vec<cratonvm_reader::attribute::Attribute> {
    attrs
        .iter()
        .filter_map(|la| {
            let mut cloned = la.clone();
            cloned.decode(cp).ok().cloned()
        })
        .collect()
}

fn lower_every_fixture() -> Vec<Fixture> {
    let mut out: Vec<Fixture> = fixture_classes()
        .iter()
        .flat_map(|p| lower_class(p))
        .collect();
    out.sort_by(|a, b| a.stem.cmp(&b.stem));
    out
}

/// `stem \t major \t ok|refused: <message>`, one line per admitted
/// method, sorted.
///
/// The manifest is what makes a DISAPPEARING kernel visible: a `.ptx`
/// file that stops being generated would otherwise just sit on disk
/// unread, and the suite would report success over a shrinking set.
fn render_manifest(fixtures: &[Fixture]) -> String {
    let mut s = String::from(
        "# Generated by `GPU_GOLDEN_BLESS=1 cargo test -p cratonvm-jit-cuda golden`.\n\
         # One line per method the analyzer admits, sorted.\n\
         # Columns: stem, class-file major version, lowering outcome.\n",
    );
    for f in fixtures {
        let outcome = match &f.outcome {
            Outcome::Lowered(_) => "ok".to_string(),
            Outcome::Refused(msg) => format!("refused: {msg}"),
        };
        s.push_str(&format!("{}\t{}\t{}\n", f.stem, f.major, outcome));
    }
    s
}

fn bless(fixtures: &[Fixture]) {
    let dir = golden_dir();
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", dir.display()));

    // Drop stale `.ptx` files rather than leaving them: a kernel that
    // stops lowering must not leave a golden behind that still looks
    // like coverage.
    let keep: BTreeSet<&str> = fixtures
        .iter()
        .filter(|f| matches!(f.outcome, Outcome::Lowered(_)))
        .map(|f| f.stem.as_str())
        .collect();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "ptx") {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if !keep.contains(stem) {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }

    for f in fixtures {
        if let Outcome::Lowered(text) = &f.outcome {
            let path = dir.join(format!("{}.ptx", f.stem));
            std::fs::write(&path, text)
                .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
        }
    }
    let manifest = dir.join("manifest.txt");
    std::fs::write(&manifest, render_manifest(fixtures))
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", manifest.display()));
}

/// The whole point of the module.
///
/// Fails on any byte of difference in any rendered module, on any
/// change to the admitted set, and on any change to the refusal set.
#[test]
fn every_eligible_fixture_renders_the_recorded_ptx() {
    let fixtures = lower_every_fixture();
    assert!(
        !fixtures.is_empty(),
        "no fixture in {} was admitted by the analyzer — the fixture \
         directory is empty or `Code` was not force-decoded, and an \
         empty golden set would report success while testing nothing",
        fixture_dir().display()
    );

    if blessing() {
        bless(&fixtures);
    }

    let dir = golden_dir();
    let manifest_path = dir.join("manifest.txt");
    let recorded_manifest = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
        panic!(
            "no golden manifest at {} ({e}). Record one with \
             `GPU_GOLDEN_BLESS=1 cargo test -p cratonvm-jit-cuda golden`.",
            manifest_path.display()
        )
    });

    // A javac upgrade changes the bytecode this crate is handed, which
    // changes the PTX for reasons that are not a lowering regression.
    // Say so and stop, rather than reporting a diff that will be read
    // as one. The manifest carries the major version for exactly this.
    let recorded_major: Option<u16> = recorded_manifest
        .lines()
        .find(|l| !l.starts_with('#'))
        .and_then(|l| l.split('\t').nth(1))
        .and_then(|m| m.parse().ok());
    let observed_major = fixtures[0].major;
    if let Some(recorded) = recorded_major {
        if recorded != observed_major {
            eprintln!(
                "SKIPPING golden comparison: fixtures were recorded from \
                 class-file major {recorded} and this toolchain produced \
                 major {observed_major}. Goldens depend on javac's output, \
                 so this is a toolchain change, not a lowering change. \
                 Re-record with GPU_GOLDEN_BLESS=1 after confirming the \
                 diff is only what the new javac emits."
            );
            return;
        }
    }

    assert_eq!(
        render_manifest(&fixtures),
        recorded_manifest,
        "the set of methods the analyzer admits, or what lowering does \
         with them, has changed.\n\nIf that is intended, re-record with \
         `GPU_GOLDEN_BLESS=1 cargo test -p cratonvm-jit-cuda golden` and \
         review the manifest diff — a method moving between `ok` and \
         `refused` is a behaviour change, not a formatting one."
    );

    let mut diverged = Vec::new();
    for f in &fixtures {
        let Outcome::Lowered(text) = &f.outcome else {
            continue;
        };
        let path = dir.join(format!("{}.ptx", f.stem));
        match std::fs::read_to_string(&path) {
            Ok(recorded) if recorded == *text => {}
            Ok(recorded) => diverged.push(format!(
                "{}:\n{}",
                f.stem,
                first_difference(&recorded, text)
            )),
            Err(e) => diverged.push(format!("{}: golden missing ({e})", f.stem)),
        }
    }

    assert!(
        diverged.is_empty(),
        "{} rendered module(s) differ from the recorded PTX:\n\n{}\n\n\
         This file exists to make that visible. If the change is \
         intended, re-record with `GPU_GOLDEN_BLESS=1 cargo test -p \
         cratonvm-jit-cuda golden` and read the diff — a lowering \
         refactor that is supposed to preserve behaviour should produce \
         no diff at all.",
        diverged.len(),
        diverged.join("\n\n")
    );
}

/// The first differing line, with the recorded and actual text beside
/// each other.
///
/// A whole-module diff is unreadable in test output, and the first
/// divergence is almost always the cause: everything after it is
/// downstream register renumbering saying the same thing again.
fn first_difference(recorded: &str, actual: &str) -> String {
    for (i, (r, a)) in recorded.lines().zip(actual.lines()).enumerate() {
        if r != a {
            return format!("  line {}:\n    recorded: {r}\n    actual:   {a}\n", i + 1);
        }
    }
    let (rn, an) = (recorded.lines().count(), actual.lines().count());
    format!(
        "  identical for {} lines, then recorded has {rn} and actual has {an}\n",
        rn.min(an)
    )
}
