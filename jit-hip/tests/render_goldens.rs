// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Render every recorded PTX golden's kernel as HIP C++, and record
//! both what came out and what was refused.
//!
//! # What this is and is not
//!
//! It is a change detector over the HIP renderer and an honest ledger
//! of its coverage. It is **not** a correctness test: nothing here
//! compiles or runs the emitted C++. `cuda-bridge`'s `hip_module_it`
//! does that on hardware, and the host-shim oracle does it without.
//!
//! # Why the refusals are recorded, not skipped
//!
//! `jit-hip` refuses any instruction the front end has not expressed
//! structurally yet (`Instr::Raw`). That refusal set IS the coverage
//! number, and writing it down is what stops "the HIP backend works"
//! from quietly meaning "on the four kernels somebody tried". Each
//! entry names the first PTX line that has no HIP form, which is also
//! the next emission site worth converting.
//!
//! Re-record with:
//!
//! ```text
//! GPU_GOLDEN_BLESS=1 cargo test -p cratonvm-jit-hip
//! ```

use std::path::{Path, PathBuf};

use cratonvm_jit_cuda::analyzer::{analyze_with_annotations_and_pool, OffloadVerdict};
use cratonvm_jit_cuda::annotations::{
    read_method_annotations, AdmissionFlags, GpuKernelAttrs, MethodAnnotations,
};
use cratonvm_jit_cuda::lowering::lower_method_with_pool;
use cratonvm_jit_hip::{render_module, HipTarget};

/// The architecture the goldens are rendered for.
///
/// One RDNA part, fixed. `wavefront_for_arch` is tested directly in
/// `target.rs`; sweeping architectures here would multiply the file
/// count to re-test something else's invariant. `gfx1100` is the
/// cheapest officially ROCm-supported consumer target, so it is the
/// one a first AMD box is most likely to be.
const GOLDEN_ARCH: &str = "gfx1100";

/// The compute capability the PTX side lowers for. Must match
/// `jit_cuda::golden`'s, or the two sets describe different kernels.
const GOLDEN_SM: (u32, u32) = (7, 5);

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("JIT_CUDA_FIXTURE_DIR"))
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("hip")
}

fn blessing() -> bool {
    std::env::var("GPU_GOLDEN_BLESS").is_ok_and(|v| v == "1")
}

fn sanitise(descriptor: &str) -> String {
    descriptor
        .chars()
        .map(|c| match c {
            '(' | ')' | '[' | '/' | ';' => '_',
            other => other,
        })
        .collect()
}

fn fixture_classes() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
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

/// Every admission loosening, preserving `@GpuExclude`.
///
/// Mirrors `jit_cuda::golden` exactly, so the two golden sets cover the
/// same methods and a coverage number computed from one applies to the
/// other.
fn loosened(real: &MethodAnnotations) -> MethodAnnotations {
    MethodAnnotations {
        gpu_kernel: Some(GpuKernelAttrs {
            admit: AdmissionFlags {
                allocation: true,
                unguarded_integer_division: true,
                approximate_float_remainder: true,
                intrinsic_calls: true,
            },
            ..real.gpu_kernel.clone().unwrap_or_default()
        }),
        gpu_exclude: real.gpu_exclude.clone(),
    }
}

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

enum Outcome {
    Rendered(String),
    Refused(String),
}

struct Entry {
    stem: String,
    outcome: Outcome,
}

fn render_class(path: &Path) -> Vec<Entry> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(mut class) = cratonvm_reader::class_reader::read_class(&bytes) else {
        return Vec::new();
    };
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
    let class_name = path
        .strip_prefix(fixture_dir())
        .unwrap_or(path)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let cp = class.constant_pool;
    let methods = class.methods;

    let target = HipTarget::from_arch(GOLDEN_ARCH, true, true).expect("gfx1100 is in the table");
    let mut out = Vec::new();
    for method in methods {
        let decoded = decode_method_attrs(&method.attributes, &cp);
        let anns = read_method_annotations(&decoded, &cp);
        let (sig, suffix) = match analyze_with_annotations_and_pool(&method, &anns, &cp) {
            OffloadVerdict::Eligible(sig) => (sig, ""),
            OffloadVerdict::Rejected(_) => {
                match analyze_with_annotations_and_pool(&method, &loosened(&anns), &cp) {
                    OffloadVerdict::Eligible(sig) => (sig, "@loosened"),
                    OffloadVerdict::Rejected(_) => continue,
                }
            }
        };
        let Ok(module) =
            lower_method_with_pool(&class_name, &method, &cp, &sig, GOLDEN_SM.0, GOLDEN_SM.1)
        else {
            // Refused by the PTX emitter too; `jit_cuda::golden`'s
            // manifest already records why, and repeating it here would
            // be two places to update.
            continue;
        };
        let stem = format!(
            "{}__{}__{}{suffix}",
            class_name.replace('/', "_"),
            method.name,
            sanitise(&method.descriptor)
        );
        let outcome = match render_module(&module, &target) {
            Ok(m) => Outcome::Rendered(m.source),
            Err(e) => Outcome::Refused(e.to_string()),
        };
        out.push(Entry { stem, outcome });
    }
    out
}

