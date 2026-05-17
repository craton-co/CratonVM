//! A single execution frame (stack frame) in the JVM.
//!
//! Each method invocation creates a new `Frame` containing:
//! - Local variables (`Vec<CompactValue>`, 8 bytes/slot, NaN-boxed tag inline)
//! - Operand stack (also NaN-boxed `Vec<CompactValue>` via `ValueStack`)
//! - Program counter
//! - Method bytecode and exception table

use std::collections::HashMap;
use std::sync::Arc;

use rustjvm_reader::attribute::ExceptionTableEntry;

use crate::classloading::resolution::CachedBytecodeMethod;
use crate::classloading::ClassId;
use crate::runtime::ValueStack;
use crate::types::{
    CompactTag, CompactValue, ObjectRef, Value, VTAG_DOUBLE, VTAG_FLOAT, VTAG_INT, VTAG_LONG,
    VTAG_NULL, VTAG_OBJECT, VTAG_RETADDR, VTAG_UNINIT,
};

/// Create a bytecode Arc with 2 trailing zero bytes for safe speculative reads.
/// This allows the hot loop to unconditionally read `code[pc+1]` and `code[pc+2]`
/// without bounds checks, since the padding guarantees valid memory.
pub fn padded_bytecode(code: &[u8]) -> Arc<[u8]> {
    let mut padded = Vec::with_capacity(code.len() + 2);
    padded.extend_from_slice(code);
    padded.push(0);
    padded.push(0);
    Arc::from(padded.into_boxed_slice())
}

/// JVM slots required to hold `args` as the initial local variable array
/// (category-2 types occupy two consecutive slots).
#[inline]
fn invoke_arg_slot_count(args: &[Value]) -> usize {
    let mut n = 0usize;
    for a in args {
        n += if a.is_category2() { 2 } else { 1 };
    }
    n
}

/// `Code.max_locals` must be at least this many slots for the parameters the
/// verifier expects, but some classfiles in the wild are wrong. If declared
/// `max_locals` is too small, `copy_args_to_locals` would silently drop tail
/// arguments and callees would see `null`/uninitialized locals (e.g. Surefire
/// `JUnitPlatformProvider.<init>(ProviderParameters,Launcher)` with `launcher`
/// never stored).
#[inline]
fn effective_max_locals(declared: u16, args: &[Value]) -> u16 {
    let needed = invoke_arg_slot_count(args);
    let needed_u16 = u16::try_from(needed).unwrap_or(u16::MAX);
    declared.max(needed_u16)
}

/// Cold-path frame metadata: either owned Arcs or a shared CachedBytecodeMethod.
/// For cached calls, storing a single Arc<CachedBytecodeMethod> avoids cloning
/// 5 separate Arc fields per call (class_name, method_name, descriptor, source_file,
/// exception_table), saving ~10 atomic ops per call cycle (clone + drop).
enum FrameInner {
    /// Non-cached frame: owns all metadata Arcs individually.
    Owned {
        class_name: Arc<str>,
        method_name: Arc<str>,
        method_descriptor: Arc<str>,
        source_file: Option<Arc<str>>,
        exception_table: Arc<[ExceptionTableEntry]>,
    },
    /// Cached frame: all cold metadata derived from a single Arc.
    Cached(Arc<CachedBytecodeMethod>),
}

impl std::fmt::Debug for FrameInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameInner::Owned {
                class_name,
                method_name,
                ..
            } => {
                write!(f, "Owned({}.{})", class_name, method_name)
            }
            FrameInner::Cached(cm) => {
                write!(f, "Cached({}.{})", cm.class_name, cm.method_name)
            }
        }
    }
}

/// A single execution frame for a method invocation.
///
/// Locals are stored as `Vec<CompactValue>` (8 bytes/slot, NaN-boxed tag
/// embedded in the high bits of the u64). This collapses what used to be a
/// parallel `Vec<u64> + Vec<u8>` SoA pair into a single cache-line-friendly
/// buffer: every `iload_N` / `istore_N` / `iinc` opcode now touches one
/// allocation instead of two (HIGH-8 audit fix, 2026-05-16).
#[derive(Debug)]
pub struct Frame {
    /// The class that declares this method.
    pub class_id: ClassId,

    /// Current program counter (byte offset into `code`).
    pub pc: usize,

    /// PC of the instruction that started the current/last execution cycle.
    /// Used for exception table lookup when unwinding through parent frames,
    /// since `pc` may have already been advanced past the invoke instruction.
    pub last_instr_pc: usize,

    /// Local variable storage (NaN-boxed CompactValues, one 8-byte slot each).
    locals: Vec<CompactValue>,

    /// Operand stack (NaN-boxed CompactValues internally).
    pub stack: ValueStack,

    /// The raw bytecode of the method (hot path — kept as direct Arc).
    pub code: Arc<[u8]>,

    /// Maximum operand stack depth (from Code attribute).
    pub max_stack: u16,

    /// Local variable slot count for this frame: at least the `Code.max_locals`
    /// value from the class file and at least the slots required by the actual
    /// invocation arguments (defensive clamp; see `effective_max_locals`).
    pub max_locals: u16,

    /// Cold-path metadata (method name, descriptor, exception table, etc.).
    inner: FrameInner,

    /// Backward branch counter for OSR (On-Stack Replacement).
    /// Incremented on each backward branch; triggers JIT when exceeding threshold.
    pub backward_count: u32,

    /// For synchronized methods dispatched via the stackless path: the monitor
    /// object that must be released when this frame returns or is unwound by
    /// an exception.  `None` for non-synchronized methods.
    pub monitor_on_exit: Option<ObjectRef>,

    /// T14: Set to true for methods from real JDK classes (java/*, jdk/*, sun/*).
    /// Used to skip the fast-path interpreter which uses pop_unchecked and may
    /// panic on bytecode patterns not handled by the fast path.
    pub is_jdk_class: bool,
}

