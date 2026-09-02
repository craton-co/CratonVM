// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Reader for the `craton.gpu.*` annotations on Java methods and classes.
//!
//! Phase-1 spec §2.2 / §2.3. This module is **leaf-level** — every other
//! Rust-side change consumes the types defined here.
//!
//! Recognised annotation type descriptors:
//!
//! - `Lcraton/gpu/GpuKernel;` — opt-in marker on a static method.
//! - `Lcraton/gpu/GpuExclude;` — opt-out marker (highest priority).
//! - `Lcraton/gpu/EnableGpuAsync;` — class-level marker permitting async
//!   dispatch with an optional warmup count.
//!
//! The reader walks the already-decoded
//! [`cratonvm_reader::attribute::Attribute::RuntimeInvisibleAnnotations`]
//! / `RuntimeVisibleAnnotations` slices — the class reader has already
//! split the JVMS §4.7.16 byte stream into structured `Annotation` +
//! `ElementValue` values, so this layer does no raw-byte parsing.
//!
//! All getters are total — a missing element pair simply means "use the
//! default value" (per §2.1). Unknown annotation descriptors are
//! silently ignored.

use cratonvm_reader::attribute::{Annotation, Attribute, ElementValue};
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

// ---------------------------------------------------------------------
// Public leaf enums
// ---------------------------------------------------------------------

/// Grid shape for a GPU kernel — currently only the elementwise shape
/// is wired through the analyzer / launcher. Reserved variants are
/// recognised at parse time so a future emitter pass can route on them
/// without reparsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GridShape {
    /// Default: one CUDA thread per array element. The kernel's iteration
    /// space is the length of the longest array parameter.
    #[default]
    Elementwise,
    /// Row-per-thread shape — each CUDA thread handles a row of a 2D output.
    RowPerThread,
    /// Block-reduction shape — block-wide reductions land here.
    BlockReduction,
    /// The annotation named a constant this crate does not know.
    ///
    /// AUDIT 2026-09-02. `parse_grid_shape` matches the Java enum by
    /// CONSTANT NAME, against a `craton.gpu.GridShape` that is versioned
    /// in a different repository (`craton-gpu-java`) and located at build
    /// time by environment variable, sibling checkout, or absolute path.
    /// An unrecognised name used to fall through to
    /// `GridShape::default()`, which is `Elementwise` — so renaming or
    /// adding a constant on the Java side would silently turn
    /// `grid = BLOCK_REDUCTION` into an element-wise kernel.
    ///
    /// That is precisely the failure AUDIT 2026-08-28 fixed by rejecting
    /// unimplemented grid shapes: a block reduction lowered element-wise
    /// produces wrong answers rather than an error, and a rejection would
    /// merely have run the method on the CPU. Defaulting an unknown name
    /// re-opened it through the name channel. Landing here instead sends
    /// the method back through the same rejection, which is the only
    /// honest answer to "the user asked for a shape we cannot name".
    Unknown,
}

/// How aggressively the analyzer should admit a `@GpuKernel`-marked
/// method. The default is `Strict`: any opcode the elementwise emitter
/// can't faithfully express rejects the method back to the CPU. Looser
/// modes let the user opt in to incomplete lowerings (e.g. a stub that
/// allocates inside the kernel) — useful for experiments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdmissionHint {
    /// Default — reject any non-elementwise pattern.
    #[default]
    Strict,
    /// Allow primitive-array `new int[n]` where `n` is derived from a
    /// method parameter — the emitter treats it as the output buffer.
    AllowAllocation,
    /// Skip the implicit divisor-zero guard on idiv/ldiv/irem/lrem.
    AllowDivByZero,
    /// Allow `invokestatic` to the five Math.{sqrt,sin,cos,exp,log}(D)D
    /// intrinsics. Other invoke variants still reject.
    AllowIntrinsicCalls,
}

// ---------------------------------------------------------------------
// Public attribute structs — per spec §2.3
// ---------------------------------------------------------------------

/// Parsed `@craton.gpu.GpuKernel` attributes. Every field has a default
/// matching §2.1 so callers may treat a missing pair identically to a
/// pair set to the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuKernelAttrs {
    pub grid: GridShape,
    pub block_x: u32,
    pub block_y: u32,
    pub block_z: u32,
    pub shared_bytes: u32,
    pub admit: AdmissionHint,
}

