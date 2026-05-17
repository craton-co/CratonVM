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
//! [`rustjvm_reader::attribute::Attribute::RuntimeInvisibleAnnotations`]
//! / `RuntimeVisibleAnnotations` slices — the class reader has already
//! split the JVMS §4.7.16 byte stream into structured `Annotation` +
//! `ElementValue` values, so this layer does no raw-byte parsing.
//!
//! All getters are total — a missing element pair simply means "use the
//! default value" (per §2.1). Unknown annotation descriptors are
//! silently ignored.

use rustjvm_reader::attribute::{Annotation, Attribute, ElementValue};
use rustjvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

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
    /// Reserved — a tiled grid (per-block reductions, stencils).
    Tiled,
    /// Reserved — a single launch with a custom grid configuration.
    Custom,
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
    /// Allow new-array opcodes inside the kernel (PHASE1: still rejected
    /// by the emitter; recorded for the next phase).
    AllowAllocation,
    /// Allow calls into other classes (PHASE1: still rejected).
    AllowInvoke,
    /// Allow synchronized blocks (PHASE1: still rejected).
    AllowSynchronized,
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
        // Spec §2.1 defaults.
        Self {
            grid: GridShape::Elementwise,
            block_x: 256,
            block_y: 1,
            block_z: 1,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnableAsyncAttrs {
    pub warmup: u32,
}

impl Default for EnableAsyncAttrs {
    fn default() -> Self {
        Self { warmup: 0 }
    }
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

/// Read the GPU-relevant annotations off a class.
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

fn parse_grid_shape(value: &ElementValue, cp: &ConstantPool) -> GridShape {
    match as_enum_const_name(value, cp) {
        Some("ELEMENTWISE") => GridShape::Elementwise,
        Some("TILED") => GridShape::Tiled,
        Some("CUSTOM") => GridShape::Custom,
        _ => GridShape::default(),
    }
}

fn parse_admission_hint(value: &ElementValue, cp: &ConstantPool) -> AdmissionHint {
    match as_enum_const_name(value, cp) {
        Some("STRICT") => AdmissionHint::Strict,
        Some("ALLOW_ALLOCATION") => AdmissionHint::AllowAllocation,
        Some("ALLOW_INVOKE") => AdmissionHint::AllowInvoke,
        Some("ALLOW_SYNCHRONIZED") => AdmissionHint::AllowSynchronized,
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
    use rustjvm_reader::attribute::ElementValuePair;

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
        assert_eq!(k.block_x, 256);
        assert_eq!(k.block_y, 1);
        assert_eq!(k.block_z, 1);
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
        // Untouched fields still take their defaults.
        assert_eq!(k.grid, GridShape::Elementwise);
        assert_eq!(k.block_x, 256);
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