fn init_locals(max_locals: u16, args: &[Value]) -> (Vec<CompactValue>, u16) {
    let eff = effective_max_locals(max_locals, args);
    let n = eff as usize;
    let mut locals = vec![CompactValue::uninitialized(); n];
    copy_args_to_locals(&mut locals, args);
    (locals, eff)
}

/// Pool-backed local-Vec initialisation.
///
/// The pool stores `(Vec<u64>, Vec<u8>)` tuples — the `u64` half is the
/// recycled locals buffer (transmuted to/from `Vec<CompactValue>` via
/// `#[repr(transparent)]`), and the `u8` half is now unused (locals no longer
/// have a parallel tag Vec; tags are encoded inline in each CompactValue).
/// The `u8` Vec is preserved in the pool tuple shape so cross-crate callers
/// (`JvmThread::recycle_frame_with_shared`, `VecPool<u8>` spill paths) keep
/// working without churn.
fn init_locals_pooled(
    max_locals: u16,
    args: &[Value],
    pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
) -> (Vec<CompactValue>, u16) {
    let eff = effective_max_locals(max_locals, args);
    let n = eff as usize;
    let (vals, _tags) = pool.pop().unwrap_or_default();
    // SAFETY: CompactValue is repr(transparent) over u64 — transmute is a
    // no-op layout-wise. The discarded tag Vec is unused for locals now.
    let mut locals = u64_vec_to_compact(vals);
    locals.clear();
    locals.resize(n, CompactValue::uninitialized());
    copy_args_to_locals(&mut locals, args);
    (locals, eff)
}