impl Default for GpuKernelAttrs {
    fn default() -> Self {
        // Spec §2.1 defaults: 0 = "JVM picks the block dimension".
        Self {
            grid: GridShape::Elementwise,
            block_x: 0,
            block_y: 0,
            block_z: 0,
            shared_bytes: 0,
            admit: AdmissionHint::Strict,
        }
    }
}

/// Parsed `@craton.gpu.GpuExclude` attributes. A non-empty `reason`
/// surfaces in `--print-gpu-decisions` output so users can see why a
/// method was force-excluded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GpuExcludeAttrs {
    pub reason: String,
}

/// Parsed `@craton.gpu.EnableGpuAsync` attributes (class-level).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EnableAsyncAttrs {
    pub warmup: u32,
}

/// What we extracted from a single method's annotation set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MethodAnnotations {
    /// Present iff the method carries `@GpuKernel`.
    pub gpu_kernel: Option<GpuKernelAttrs>,
    /// Present iff the method carries `@GpuExclude`. Takes precedence
    /// over `gpu_kernel` (the analyzer must check this first).
    pub gpu_exclude: Option<GpuExcludeAttrs>,
}

/// What we extracted from a class's annotation set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClassAnnotations {
    /// Present iff the class carries `@EnableGpuAsync`.
    pub enable_async: Option<EnableAsyncAttrs>,
}

// ---------------------------------------------------------------------
// Annotation type descriptors
// ---------------------------------------------------------------------

const GPU_KERNEL_DESC: &str = "Lcraton/gpu/GpuKernel;";
const GPU_EXCLUDE_DESC: &str = "Lcraton/gpu/GpuExclude;";
const ENABLE_GPU_ASYNC_DESC: &str = "Lcraton/gpu/EnableGpuAsync;";

// ---------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------

/// Read the GPU-relevant annotations off a method.
///
/// Walks every `RuntimeInvisible`/`RuntimeVisible` annotation attribute
/// (both are accepted for forward-compat) and classifies by the
/// `type_index`'s UTF-8 descriptor. Unknown annotation types are
/// ignored. Missing pairs default per §2.1.
pub fn read_method_annotations(
    method_attributes: &[Attribute],
    cp: &ConstantPool,
) -> MethodAnnotations {
    let mut out = MethodAnnotations::default();
    for ann in iter_annotations(method_attributes) {
        let Some(desc) = cp.get_utf8(ann.type_index) else {
            continue;
        };
        match desc {
            GPU_KERNEL_DESC => {
                out.gpu_kernel = Some(parse_gpu_kernel(ann, cp));
            }
            GPU_EXCLUDE_DESC => {
                out.gpu_exclude = Some(parse_gpu_exclude(ann, cp));
            }
            _ => {}
        }
    }
    out
}

/// Read the GPU-relevant annotations off a class from its raw
/// `Attribute` table. Used when the caller only has unparsed
/// attributes available (e.g. in tests).
pub fn read_class_annotations(
    class_attributes: &[Attribute],
    cp: &ConstantPool,
) -> ClassAnnotations {
    let mut out = ClassAnnotations::default();
    for ann in iter_annotations(class_attributes) {
        let Some(desc) = cp.get_utf8(ann.type_index) else {
            continue;
        };
        if desc == ENABLE_GPU_ASYNC_DESC {
            out.enable_async = Some(parse_enable_gpu_async(ann, cp));
        }
    }
    out
}

/// Read the GPU-relevant annotations off a class given a pre-parsed
/// `Annotation` slice. Used when the caller has a runtime `Class` in
/// hand (the classloader stores the annotations pre-parsed there).
pub fn read_class_annotations_from_parsed(
    annotations: &[Annotation],
    cp: &ConstantPool,
) -> ClassAnnotations {
    let mut out = ClassAnnotations::default();
    for ann in annotations {
        let Some(desc) = cp.get_utf8(ann.type_index) else {
            continue;
        };
        if desc == ENABLE_GPU_ASYNC_DESC {
            out.enable_async = Some(parse_enable_gpu_async(ann, cp));
        }
    }
    out
}

