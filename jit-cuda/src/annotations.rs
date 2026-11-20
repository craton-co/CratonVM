// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

/// The admission loosenings a `@GpuKernel` asked for, as an
/// independent set rather than a single choice.
///
/// # Why this exists beside [`AdmissionHint`]
///
/// AUDIT 2026-09-02. `AdmissionHint` is one-of, and the four things it
/// names are not mutually exclusive: wanting `Math.sqrt` says nothing
/// about wanting an unguarded division. A user could ask for either and
/// never both.
///
/// The cost of that showed up inside this crate rather than in a bug
/// report. `frem`/`drem` are admitted under `AllowDivByZero`, and the
/// reason given in `analyzer::Reason::FloatRemainder` is not a semantic
/// one — it is that minting a dedicated variant "would require editing
/// `annotations.rs`". So one flag came to gate two unrelated lowering
/// decisions (skip the integer zero-divisor guard; accept a
/// precision-bounded `frem` identity), and keeping them untangled takes
/// three paragraphs of doc comment on three different types.
///
/// Here they are separate bits. `AllowDivByZero` still sets both, so
/// nothing a user has written changes meaning; an annotation that wants
/// only one can now say so.
///
/// # The array form
///
/// Java annotation elements may be arrays, so the composable spelling is
/// `@GpuKernel(admit = {ALLOW_INTRINSIC_CALLS, ALLOW_DIV_BY_ZERO})`.
/// [`parse_admission_flags`] accepts it and the single-constant form
/// alike. Whether `craton.gpu.GpuKernel` declares `admit()` as
/// `AdmissionHint` or `AdmissionHint[]` is that project's decision —
/// this side is ready for either, which is the half that can be settled
/// from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AdmissionFlags {
    /// Set by `ALLOW_ALLOCATION`. **Parsed and stored, read by
    /// nothing** — AUDIT 2026-09-21.
    ///
    /// It used to make `analyzer::classify` admit `newarray`
    /// of a primitive component whose size came from a method
    /// parameter. `lowering/emit.rs` has no arm for `newarray` (nor
    /// for the `areturn` of the array it produces), so every method
    /// admitted on this bit was refused one layer later and
    /// blacklisted. Allocation now rejects under every hint; see
    /// [`crate::analyzer::Reason::Allocation`] for what lowering it
    /// would take, which is a VM-marshaller change, not an emitter
    /// one.
    ///
    /// The bit stays so the annotation keeps parsing (sources that
    /// already carry `@GpuKernel(admit = ALLOW_ALLOCATION)` still
    /// compile and still analyze, they just analyze to a reject) and
    /// so a future round that wires the array-return path has a flag
    /// to read. `analyzer::tests::allow_allocation_hint_is_still_parsed_into_flags`
    /// pins it set; `allow_allocation_hint_does_not_admit_newarray`
    /// pins it inert.
    pub allocation: bool,
    /// Lowering skips the explicit divisor-zero deopt guard on
    /// `idiv`/`ldiv`/`irem`/`lrem`. Other guards, such as
    /// signed-minimum divided by `-1`, remain in force.
    pub unguarded_integer_division: bool,
    /// `frem`/`drem` are admitted at all.
    ///
    /// Separate from [`AdmissionFlags::unguarded_integer_division`]
    /// because it accepts a different thing: the div+truncate+fma
    /// identity is bit-exact only while the quotient magnitude stays
    /// within the type's exactly-representable-integer range (`< 2^24`
    /// for `float`, `< 2^53` for `double`). Outside it the answer is
    /// wrong by a whole multiple of the divisor — not a rounding error —
    /// and the GPU deopt machinery catches bounds and div-zero traps,
    /// never wrong VALUES.
    pub approximate_float_remainder: bool,
    /// `invokestatic` to a curated [`crate::analyzer::MathIntrinsic`] is
    /// accepted. Every other `invokestatic` still rejects.
    pub intrinsic_calls: bool,
}

impl AdmissionFlags {
    /// The strict default: nothing loosened.
    pub const STRICT: Self = Self {
        allocation: false,
        unguarded_integer_division: false,
        approximate_float_remainder: false,
        intrinsic_calls: false,
    };

    /// Fold one Java constant name into the set.
    ///
    /// Unknown names are ignored rather than defaulted, which is the
    /// safe direction here: every bit this can set LOOSENS admission, so
    /// a name this crate does not know leaves the analyzer stricter than
    /// the user asked for. It can lose coverage and never admit
    /// something the emitter cannot lower.
    ///
    /// (`parse_grid_shape` cannot do the same — an unknown grid shape
    /// has to REJECT, because defaulting there would silently lower a
    /// block reduction element-wise. See [`GridShape::Unknown`].)
    fn apply_name(&mut self, name: &str) {
        match name {
            "STRICT" => {}
            "ALLOW_ALLOCATION" => self.allocation = true,
            "ALLOW_DIV_BY_ZERO" => {
                // Both, deliberately: this is the constant users have
                // been writing, and it has always meant both since
                // `frem`/`drem` were admitted under it.
                self.unguarded_integer_division = true;
                self.approximate_float_remainder = true;
            }
            "ALLOW_INTRINSIC_CALLS" => self.intrinsic_calls = true,
            _ => {}
        }
    }
}

impl From<AdmissionHint> for AdmissionFlags {
    fn from(h: AdmissionHint) -> Self {
        let mut f = Self::STRICT;
        match h {
            AdmissionHint::Strict => {}
            AdmissionHint::AllowAllocation => f.allocation = true,
            AdmissionHint::AllowDivByZero => {
                f.unguarded_integer_division = true;
                f.approximate_float_remainder = true;
            }
            AdmissionHint::AllowIntrinsicCalls => f.intrinsic_calls = true,
        }
        f
    }
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
    /// Everything the annotation asked for, as an independent set — see
    /// [`AdmissionFlags`] for why one-of was the wrong shape.
    ///
    /// This replaced an `AdmissionHint` of the same name rather than
    /// sitting beside one. A struct with both would need every
    /// constructor to keep them in sync, and the first run after adding
    /// the second field proved the point: twenty tests built
    /// `GpuKernelAttrs { admit, ..Default::default() }`, left the set at
    /// `STRICT`, and silently lost their hint. Changing the type breaks
    /// those callers loudly instead. `AdmissionHint::into()` converts.
    pub admit: AdmissionFlags,
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
            admit: AdmissionFlags::STRICT,
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
pub(crate) const GRID_SHAPE_NAMES: [&str; 3] = ["ELEMENTWISE", "ROW_PER_THREAD", "BLOCK_REDUCTION"];

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

/// Parse `admit` into an independent set, accepting either the
/// single-constant spelling (`admit = ALLOW_DIV_BY_ZERO`) or the array
/// one (`admit = {ALLOW_INTRINSIC_CALLS, ALLOW_DIV_BY_ZERO}`).
///
/// A nested array is not recursed into: the JVMS permits arbitrary
/// nesting in the encoding, javac never produces it for an
/// enum-array-valued element, and flattening one would be reading intent
/// into a shape no source form makes.
pub(crate) fn parse_admission_flags(value: &ElementValue, cp: &ConstantPool) -> AdmissionFlags {
    let mut out = AdmissionFlags::STRICT;
    match value {
        ElementValue::Array(items) => {
            for item in items {
                if let Some(name) = as_enum_const_name(item, cp) {
                    out.apply_name(name);
                }
            }
        }
        other => {
            if let Some(name) = as_enum_const_name(other, cp) {
                out.apply_name(name);
            }
        }
    }
    out
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
        out.admit = parse_admission_flags(v, cp);
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
mod tests;