fn copy_args_to_locals(locals: &mut [CompactValue], args: &[Value]) {
    let mut slot = 0;
    for arg in args {
        if slot < locals.len() {
            locals[slot] = CompactValue::from_value(*arg);
            slot += 1;
            // Category 2 values (long, double) occupy two slots; the upper
            // half is left uninitialised by JVM convention.
            if arg.is_category2() && slot < locals.len() {
                locals[slot] = CompactValue::uninitialized();
                slot += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Vec<u64> <-> Vec<CompactValue> transmute helpers (safe under
// CompactValue's repr(transparent) guarantee — identical size and alignment).
// Used at the boundary between the pooled `Vec<u64>` slot buffers and the
// frame's internal `Vec<CompactValue>` locals storage.
// ---------------------------------------------------------------------------

#[inline(always)]
fn u64_vec_to_compact(v: Vec<u64>) -> Vec<CompactValue> {
    // SAFETY: CompactValue is #[repr(transparent)] over u64 — identical size,
    // alignment, and validity invariants (any u64 is a valid CompactValue
    // bit pattern). Vec's length/capacity/allocator are preserved.
    let mut v = std::mem::ManuallyDrop::new(v);
    let len = v.len();
    let cap = v.capacity();
    let ptr = v.as_mut_ptr() as *mut CompactValue;
    unsafe { Vec::from_raw_parts(ptr, len, cap) }
}

#[inline(always)]
fn compact_vec_to_u64(v: Vec<CompactValue>) -> Vec<u64> {
    // SAFETY: see u64_vec_to_compact.
    let mut v = std::mem::ManuallyDrop::new(v);
    let len = v.len();
    let cap = v.capacity();
    let ptr = v.as_mut_ptr() as *mut u64;
    unsafe { Vec::from_raw_parts(ptr, len, cap) }
}

/// Convert a local-slot (u64 + tag) directly to a CompactValue,
/// skipping the Value-enum round-trip (T10.9.D hot-path helper).
///
/// Mirrors the policy of `decode_value`: malformed or zero-init Object
/// slots degrade to null rather than panicking.
#[inline(always)]
fn local_slot_to_compact(val: u64, tag: u8) -> CompactValue {
    match tag {
        VTAG_INT => CompactValue::int(val as i32),
        VTAG_LONG => CompactValue::long(val as i64),
        VTAG_FLOAT => CompactValue::float(f32::from_bits(val as u32)),
        VTAG_DOUBLE => CompactValue::from_bits(val),
        VTAG_OBJECT => {
            // Guard against zero-initialised / stale / unaligned object slots
            // the same way decode_value does.
            if val == 0 || (val as usize) % 8 != 0 {
                CompactValue::null()
            } else {
                CompactValue::object(val)
            }
        }
        VTAG_NULL => CompactValue::null(),
        VTAG_RETADDR => CompactValue::return_address(val as u32),
        _ => CompactValue::uninitialized(),
    }
}

/// Convert a CompactValue back into a legacy (u64 + tag) slot pair.
///
/// Used at boundaries where the legacy SoA representation is still required
/// (continuation freeze/thaw, `locals_snapshot`, `to_frozen_frame`). The
/// hot interpreter path no longer needs this helper.
#[inline(always)]
fn compact_to_local_slot(cv: CompactValue) -> (u64, u8) {
    match cv.tag() {
        CompactTag::Int => (cv.as_int().unwrap_or(0) as u32 as u64, VTAG_INT),
        CompactTag::Long => (cv.as_long_unchecked() as u64, VTAG_LONG),
        CompactTag::Float => (cv.as_float().unwrap_or(0.0).to_bits() as u64, VTAG_FLOAT),
        CompactTag::Double => {
            // Untagged — raw bits. Could be a Double or a Long that was
            // created via CompactValue::long (both untagged).  Prefer the
            // Double tag; stores from Lstore use set_local with Value::Long
            // which goes through encode_value-equivalent paths.
            (cv.raw_bits(), VTAG_DOUBLE)
        }
        CompactTag::Object => {
            let ptr = cv.as_object_ptr().unwrap_or(0);
            (ptr, VTAG_OBJECT)
        }
        CompactTag::Null => (0, VTAG_NULL),
        CompactTag::Uninitialized => (0, VTAG_UNINIT),
        CompactTag::ReturnAddress => {
            (cv.as_return_address().unwrap_or(0) as u64, VTAG_RETADDR)
        }
    }
}

/// T14 — The interpreter's raw-bytecode super-instruction loop is tuned for
/// synthetic classfiles and uses `pop_unchecked` / `set_local_unchecked` at
/// sites that assume verifier-narrow stack shapes. Real JDK packages and
/// Spring Framework (`org.springframework.*`) routinely mix reference returns
/// from `invokevirtual` with `astore`/`checkcast` sequences where the fast
/// path can diverge from the spec-correct slow path (Letsgo no-JIT AV
/// immediately after `ConfigurationClassEnhancer.enhance` returns at
/// `ConfigurationClassPostProcessor.enhanceConfigurationClasses` + `astore`).
///
/// When this returns `true`, `execute_frame` skips the fast path and uses
/// full `Instruction::decode` dispatch (`is_jdk_class` on [`Frame`]).
#[inline]
pub(crate) fn class_disables_interp_fast_path(class_name: &str) -> bool {
    class_name.starts_with("java/")
        || class_name.starts_with("jdk/")
        || class_name.starts_with("sun/")
        || class_name.starts_with("com/sun/")
        || class_name.contains("springframework")
}

impl Frame {
    /// Create a new frame for a method (converts owned String/Vec to Arc).
    ///
    /// `args` are copied into the first local variable slots.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        class_id: ClassId,
        class_name: String,
        method_name: String,
        method_descriptor: String,
        source_file: Option<String>,
        code: Vec<u8>,
        exception_table: Vec<ExceptionTableEntry>,
        max_stack: u16,
        max_locals: u16,
        args: &[Value],
    ) -> Self {
        let (locals, eff_max_locals) = init_locals(max_locals, args);
        let is_jdk = class_disables_interp_fast_path(&*class_name);
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            locals,
            stack: ValueStack::new((max_stack as usize).max(16) + 8),
            code: padded_bytecode(&code),
            max_stack,
            max_locals: eff_max_locals,
            inner: FrameInner::Owned {
                class_name: Arc::from(class_name.as_str()),
                method_name: Arc::from(method_name.as_str()),
                method_descriptor: Arc::from(method_descriptor.as_str()),
                source_file: source_file.map(|s| Arc::from(s.as_str())),
                exception_table: Arc::from(exception_table.into_boxed_slice()),
            },
            backward_count: 0,
            monitor_on_exit: None,
            is_jdk_class: is_jdk,
        }
    }

    /// Create a new frame from pre-built Arc data (zero-copy for code and strings).
    #[allow(clippy::too_many_arguments)]
    pub fn new_from_arcs(
        class_id: ClassId,
        class_name: Arc<str>,
        method_name: Arc<str>,
        method_descriptor: Arc<str>,
        source_file: Option<Arc<str>>,
        code: Arc<[u8]>,
        exception_table: Arc<[ExceptionTableEntry]>,
        max_stack: u16,
        max_locals: u16,
        args: &[Value],
    ) -> Self {
        let (locals, eff_max_locals) = init_locals(max_locals, args);
        let is_jdk = class_disables_interp_fast_path(&*class_name);
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            locals,
            stack: ValueStack::new((max_stack as usize).max(16) + 8),
            code,
            max_stack,
            max_locals: eff_max_locals,
            inner: FrameInner::Owned {
                class_name,
                method_name,
                method_descriptor,
                source_file,
                exception_table,
            },
            backward_count: 0,
            monitor_on_exit: None,
            is_jdk_class: is_jdk,
        }
    }

    /// Create a new frame from Arc data, reusing pooled Vecs for locals and stack.
    #[allow(clippy::too_many_arguments)]
    pub fn new_pooled(
        class_id: ClassId,
        class_name: Arc<str>,
        method_name: Arc<str>,
        method_descriptor: Arc<str>,
        source_file: Option<Arc<str>>,
        code: Arc<[u8]>,
        exception_table: Arc<[ExceptionTableEntry]>,
        max_stack: u16,
        max_locals: u16,
        args: &[Value],
        locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
        stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    ) -> Self {
        let (locals, eff_max_locals) =
            init_locals_pooled(max_locals, args, locals_pool);
        let padded_max = (max_stack as usize).max(16) + 8;
        let stack = if let Some((vals, tags)) = stacks_pool.pop() {
            ValueStack::from_pooled(vals, tags, padded_max)
        } else {
            ValueStack::new(padded_max)
        };
        let is_jdk = class_disables_interp_fast_path(&*class_name);
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            locals,
            stack,
            code,
            max_stack,
            max_locals: eff_max_locals,
            inner: FrameInner::Owned {
                class_name,
                method_name,
                method_descriptor,
                source_file,
                exception_table,
            },
            backward_count: 0,
            monitor_on_exit: None,
            is_jdk_class: is_jdk,
        }
    }

    /// Create a new frame from a cached method, reusing pooled Vecs.
    /// Only clones the `code` Arc (hot path) — all cold metadata is accessed
    /// through the single `Arc<CachedBytecodeMethod>`, saving ~10 atomic ops per call.
    pub fn new_pooled_cached(
        cached: Arc<CachedBytecodeMethod>,
        args: &[Value],
        locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
        stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    ) -> Self {
        let (locals, eff_max_locals) =
            init_locals_pooled(cached.max_locals, args, locals_pool);
        let padded_max = (cached.max_stack as usize).max(16) + 8;
        let stack = if let Some((vals, tags)) = stacks_pool.pop() {
            ValueStack::from_pooled(vals, tags, padded_max)
        } else {
            ValueStack::new(padded_max)
        };
        let class_id = cached.declaring_class_id;
        let code = cached.code.clone();
        let max_stack = cached.max_stack;
        let is_jdk = class_disables_interp_fast_path(cached.class_name.as_ref());
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            locals,
            stack,
            code,
            max_stack,
            max_locals: eff_max_locals,
            inner: FrameInner::Cached(cached),
            backward_count: 0,
            monitor_on_exit: None,
            is_jdk_class: is_jdk,
        }
    }

    /// Reset this frame in-place for tail-call elimination.
    /// Reuses the existing Vec allocations (locals, stack) to avoid allocation.
    pub fn reset_for_tail_call(
        &mut self,
        class_id: ClassId,
        code: Arc<[u8]>,
        max_stack: u16,
        max_locals: u16,
        args: &[Value],
        class_name: Arc<str>,
        method_name: Arc<str>,
        descriptor: Arc<str>,
        source_file: Option<Arc<str>>,
        exception_table: Arc<[ExceptionTableEntry]>,
    ) {
        self.class_id = class_id;
        self.pc = 0;
        self.last_instr_pc = 0;
        self.backward_count = 0;
        self.code = code;
        self.max_stack = max_stack;
        let eff_max_locals = effective_max_locals(max_locals, args);
        self.max_locals = eff_max_locals;
        // Update is_jdk_class for correct fast/slow path dispatch
        self.is_jdk_class = class_disables_interp_fast_path(class_name.as_ref());
        // Update inner metadata so class_name(), method_name(), exception_table() are correct
        self.inner = FrameInner::Owned {
            class_name,
            method_name,
            method_descriptor: descriptor,
            source_file,
            exception_table,
        };
        // Reset locals
        let n = eff_max_locals as usize;
        self.locals.clear();
        self.locals.resize(n, CompactValue::uninitialized());
        copy_args_to_locals(&mut self.locals, args);
        // Reset operand stack
        self.stack.clear();
    }

    /// Return this frame's Vec allocations to the pool for reuse.
    ///
    /// The locals tuple's `Vec<u8>` half is now always empty — locals are a
    /// flat `Vec<CompactValue>` (transmuted to/from `Vec<u64>` at the pool
    /// boundary via `#[repr(transparent)]`). The tag-Vec slot is retained in
    /// the tuple shape for backwards compatibility with `JvmThread`'s
    /// per-vector pool routing.
    pub fn recycle(
        self,
        locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
        stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    ) {
        locals_pool.push((compact_vec_to_u64(self.locals), Vec::new()));
        stacks_pool.push(self.stack.into_inner());
    }

    /// T10.7 — consume the frame and return its four pooled `Vec`s as
    /// `(local_vals, local_tags, stack_vals, stack_tags)`.
    ///
    /// Used by `JvmThread::recycle_frame_with_shared` to decide per-vector
    /// whether to keep the allocation in the thread-local pool or spill it
    /// into the VM-wide `VecPool` on `SharedVm`.
    ///
    /// `local_tags` is always empty after the HIGH-8 audit migration to
    /// CompactValue-only locals; the slot remains for ABI compatibility with
    /// callers that route the per-Vec spill independently.
    pub fn take_pool_parts(self) -> (Vec<u64>, Vec<u8>, Vec<u64>, Vec<u8>) {
        let (stack_vals, stack_tags) = self.stack.into_inner();
        (compact_vec_to_u64(self.locals), Vec::new(), stack_vals, stack_tags)
    }

    // ── Cold-path accessors (method metadata, exception table) ──────────

    /// Access the class name (cold path — error messages, stack traces).
    #[inline]
    pub fn class_name(&self) -> &str {
        match &self.inner {
            FrameInner::Owned { class_name, .. } => class_name,
            FrameInner::Cached(cm) => &cm.class_name,
        }
    }

    /// Access the method name (cold path — error messages, stack traces).
    #[inline]
    pub fn method_name(&self) -> &str {
        match &self.inner {
            FrameInner::Owned { method_name, .. } => method_name,
            FrameInner::Cached(cm) => &cm.method_name,
        }
    }

    /// Access the method descriptor (cold path — error messages).
    #[inline]
    pub fn method_descriptor(&self) -> &str {
        match &self.inner {
            FrameInner::Owned {
                method_descriptor, ..
            } => method_descriptor,
            FrameInner::Cached(cm) => &cm.method_descriptor,
        }
    }

    /// Access the source file (cold path — stack traces).
    #[inline]
    pub fn source_file(&self) -> Option<&str> {
        match &self.inner {
            FrameInner::Owned { source_file, .. } => source_file.as_deref(),
            FrameInner::Cached(cm) => cm.source_file.as_deref(),
        }
    }

    /// Access the exception table (cold path — catch block lookup).
    #[inline]
    pub fn exception_table(&self) -> &[ExceptionTableEntry] {
        match &self.inner {
            FrameInner::Owned {
                exception_table, ..
            } => exception_table,
            FrameInner::Cached(cm) => &cm.exception_table,
        }
    }

    /// Clone class_name as Arc<str> for stack trace capture (cold path).
    pub fn class_name_arc(&self) -> Arc<str> {
        match &self.inner {
            FrameInner::Owned { class_name, .. } => class_name.clone(),
            FrameInner::Cached(cm) => cm.class_name.clone(),
        }
    }

    /// Clone method_name as Arc<str> for stack trace capture (cold path).
    pub fn method_name_arc(&self) -> Arc<str> {
        match &self.inner {
            FrameInner::Owned { method_name, .. } => method_name.clone(),
            FrameInner::Cached(cm) => cm.method_name.clone(),
        }
    }

    /// Clone method_descriptor as Arc<str> (cold path — PGO profiling, error messages).
    pub fn method_descriptor_arc(&self) -> Arc<str> {
        match &self.inner {
            FrameInner::Owned {
                method_descriptor, ..
            } => method_descriptor.clone(),
            FrameInner::Cached(cm) => cm.method_descriptor.clone(),
        }
    }

    /// Clone source_file as Option<Arc<str>> for stack trace capture (cold path).
    pub fn source_file_arc(&self) -> Option<Arc<str>> {
        match &self.inner {
            FrameInner::Owned { source_file, .. } => source_file.clone(),
            FrameInner::Cached(cm) => cm.source_file.clone(),
        }
    }

    // ── Local variable access ───────────────────────────────────────────

    /// Number of local variable slots.
    #[inline(always)]
    pub fn locals_len(&self) -> usize {
        self.locals.len()
    }

    /// Get a local variable by index.
    pub fn get_local(&self, index: u16) -> Value {
        let i = index as usize;
        if i >= self.locals.len() {
            return Value::Uninitialized;
        }
        self.locals[i].to_value()
    }

    /// Get a local variable by index without error wrapping.
    /// Used by the fast-path interpreter for verified bytecode.
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn get_local_unchecked(&self, index: usize) -> Value {
        self.locals[index].to_value()
    }

    /// Set a local variable by index.
    pub fn set_local(&mut self, index: u16, value: Value) {
        let i = index as usize;
        if i < self.locals.len() {
            self.locals[i] = CompactValue::from_value(value);
        }
    }

    /// Set a local variable by index without error wrapping.
    /// Used by the fast-path interpreter for verified bytecode.
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn set_local_unchecked(&mut self, index: usize, value: Value) {
        self.locals[index] = CompactValue::from_value(value);
    }

    /// Set a local int slot directly (T10.9.D hot-path, mirrors
    /// `ValueStack::push_int_unchecked`). Avoids the `Value` enum round-trip
    /// for `istore_N` / `iinc` / int-typed `wide_istore`.
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn set_local_int_unchecked(&mut self, index: usize, v: i32) {
        self.locals[index] = CompactValue::int(v);
    }

    /// Get a local int slot directly (T10.9.D hot-path, mirrors
    /// `ValueStack::pop_int_unchecked`). Returns 0 if the slot does not
    /// currently hold an int (mirrors the prior `as_int().unwrap_or(0)`
    /// fall-back used at `iinc` sites).
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn get_local_int_unchecked(&self, index: usize) -> i32 {
        self.locals[index].as_int().unwrap_or(0)
    }

    /// Get the raw u64 value of a local (for JIT/OSR interop).
    ///
    /// Returns the legacy SoA-style raw bits (e.g. `i32 as u32 as u64` for
    /// ints, `f64::to_bits()` for doubles), reconstructed from the inline
    /// CompactValue. The JIT ABI expects this exact representation.
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn get_local_raw(&self, index: usize) -> u64 {
        let (v, _t) = compact_to_local_slot(self.locals[index]);
        v
    }

    /// Get the legacy VTAG byte of a local (for JIT/OSR interop).
    ///
    /// Derived from the inline CompactValue tag — the parallel `local_tags`
    /// Vec no longer exists.
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn get_local_tag(&self, index: usize) -> u8 {
        let (_v, t) = compact_to_local_slot(self.locals[index]);
        t
    }

    /// Get a local as a `CompactValue` without the `Value` enum round-trip
    /// (T10.9.D hot-path).  Bounds-safe: out-of-range returns
    /// `CompactValue::uninitialized()` to preserve prior `get_local` semantics.
    #[inline(always)]
    pub fn get_local_compact(&self, index: u16) -> CompactValue {
        let i = index as usize;
        if i >= self.locals.len() {
            return CompactValue::uninitialized();
        }
        self.locals[i]
    }

    /// Set a local from a `CompactValue` (T10.9.D hot-path).  Silently no-ops
    /// on out-of-range index to mirror `set_local`.
    #[inline(always)]
    pub fn set_local_compact(&mut self, index: u16, cv: CompactValue) {
        let i = index as usize;
        if i < self.locals.len() {
            self.locals[i] = cv;
        }
    }

    /// Set a local from a `CompactValue` without bounds check (fast path).
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn set_local_compact_unchecked(&mut self, index: usize, cv: CompactValue) {
        self.locals[index] = cv;
    }

    /// Get a local as a `CompactValue` without bounds check (fast path).
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn get_local_compact_unchecked(&self, index: usize) -> CompactValue {
        self.locals[index]
    }

    // ── Continuation freeze/thaw ─────────────────��───────────────────

    /// Clone the exception table Arc (cold path — for continuation freeze).
    pub fn exception_table_arc(&self) -> Arc<[ExceptionTableEntry]> {
        match &self.inner {
            FrameInner::Owned { exception_table, .. } => exception_table.clone(),
            FrameInner::Cached(cm) => cm.exception_table.clone(),
        }
    }

    /// Snapshot local variable raw values (for continuation freeze).
    ///
    /// Locals are stored inline as `CompactValue`, but the public snapshot
    /// shape is preserved as `(Vec<u64>, Vec<u8>)` for compatibility with
    /// `FrozenFrame` and external callers. Each slot is decomposed back into
    /// the legacy (bits, vtag) pair via `compact_to_local_slot`.
    pub fn locals_snapshot(&self) -> (Vec<u64>, Vec<u8>) {
        let n = self.locals.len();
        let mut vals = Vec::with_capacity(n);
        let mut tags = Vec::with_capacity(n);
        for cv in &self.locals {
            let (v, t) = compact_to_local_slot(*cv);
            vals.push(v);
            tags.push(t);
        }
        (vals, tags)
    }

    /// Freeze this frame into a `FrozenFrame` that can be stored in a continuation.
    /// Captures all state needed to reconstruct the frame later.
    pub fn to_frozen_frame(&self) -> crate::threading::virtual_threads::FrozenFrame {
        let (stack_vals, stack_tags) = self.stack.snapshot_raw();
        let (locals_vals, locals_tags) = self.locals_snapshot();
        crate::threading::virtual_threads::FrozenFrame {
            class_name: self.class_name().to_string(),
            method_name: self.method_name().to_string(),
            descriptor: self.method_descriptor().to_string(),
            bytecode_pc: self.pc,
            locals: locals_vals,
            local_tags: locals_tags,
            stack: stack_vals,
            stack_tags,
            // Restoration metadata
            code: Some(self.code.clone()),
            class_id: Some(self.class_id),
            max_stack: Some(self.max_stack),
            max_locals: Some(self.max_locals),
            exception_table: Some(self.exception_table_arc()),
            source_file: self.source_file().map(|s| s.to_string()),
        }
    }

    /// Restore a Frame from a `FrozenFrame` (continuation thaw).
    /// The FrozenFrame must have been produced by `to_frozen_frame()` (i.e. have
    /// restoration metadata).
    ///
    /// # Panics
    /// Panics if the FrozenFrame lacks restoration metadata (code, class_id, etc.).
    pub fn from_frozen_frame(frozen: crate::threading::virtual_threads::FrozenFrame) -> Self {
        let code = frozen.code.expect("FrozenFrame missing code for thaw");
        let class_id = frozen.class_id.expect("FrozenFrame missing class_id for thaw");
        let max_stack = frozen.max_stack.expect("FrozenFrame missing max_stack for thaw");
        let max_locals = frozen.max_locals.expect("FrozenFrame missing max_locals for thaw");
        let exception_table = frozen.exception_table.expect("FrozenFrame missing exception_table for thaw");

        let stack = ValueStack::from_snapshot(frozen.stack, frozen.stack_tags, max_stack as usize);

        // Rebuild the inline-CompactValue locals from the legacy (bits, vtag)
        // pair carried in the frozen frame.
        let n = frozen.locals.len();
        debug_assert_eq!(
            n,
            frozen.local_tags.len(),
            "FrozenFrame locals/local_tags length mismatch on thaw"
        );
        let mut locals = Vec::with_capacity(n);
        for i in 0..n {
            let tag = frozen.local_tags.get(i).copied().unwrap_or(VTAG_UNINIT);
            let val = frozen.locals[i];
            locals.push(local_slot_to_compact(val, tag));
        }

        Self {
            class_id,
            pc: frozen.bytecode_pc,
            last_instr_pc: frozen.bytecode_pc,
            locals,
            stack,
            code,
            max_stack,
            max_locals,
            inner: FrameInner::Owned {
                class_name: Arc::from(frozen.class_name.as_str()),
                method_name: Arc::from(frozen.method_name.as_str()),
                method_descriptor: Arc::from(frozen.descriptor.as_str()),
                source_file: frozen.source_file.map(|s| Arc::from(s.as_str())),
                exception_table,
            },
            backward_count: 0,
            monitor_on_exit: None,
            is_jdk_class: false,
        }
    }

    // ── GC scanning and pointer update helpers ─────────────────────────

    /// Collect all non-null Object references from locals for GC root scanning.
    ///
    /// **Spring Boot SEGV fix (2026-05-15):** Previously this function also treated
    /// any `VTAG_LONG` local whose bits happened to look like an aligned object
    /// pointer (`jlong_bits_as_aligned_object_ptr`) as a root. That heuristic was
    /// added as a backstop for JIT/native bridges that occasionally smuggled
    /// `jobject` handles through `Value::Long`, but it is **not safe in the
    /// interpreter's local-variable scan**: by JVM spec a `VTAG_LONG` slot is a
    /// primitive `long`, never a heap reference. Locals are tag-typed via
    /// `astore`/`lstore` and `coerce_value_for_return` already promotes any
    /// smuggled jobject to `VTAG_OBJECT` before it reaches a local slot.
    ///
    /// In `enhanceConfigurationClasses` (Spring 5.3.27) the frame has 15 locals
    /// where some primitive `long` slots carried bit patterns that look like
    /// aligned heap pointers (low 3 bits = 0, value < 1<<48). The next safepoint
    /// (backward `goto 401`) would call this function, push the bogus pointer
    /// into the root set, and the GC then dereferenced garbage → `0xC0000005`
    /// SEGV. Removing the `VTAG_LONG` arm eliminates this entire class of false
    /// positives. See `applogs/letsgo-segv-diagnosis.md` for full evidence.
    pub fn scan_local_objects(&self, roots: &mut Vec<ObjectRef>) {
        for cv in &self.locals {
            // Only Object-tagged slots are heap references — same policy as
            // the prior `is_object_tag(VTAG_OBJECT)` check. Long / Null /
            // Int / Float / Double / ReturnAddress / Uninitialized are never
            // roots. `is_object()` returns false for null slots (which carry
            // `SUB_NULL`, not `SUB_OBJECT`), matching the prior behavior
            // where `bits != 0` filtered null-pointer slots.
            if cv.is_object() {
                if let Some(ptr) = cv.as_object_ptr() {
                    // Belt-and-suspenders: SUB_OBJECT is never constructed
                    // with a zero pointer (CompactValue::object panics on
                    // null), but defend against bit-corrupted slots.
                    if ptr != 0 {
                        roots.push(unsafe { ObjectRef::from_raw(ptr as *mut u8) });
                    }
                }
            }
        }
    }

    /// Update Object references in locals after GC using the pointer map.
    ///
    /// Symmetric with [`Self::scan_local_objects`]: only `VTAG_OBJECT` slots are
    /// heap references. `VTAG_LONG` slots are primitive `long`s by spec and must
    /// never be remapped by GC — doing so would corrupt a primitive value whose
    /// bits happened to look like a moved pointer (Spring Boot SEGV root cause,
    /// see `applogs/letsgo-segv-diagnosis.md`).
    pub fn update_local_refs(&mut self, pointer_map: &HashMap<usize, usize>) {
        for cv in self.locals.iter_mut() {
            if !cv.is_object() {
                continue;
            }
            let Some(old_ptr) = cv.as_object_ptr() else {
                continue;
            };
            if let Some(&new_addr) = pointer_map.get(&(old_ptr as usize)) {
                // Rebuild the Object CompactValue with the relocated pointer.
                // `CompactValue::object` panics on null or out-of-range, so
                // route through `try_from_pointer` to degrade safely if the
                // GC handed back an unexpected address (e.g. 0 = freed).
                *cv = CompactValue::try_from_pointer(new_addr as u64)
                    .unwrap_or_else(CompactValue::null);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_creation_with_args() {
        let frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            5,
            &[Value::Int(42), Value::Long(100)],
        );

        assert_eq!(frame.locals_len(), 5);
        assert_eq!(frame.get_local(0).as_int(), Some(42));
        // CompactValue stores Long untagged, so `get_local(...).as_long()`
        // on the `Value` round-trip cannot distinguish Long from Double.
        // Use the CompactValue accessor with context (Long-by-instruction).
        assert_eq!(frame.get_local_compact(1).as_long(), Some(100));
        // Slot 2 is the second half of the long → Uninitialized
    }

    /// If `Code.max_locals` is smaller than the invocation argument slots,
    /// the frame must still receive every argument (Surefire-style bad metadata).
    #[test]
    fn frame_expands_locals_when_declared_max_smaller_than_invoke_args() {
        let args = [Value::Int(10), Value::Int(20), Value::Int(30)];
        let frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            4,
            2,
            &args,
        );
        assert_eq!(frame.max_locals, 3);
        assert_eq!(frame.locals_len(), 3);
        assert_eq!(frame.get_local(0).as_int(), Some(10));
        assert_eq!(frame.get_local(1).as_int(), Some(20));
        assert_eq!(frame.get_local(2).as_int(), Some(30));
    }

    #[test]
    fn frame_expands_locals_for_category2_when_declared_too_small() {
        let args = [Value::Long(0x1122_3344_5566_7788)];
        let frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            4,
            1,
            &args,
        );
        assert_eq!(frame.max_locals, 2);
        assert_eq!(frame.locals_len(), 2);
        // Long/Double tag ambiguity in CompactValue — use the compact
        // accessor with explicit Long context.
        assert_eq!(
            frame.get_local_compact(0).as_long(),
            Some(0x1122_3344_5566_7788)
        );
        assert_eq!(frame.get_local(1), Value::Uninitialized);
    }

    #[test]
    fn frame_set_and_get_local() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            3,
            &[],
        );

        frame.set_local(0, Value::Int(1));
        frame.set_local(1, Value::Float(2.5));
        assert_eq!(frame.get_local(0).as_int(), Some(1));
        assert_eq!(frame.get_local(1).as_float(), Some(2.5));
    }

    #[test]
    fn frame_soa_all_types() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            8,
            &[],
        );

        frame.set_local(0, Value::Int(42));
        frame.set_local(1, Value::Long(9999999999));
        frame.set_local(2, Value::Float(3.15));
        frame.set_local(3, Value::Double(2.719));
        frame.set_local(4, Value::Object(None));
        frame.set_local(5, Value::Uninitialized);
        frame.set_local(6, Value::ReturnAddress(123));

        assert_eq!(frame.get_local(0).as_int(), Some(42));
        // Long via the context-aware compact accessor.
        assert_eq!(frame.get_local_compact(1).as_long(), Some(9999999999));
        assert!((frame.get_local(2).as_float().unwrap() - 3.15).abs() < 1e-6);
        assert!((frame.get_local(3).as_double().unwrap() - 2.719).abs() < 1e-9);
        assert!(frame.get_local(4).is_null());
        assert_eq!(frame.get_local(5), Value::Uninitialized);
        if let Value::ReturnAddress(a) = frame.get_local(6) {
            assert_eq!(a, 123);
        } else {
            panic!("expected ReturnAddress");
        }
    }

    #[test]
    fn frame_get_local_raw() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            3,
            &[Value::Int(42), Value::Long(100)],
        );

        assert_eq!(frame.get_local_raw(0), 42);
        assert_eq!(frame.get_local_raw(1) as i64, 100);
    }

    /// A `VTAG_LONG` local whose bits happen to look like an aligned object
    /// pointer is **NOT** a heap root — primitives are not references by JVM
    /// spec, and the bridges that used to smuggle `jobject` through `Long`
    /// now promote them to `VTAG_OBJECT` via `coerce_value_for_return` before
    /// the value ever reaches a local. This test guards against the Spring
    /// Boot SEGV regression where pointer-shaped long bits were mis-rooted.
    #[test]
    fn scan_local_objects_does_not_root_long_with_pointer_shaped_bits() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        let fake = 0x1000usize as i64;
        frame.set_local_unchecked(0, Value::Long(fake));

        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots);
        assert!(roots.is_empty(), "VTAG_LONG must never produce a root");
    }

    #[test]
    fn scan_local_objects_skips_long_that_is_not_aligned_object_pattern() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        frame.set_local_unchecked(0, Value::Long(7));

        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots);
        assert!(roots.is_empty());
    }

    /// Symmetric with `scan_local_objects`: `VTAG_LONG` slots are primitives,
    /// GC must NOT remap them even if their bits look like a moved pointer.
    #[test]
    fn update_local_refs_does_not_touch_long_with_pointer_shaped_bits() {
        use std::collections::HashMap;

        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        let old = 0x2000usize as i64;
        frame.set_local_unchecked(0, Value::Long(old));

        let mut map = HashMap::new();
        map.insert(0x2000usize, 0x3000usize);
        frame.update_local_refs(&map);

        // Long primitive must be preserved verbatim. Use the compact
        // accessor (CompactValue's Long/Double tag ambiguity is resolved
        // by the caller's instruction context).
        assert_eq!(frame.get_local_compact(0).as_long(), Some(0x2000));
    }

    #[test]
    #[should_panic]
    fn get_local_unchecked_panics_out_of_bounds() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        let _ = frame.get_local_unchecked(99); // should panic
    }

    #[test]
    #[should_panic]
    fn set_local_unchecked_panics_out_of_bounds() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        frame.set_local_unchecked(99, Value::Int(1)); // should panic
    }

    #[test]
    fn get_local_out_of_bounds_returns_uninitialized() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        assert_eq!(frame.get_local(99), Value::Uninitialized);
    }

    #[test]
    fn set_local_out_of_bounds_is_no_op() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        frame.set_local(99, Value::Int(42)); // should not panic
        assert_eq!(frame.get_local(99), Value::Uninitialized);
    }

    #[test]
    fn frame_metadata_accessors() {
        let frame = Frame::new(
            ClassId::new(5),
            "com/example/Foo".to_string(),
            "bar".to_string(),
            "(I)V".to_string(),
            Some("Foo.java".to_string()),
            vec![0xb1], // return void
            vec![],
            10,
            3,
            &[],
        );
        assert_eq!(frame.class_name(), "com/example/Foo");
        assert_eq!(frame.method_name(), "bar");
        assert_eq!(frame.method_descriptor(), "(I)V");
        assert_eq!(frame.source_file(), Some("Foo.java"));
        assert_eq!(frame.class_id, ClassId::new(5));
    }

    #[test]
    fn padded_bytecode_adds_trailing_zeros() {
        let code = vec![0x2a, 0xb1]; // aload_0, return
        let padded = padded_bytecode(&code);
        assert_eq!(padded.len(), 4);
        assert_eq!(padded[0], 0x2a);
        assert_eq!(padded[1], 0xb1);
        assert_eq!(padded[2], 0);
        assert_eq!(padded[3], 0);
    }

    #[test]
    fn frame_recycle_returns_vecs() {
        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            5,
            &[Value::Int(1)],
        );
        frame.recycle(&mut locals_pool, &mut stacks_pool);
        assert_eq!(locals_pool.len(), 1);
        assert_eq!(stacks_pool.len(), 1);
    }

    #[test]
    #[should_panic]
    fn get_local_raw_panics_out_of_bounds() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        let _ = frame.get_local_raw(99); // should panic
    }

    #[test]
    #[should_panic]
    fn get_local_tag_panics_out_of_bounds() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        let _ = frame.get_local_tag(99); // should panic
    }

    // ── Additional edge case tests ────────────────────────────────────

    #[test]
    fn frame_creation_with_zero_locals() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            4,
            0, // zero locals
            &[],
        );
        assert_eq!(frame.locals_len(), 0);
        // get_local out of bounds returns Uninitialized
        assert_eq!(frame.get_local(0), Value::Uninitialized);
    }

    #[test]
    fn frame_creation_with_max_locals() {
        // Use a large max_locals value
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            u16::MAX, // 65535 locals
            &[],
        );
        assert_eq!(frame.locals_len(), u16::MAX as usize);
        // All locals should be uninitialized
        assert_eq!(frame.get_local(0), Value::Uninitialized);
        assert_eq!(frame.get_local(100), Value::Uninitialized);
        assert_eq!(frame.get_local(65534), Value::Uninitialized);
    }

    #[test]
    fn frame_local_set_get_boundary() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            3,
            &[],
        );
        // Set first and last local
        frame.set_local(0, Value::Int(100));
        frame.set_local(2, Value::Int(200));
        assert_eq!(frame.get_local(0).as_int(), Some(100));
        assert_eq!(frame.get_local(2).as_int(), Some(200));
        // Middle is still uninitialized
        assert_eq!(frame.get_local(1), Value::Uninitialized);

        // Out of bounds set is a no-op
        frame.set_local(3, Value::Int(300));
        assert_eq!(frame.get_local(3), Value::Uninitialized);
    }

    #[test]
    fn exception_table_lookup_matching() {
        let exception_table = vec![
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 10,
                handler_pc: 20,
                catch_type: 5, // specific exception class
            },
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 10,
                handler_pc: 30,
                catch_type: 0, // catch-all (finally)
            },
        ];
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            exception_table,
            4,
            2,
            &[],
        );
        let table = frame.exception_table();
        assert_eq!(table.len(), 2);
        // First entry covers pc 0..10, handler at 20
        assert_eq!(table[0].start_pc, 0);
        assert_eq!(table[0].end_pc, 10);
        assert_eq!(table[0].handler_pc, 20);
        assert_eq!(table[0].catch_type, 5);
    }

    #[test]
    fn exception_table_non_matching_range() {
        let exception_table = vec![ExceptionTableEntry {
            start_pc: 10,
            end_pc: 20,
            handler_pc: 30,
            catch_type: 0,
        }];
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            exception_table,
            4,
            2,
            &[],
        );
        let table = frame.exception_table();
        // PC 5 is outside [10, 20), so no handler matches
        let handler = table
            .iter()
            .find(|e| 5 >= e.start_pc as usize && 5 < e.end_pc as usize);
        assert!(handler.is_none());
    }

    #[test]
    fn exception_table_nested_handlers() {
        let exception_table = vec![
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 50,
                handler_pc: 100,
                catch_type: 0, // outer catch-all
            },
            ExceptionTableEntry {
                start_pc: 10,
                end_pc: 30,
                handler_pc: 60,
                catch_type: 5, // inner specific
            },
        ];
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            exception_table,
            4,
            2,
            &[],
        );
        let table = frame.exception_table();
        // PC 15 is inside both ranges
        let matches: Vec<_> = table
            .iter()
            .filter(|e| 15 >= e.start_pc as usize && 15 < e.end_pc as usize)
            .collect();
        assert_eq!(matches.len(), 2);
        // First match is the outer handler, second is inner
        assert_eq!(matches[0].handler_pc, 100);
        assert_eq!(matches[1].handler_pc, 60);
    }

    #[test]
    fn frame_with_no_exception_handlers() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![], // no exception handlers
            4,
            2,
            &[],
        );
        assert!(frame.exception_table().is_empty());
    }

    #[test]
    fn frame_source_file_none() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None, // no source file
            vec![],
            vec![],
            4,
            2,
            &[],
        );
        assert!(frame.source_file().is_none());
        assert!(frame.source_file_arc().is_none());
    }

    #[test]
    fn frame_pc_starts_at_zero() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0x2a, 0xb1],
            vec![],
            4,
            2,
            &[],
        );
        assert_eq!(frame.pc, 0);
        assert_eq!(frame.backward_count, 0);
    }

}