// ---------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------

/// Iterate every annotation in every `RuntimeVisibleAnnotations` /
/// `RuntimeInvisibleAnnotations` attribute. Both kinds are accepted —
/// the spec admits either form for forward-compat.
fn iter_annotations(attributes: &[Attribute]) -> impl Iterator<Item = &Annotation> {
    attributes.iter().flat_map(|attr| {
        let v: &[Annotation] = match attr {
            Attribute::RuntimeInvisibleAnnotations(v) => v.as_slice(),
            Attribute::RuntimeVisibleAnnotations(v) => v.as_slice(),
            _ => &[],
        };
        v.iter()
    })
}

/// Helper: look up a pair by element name. Returns `None` if not
/// present.
fn find_pair<'a>(ann: &'a Annotation, name: &str, cp: &ConstantPool) -> Option<&'a ElementValue> {
    for pair in &ann.element_value_pairs {
        if cp.get_utf8(pair.element_name_index) == Some(name) {
            return Some(&pair.value);
        }
    }
    None
}

/// Resolve an `ElementValue` to an `i32`. Accepts CONSTANT_Integer for
/// the `'I'/'B'/'C'/'S'/'Z'` tags. Other tags / non-integer entries
/// yield `None` and the caller substitutes the default.
fn as_i32(value: &ElementValue, cp: &ConstantPool) -> Option<i32> {
    match value {
        ElementValue::Const {
            tag,
            const_value_index,
        } => match (*tag, cp.get(*const_value_index)) {
            (b'I' | b'B' | b'C' | b'S' | b'Z', Some(ConstantPoolEntry::Integer(v))) => Some(*v),
            _ => None,
        },
        _ => None,
    }
}

/// Resolve an `ElementValue` to a `String` (the `'s'` tag points at a
/// CONSTANT_Utf8 entry per JVMS §4.7.16.1).
fn as_string(value: &ElementValue, cp: &ConstantPool) -> Option<String> {
    match value {
        ElementValue::Const {
            tag,
            const_value_index,
        } if *tag == b's' => cp.get_utf8(*const_value_index).map(str::to_owned),
        _ => None,
    }
}

/// Resolve an enum-tagged `ElementValue` to its simple constant name
/// (e.g. `"ALLOW_ALLOCATION"`).
fn as_enum_const_name<'a>(value: &'a ElementValue, cp: &'a ConstantPool) -> Option<&'a str> {
    match value {
        ElementValue::Enum {
            const_name_index, ..
        } => cp.get_utf8(*const_name_index),
        _ => None,
    }
}

/// The Java constant names of `craton.gpu.GridShape`, in the order the
/// enum declares them.
///
/// Named here rather than inline in the match so
/// `rust_enum_names_match_the_java_definitions` can compare this list
/// against the compiled `.class` file and fail when the two drift.
pub(crate) const GRID_SHAPE_NAMES: [&str; 3] =
    ["ELEMENTWISE", "ROW_PER_THREAD", "BLOCK_REDUCTION"];

fn parse_grid_shape(value: &ElementValue, cp: &ConstantPool) -> GridShape {
    match as_enum_const_name(value, cp) {
        Some("ELEMENTWISE") => GridShape::Elementwise,
        Some("ROW_PER_THREAD") => GridShape::RowPerThread,
        Some("BLOCK_REDUCTION") => GridShape::BlockReduction,
        // NOT `GridShape::default()` — see `GridShape::Unknown`.
        _ => GridShape::Unknown,
    }
}

/// The Java constant names of `craton.gpu.AdmissionHint`, in the order
/// the enum declares them. See [`GRID_SHAPE_NAMES`].
pub(crate) const ADMISSION_HINT_NAMES: [&str; 4] = [
    "STRICT",
    "ALLOW_ALLOCATION",
    "ALLOW_DIV_BY_ZERO",
    "ALLOW_INTRINSIC_CALLS",
];