fn render_every_fixture() -> Vec<Entry> {
    let mut out: Vec<Entry> = fixture_classes()
        .iter()
        .flat_map(|p| render_class(p))
        .collect();
    out.sort_by(|a, b| a.stem.cmp(&b.stem));
    out
}

/// `stem \t rendered|refused: <why>`, sorted.
///
/// The refusal lines are the point. Each one names the first PTX
/// instruction with no HIP form, and the coverage header at the top
/// turns "the HIP backend works" into a number somebody has to look at.
fn render_manifest(entries: &[Entry]) -> String {
    let rendered = entries
        .iter()
        .filter(|e| matches!(e.outcome, Outcome::Rendered(_)))
        .count();
    let mut s = format!(
        "# Generated by `GPU_GOLDEN_BLESS=1 cargo test -p cratonvm-jit-hip`.\n\
         # HIP coverage: {rendered} of {} kernels the PTX path lowers.\n\
         # A `refused` line names the first instruction with no structured\n\
         # form yet -- converting that emission site in jit-cuda is what\n\
         # moves the number.\n",
        entries.len()
    );
    for e in entries {
        match &e.outcome {
            Outcome::Rendered(_) => s.push_str(&format!("{}\trendered\n", e.stem)),
            Outcome::Refused(why) => {
                // One line: the message already carries the kernel name.
                let why = why.replace('\n', " ");
                s.push_str(&format!("{}\trefused: {why}\n", e.stem));
            }
        }
    }
    s
}

fn bless(entries: &[Entry]) {
    let dir = golden_dir();
    std::fs::create_dir_all(&dir).expect("create golden dir");
    let keep: std::collections::BTreeSet<&str> = entries
        .iter()
        .filter(|e| matches!(e.outcome, Outcome::Rendered(_)))
        .map(|e| e.stem.as_str())
        .collect();
    if let Ok(read) = std::fs::read_dir(&dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "hip") {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if !keep.contains(stem) {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    for e in entries {
        if let Outcome::Rendered(src) = &e.outcome {
            std::fs::write(dir.join(format!("{}.hip", e.stem)), src).expect("write golden");
        }
    }
    std::fs::write(dir.join("manifest.txt"), render_manifest(entries)).expect("write manifest");
}

#[test]
fn every_lowered_fixture_renders_the_recorded_hip() {
    let entries = render_every_fixture();
    assert!(
        !entries.is_empty(),
        "no fixture lowered at all -- the fixture directory is empty, or \
         `Code` was not force-decoded, and an empty set would report \
         success while testing nothing"
    );

    if blessing() {
        bless(&entries);
    }

    let dir = golden_dir();
    let manifest_path = dir.join("manifest.txt");
    let recorded = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
        panic!(
            "no HIP manifest at {} ({e}). Record one with \
             `GPU_GOLDEN_BLESS=1 cargo test -p cratonvm-jit-hip`.",
            manifest_path.display()
        )
    });

    assert_eq!(
        render_manifest(&entries),
        recorded,
        "the set of kernels the HIP renderer accepts, or why it refuses one, \
         has changed.\n\nA kernel moving from `refused` to `rendered` is the \
         intended direction and means an emission site was converted; the \
         reverse is a regression. Re-record with `GPU_GOLDEN_BLESS=1 cargo \
         test -p cratonvm-jit-hip`."
    );

    let mut diverged = Vec::new();
    for e in &entries {
        let Outcome::Rendered(src) = &e.outcome else {
            continue;
        };
        let path = dir.join(format!("{}.hip", e.stem));
        match std::fs::read_to_string(&path) {
            Ok(rec) if rec == *src => {}
            Ok(rec) => diverged.push(format!("{}:\n{}", e.stem, first_difference(&rec, src))),
            Err(err) => diverged.push(format!("{}: golden missing ({err})", e.stem)),
        }
    }
    assert!(
        diverged.is_empty(),
        "{} rendered kernel(s) differ from the recorded HIP:\n\n{}",
        diverged.len(),
        diverged.join("\n\n")
    );
}

/// At least one real kernel must render, or the renderer is a
/// well-tested no-op.
///
/// Named for the shape rather than a specific fixture: the point is
/// that the elementwise path — a guarded counted loop over primitive
/// arrays, which is what the analyzer admits — reaches HIP.
#[test]
fn the_elementwise_shape_reaches_hip() {
    let entries = render_every_fixture();
    let rendered: Vec<&str> = entries
        .iter()
        .filter(|e| matches!(e.outcome, Outcome::Rendered(_)))
        .map(|e| e.stem.as_str())
        .collect();
    assert!(
        rendered.iter().any(|s| s.contains("EligibleVectorAdd")),
        "the canonical elementwise kernel does not render; HIP coverage is \
         {} of {} and none of it is the shape this backend exists for.\n\
         Rendered: {rendered:?}",
        rendered.len(),
        entries.len()
    );
}

fn first_difference(recorded: &str, actual: &str) -> String {
    for (i, (r, a)) in recorded.lines().zip(actual.lines()).enumerate() {
        if r != a {
            return format!("  line {}:\n    recorded: {r}\n    actual:   {a}\n", i + 1);
        }
    }
    let (rn, an) = (recorded.lines().count(), actual.lines().count());
    format!("  identical for {} lines, then {rn} vs {an}\n", rn.min(an))
}