fn parse_admission_hint(value: &ElementValue, cp: &ConstantPool) -> AdmissionHint {
    match as_enum_const_name(value, cp) {
        Some("STRICT") => AdmissionHint::Strict,
        Some("ALLOW_ALLOCATION") => AdmissionHint::AllowAllocation,
        Some("ALLOW_DIV_BY_ZERO") => AdmissionHint::AllowDivByZero,
        Some("ALLOW_INTRINSIC_CALLS") => AdmissionHint::AllowIntrinsicCalls,
        // Unlike `parse_grid_shape`, defaulting is safe here: `Strict` is
        // the STRICTEST hint, so an unrecognised name loses coverage (a
        // method that would have been admitted is not) and can never
        // admit something the emitter cannot lower. The contract test
        // catches the drift either way.
        _ => AdmissionHint::default(),
    }
}

fn parse_gpu_kernel(ann: &Annotation, cp: &ConstantPool) -> GpuKernelAttrs {
    let mut out = GpuKernelAttrs::default();
    if let Some(v) = find_pair(ann, "grid", cp) {
        out.grid = parse_grid_shape(v, cp);
    }
    if let Some(v) = find_pair(ann, "blockX", cp) {
        if let Some(n) = as_i32(v, cp) {
            if n > 0 {
                out.block_x = n as u32;
            }
        }
    }
    if let Some(v) = find_pair(ann, "blockY", cp) {
        if let Some(n) = as_i32(v, cp) {
            if n > 0 {
                out.block_y = n as u32;
            }
        }
    }
    if let Some(v) = find_pair(ann, "blockZ", cp) {
        if let Some(n) = as_i32(v, cp) {
            if n > 0 {
                out.block_z = n as u32;
            }
        }
    }
    if let Some(v) = find_pair(ann, "sharedBytes", cp) {
        if let Some(n) = as_i32(v, cp) {
            if n >= 0 {
                out.shared_bytes = n as u32;
            }
        }
    }
    if let Some(v) = find_pair(ann, "admit", cp) {
        out.admit = parse_admission_hint(v, cp);
    }
    out
}

fn parse_gpu_exclude(ann: &Annotation, cp: &ConstantPool) -> GpuExcludeAttrs {
    let mut out = GpuExcludeAttrs::default();
    if let Some(v) = find_pair(ann, "reason", cp) {
        if let Some(s) = as_string(v, cp) {
            out.reason = s;
        }
    }
    out
}

fn parse_enable_gpu_async(ann: &Annotation, cp: &ConstantPool) -> EnableAsyncAttrs {
    let mut out = EnableAsyncAttrs::default();
    if let Some(v) = find_pair(ann, "warmup", cp) {
        if let Some(n) = as_i32(v, cp) {
            if n >= 0 {
                out.warmup = n as u32;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_reader::attribute::ElementValuePair;

    /// The Rust match arms and the Java enums must name the same
    /// constants.
    ///
    /// # Why this test is the load-bearing one for this file
    ///
    /// Every annotation value this crate understands is matched BY
    /// STRING against a `craton.gpu.*` enum that lives in a DIFFERENT
    /// repository (`craton-gpu-java`), located at build time by
    /// `$CRATON_GPU_JAVA_SRC`, a sibling checkout, or — on Windows — an
    /// absolute path. Nothing links the two versions. Renaming a Java
    /// constant, or adding one, breaks nothing that either side can see:
    /// javac still compiles, cargo still builds, and every existing test
    /// still passes.
    ///
    /// What changes is behaviour, silently. `parse_admission_hint` falls
    /// back to `Strict`, which merely loses coverage. `parse_grid_shape`
    /// used to fall back to `Elementwise`, which does not: it would turn
    /// `grid = BLOCK_REDUCTION` into an element-wise kernel and produce
    /// wrong answers — the exact failure AUDIT 2026-08-28 fixed by
    /// rejecting unimplemented shapes, re-opened through the name
    /// channel. That fallback is now `GridShape::Unknown`, which the
    /// analyzer rejects; this test is the other half, catching the drift
    /// at build time rather than discovering it as a wrong answer.
    ///
    /// # Skipping, loudly
    ///
    /// The annotation classes are only present when the build found the
    /// external project. When they are absent this test reports a skip
    /// and passes, because failing would make the whole crate untestable
    /// on a machine that has no reason to check out a second repository.
    /// It prints what it did either way, so a CI run that covered
    /// nothing says so instead of showing a green tick.
    #[test]
    fn rust_enum_names_match_the_java_definitions() {
        let dir = env!("CRATON_GPU_CLASSES_DIR");
        if dir.is_empty() {
            eprintln!(
                "SKIP rust_enum_names_match_the_java_definitions: the craton-gpu-java \
                 project was not found at build time, so there are no compiled \
                 annotation classes to check against. Set CRATON_GPU_JAVA_SRC to \
                 enable this check."
            );
            return;
        }
        let mut checked = 0usize;
        for (class, rust_names) in [
            ("GridShape", &super::GRID_SHAPE_NAMES[..]),
            ("AdmissionHint", &super::ADMISSION_HINT_NAMES[..]),
        ] {
            let path = std::path::Path::new(dir)
                .join("craton")
                .join("gpu")
                .join(format!("{class}.class"));
            if !path.is_file() {
                eprintln!(
                    "SKIP {class}: {} is not present, though the classes \
                     directory is",
                    path.display()
                );
                continue;
            }
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let class_file = cratonvm_reader::class_reader::read_class(&bytes)
                .unwrap_or_else(|e| panic!("parse {}: {e:?}", path.display()));
            // An enum's constants are its `static final` fields typed as
            // the enum itself. `$VALUES` is the synthetic array javac
            // adds and is typed as an array, so the descriptor match
            // excludes it without needing to know its name.
            let want_descriptor = format!("Lcraton/gpu/{class};");
            let mut java_names: Vec<String> = class_file
                .fields
                .iter()
                .filter(|f| {
                    f.is_static() && f.is_final() && &*f.descriptor == want_descriptor.as_str()
                })
                .map(|f| f.name.to_string())
                .collect();
            java_names.sort();
            let mut rust_sorted: Vec<String> =
                rust_names.iter().map(|s| s.to_string()).collect();
            rust_sorted.sort();
            assert_eq!(
                java_names,
                rust_sorted,
                "craton.gpu.{class} and this crate's parser disagree about the \
                 constant set.\n  Java: {java_names:?}\n  Rust: {rust_sorted:?}\n\
                 A name only Java has is a value this crate will not recognise; \
                 a name only Rust has is a match arm that can never fire. Update \
                 the parser in annotations.rs and the list beside it.",
            );
            checked += 1;
        }
        eprintln!("rust_enum_names_match_the_java_definitions: checked {checked} enum(s)");
    }

    /// Tiny builder that hands out fresh constant-pool indices and
    /// produces a `ConstantPool` at the end. Tests use this instead of
    /// hand-counting slots.
    struct CpBuilder {
        entries: Vec<ConstantPoolEntry>,
    }

    impl CpBuilder {
        fn new() -> Self {
            Self {
                entries: vec![ConstantPoolEntry::Tombstone],
            }
        }
        fn utf8(&mut self, s: &str) -> u16 {
            let idx = self.entries.len() as u16;
            self.entries.push(ConstantPoolEntry::Utf8(s.into()));
            idx
        }
        fn integer(&mut self, v: i32) -> u16 {
            let idx = self.entries.len() as u16;
            self.entries.push(ConstantPoolEntry::Integer(v));
            idx
        }
        fn build(self) -> ConstantPool {
            ConstantPool::new(self.entries)
        }
    }

    /// Build a `RuntimeInvisibleAnnotations` attribute from one or more
    /// pre-built `Annotation`s.
    fn ria(annotations: Vec<Annotation>) -> Attribute {
        Attribute::RuntimeInvisibleAnnotations(annotations)
    }

    #[test]
    fn parse_gpu_kernel_default_admit() {
        // `@GpuKernel` with no element pairs — every field must be the
        // §2.1 default.
        let mut cp = CpBuilder::new();
        let desc = cp.utf8(GPU_KERNEL_DESC);
        let cp = cp.build();
        let ann = Annotation {
            type_index: desc,
            element_value_pairs: vec![],
        };
        let m = read_method_annotations(&[ria(vec![ann])], &cp);
        let k = m.gpu_kernel.expect("@GpuKernel must be detected");
        assert_eq!(k, GpuKernelAttrs::default());
        assert_eq!(k.grid, GridShape::Elementwise);
        assert_eq!(k.admit, AdmissionHint::Strict);
        // Spec §2.1: 0 means "JVM picks the block dimension".
        assert_eq!(k.block_x, 0);
        assert_eq!(k.block_y, 0);
        assert_eq!(k.block_z, 0);
        assert_eq!(k.shared_bytes, 0);
        assert!(m.gpu_exclude.is_none());
    }

    #[test]
    fn parse_gpu_kernel_with_allow_allocation() {
        // `@GpuKernel(admit = ALLOW_ALLOCATION)`.
        let mut cp = CpBuilder::new();
        let desc = cp.utf8(GPU_KERNEL_DESC);
        let admit_name = cp.utf8("admit");
        let admit_type = cp.utf8("Lcraton/gpu/AdmissionHint;");
        let admit_const = cp.utf8("ALLOW_ALLOCATION");
        let cp = cp.build();
        let ann = Annotation {
            type_index: desc,
            element_value_pairs: vec![ElementValuePair {
                element_name_index: admit_name,
                value: ElementValue::Enum {
                    type_name_index: admit_type,
                    const_name_index: admit_const,
                },
            }],
        };
        let m = read_method_annotations(&[ria(vec![ann])], &cp);
        let k = m.gpu_kernel.expect("@GpuKernel must be detected");
        assert_eq!(k.admit, AdmissionHint::AllowAllocation);
        // Untouched fields still take their defaults (spec §2.1: 0).
        assert_eq!(k.grid, GridShape::Elementwise);
        assert_eq!(k.block_x, 0);
    }

    #[test]
    fn parse_gpu_exclude() {
        // `@GpuExclude(reason = "test")`.
        let mut cp = CpBuilder::new();
        let desc = cp.utf8(GPU_EXCLUDE_DESC);
        let reason_name = cp.utf8("reason");
        let reason_val = cp.utf8("test");
        let cp = cp.build();
        let ann = Annotation {
            type_index: desc,
            element_value_pairs: vec![ElementValuePair {
                element_name_index: reason_name,
                value: ElementValue::Const {
                    tag: b's',
                    const_value_index: reason_val,
                },
            }],
        };
        let m = read_method_annotations(&[ria(vec![ann])], &cp);
        let e = m.gpu_exclude.expect("@GpuExclude must be detected");
        assert_eq!(e.reason, "test");
        assert!(m.gpu_kernel.is_none());
    }

    #[test]
    fn parse_enable_gpu_async_warmup_3() {
        // `@EnableGpuAsync(warmup = 3)` on a class.
        let mut cp = CpBuilder::new();
        let desc = cp.utf8(ENABLE_GPU_ASYNC_DESC);
        let warmup_name = cp.utf8("warmup");
        let warmup_val = cp.integer(3);
        let cp = cp.build();
        let ann = Annotation {
            type_index: desc,
            element_value_pairs: vec![ElementValuePair {
                element_name_index: warmup_name,
                value: ElementValue::Const {
                    tag: b'I',
                    const_value_index: warmup_val,
                },
            }],
        };
        let c = read_class_annotations(&[ria(vec![ann])], &cp);
        let a = c.enable_async.expect("@EnableGpuAsync must be detected");
        assert_eq!(a.warmup, 3);
    }

    #[test]
    fn unknown_annotation_ignored() {
        // `@Lcom/example/Other;` is not one of our three — the reader
        // returns the all-default `MethodAnnotations` (no `gpu_kernel`,
        // no `gpu_exclude`).
        let mut cp = CpBuilder::new();
        let desc = cp.utf8("Lcom/example/Other;");
        let cp = cp.build();
        let ann = Annotation {
            type_index: desc,
            element_value_pairs: vec![],
        };
        let m = read_method_annotations(&[ria(vec![ann.clone()])], &cp);
        assert_eq!(m, MethodAnnotations::default());
        assert!(m.gpu_kernel.is_none());
        assert!(m.gpu_exclude.is_none());

        let c = read_class_annotations(&[ria(vec![ann])], &cp);
        assert_eq!(c, ClassAnnotations::default());
        assert!(c.enable_async.is_none());
    }
}
