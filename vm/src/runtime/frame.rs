//! A single execution frame (stack frame) in the JVM.
//!
//! Each method invocation creates a new `Frame` containing:
//! - Local variables (SoA: u64 values + u8 tags)
//! - Operand stack (SoA: u64 values + u8 tags)
//! - Program counter
//! - Method bytecode and exception table

use std::collections::HashMap;
use std::sync::Arc;

use rustjvm_reader::attribute::ExceptionTableEntry;

use crate::classloading::resolution::CachedBytecodeMethod;
use crate::classloading::ClassId;
use crate::runtime::ValueStack;
use crate::types::{
    decode_value, encode_value, is_object_tag, CompactValue, ObjectRef, Value, VTAG_DOUBLE,
    VTAG_FLOAT, VTAG_INT, VTAG_LONG, VTAG_NULL, VTAG_OBJECT, VTAG_RETADDR, VTAG_UNINIT,
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
/// Locals use SoA layout: separate u64 values + u8 tags for cache efficiency.
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

    /// Local variable values (SoA: raw u64 data).
    local_vals: Vec<u64>,

    /// Local variable type tags (SoA: 1 byte per slot).
    local_tags: Vec<u8>,

    /// Operand stack (SoA internally).
    pub stack: ValueStack,

    /// The raw bytecode of the method (hot path — kept as direct Arc).
    pub code: Arc<[u8]>,

    /// Maximum operand stack depth (from Code attribute).
    pub max_stack: u16,

    /// Maximum local variable count (from Code attribute).
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

fn init_locals(max_locals: u16, args: &[Value]) -> (Vec<u64>, Vec<u8>) {
    let n = max_locals as usize;
    let mut vals = vec![0u64; n];
    let mut tags = vec![VTAG_UNINIT; n];
    copy_args_to_locals(&mut vals, &mut tags, args);
    (vals, tags)
}

fn init_locals_pooled(
    max_locals: u16,
    args: &[Value],
    pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
) -> (Vec<u64>, Vec<u8>) {
    let n = max_locals as usize;
    let (mut vals, mut tags) = pool.pop().unwrap_or_default();
    vals.clear();
    vals.resize(n, 0u64);
    tags.clear();
    tags.resize(n, VTAG_UNINIT);
    copy_args_to_locals(&mut vals, &mut tags, args);
    (vals, tags)
}

fn copy_args_to_locals(vals: &mut [u64], tags: &mut [u8], args: &[Value]) {
    let mut slot = 0;
    for arg in args {
        if slot < vals.len() {
            let (v, t) = encode_value(*arg);
            vals[slot] = v;
            tags[slot] = t;
            slot += 1;
            // Category 2 values (long, double) occupy two slots.
            if arg.is_category2() && slot < vals.len() {
                vals[slot] = 0;
                tags[slot] = VTAG_UNINIT;
                slot += 1;
            }
        }
    }
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

/// Convert a CompactValue back into a local-slot (u64 + tag) pair so the
/// existing SoA local storage invariants are preserved.
#[inline(always)]
fn compact_to_local_slot(cv: CompactValue) -> (u64, u8) {
    use crate::types::CompactTag;
    match cv.tag() {
        CompactTag::Int => (cv.as_int().unwrap_or(0) as u32 as u64, VTAG_INT),
        CompactTag::Long => (cv.as_long_unchecked() as u64, VTAG_LONG),
        CompactTag::Float => (cv.as_float().unwrap_or(0.0).to_bits() as u64, VTAG_FLOAT),
        CompactTag::Double => {
            // Untagged — raw bits. Could be a Double or a Long that was
            // created via CompactValue::long (both untagged).  Prefer the
            // Double tag; stores from Lstore use set_local with Value::Long
            // which goes through encode_value.
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
        let (local_vals, local_tags) = init_locals(max_locals, args);
        let is_jdk = class_name.starts_with("java/")
            || class_name.starts_with("jdk/")
            || class_name.starts_with("sun/")
            || class_name.starts_with("com/sun/");
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            local_vals,
            local_tags,
            stack: ValueStack::new((max_stack as usize).max(16) + 8),
            code: padded_bytecode(&code),
            max_stack,
            max_locals,
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
        let (local_vals, local_tags) = init_locals(max_locals, args);
        let is_jdk = class_name.starts_with("java/")
            || class_name.starts_with("jdk/")
            || class_name.starts_with("sun/")
            || class_name.starts_with("com/sun/");
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            local_vals,
            local_tags,
            stack: ValueStack::new((max_stack as usize).max(16) + 8),
            code,
            max_stack,
            max_locals,
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
        let (local_vals, local_tags) = init_locals_pooled(max_locals, args, locals_pool);
        let padded_max = (max_stack as usize).max(16) + 8;
        let stack = if let Some((vals, tags)) = stacks_pool.pop() {
            ValueStack::from_pooled(vals, tags, padded_max)
        } else {
            ValueStack::new(padded_max)
        };
        let is_jdk = class_name.starts_with("java/")
            || class_name.starts_with("jdk/")
            || class_name.starts_with("sun/")
            || class_name.starts_with("com/sun/");
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            local_vals,
            local_tags,
            stack,
            code,
            max_stack,
            max_locals,
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
        let (local_vals, local_tags) = init_locals_pooled(cached.max_locals, args, locals_pool);
        let padded_max = (cached.max_stack as usize).max(16) + 8;
        let stack = if let Some((vals, tags)) = stacks_pool.pop() {
            ValueStack::from_pooled(vals, tags, padded_max)
        } else {
            ValueStack::new(padded_max)
        };
        let class_id = cached.declaring_class_id;
        let code = cached.code.clone();
        let max_stack = cached.max_stack;
        let max_locals = cached.max_locals;
        let is_jdk = cached.class_name.starts_with("java/")
            || cached.class_name.starts_with("jdk/")
            || cached.class_name.starts_with("sun/")
            || cached.class_name.starts_with("com/sun/");
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            local_vals,
            local_tags,
            stack,
            code,
            max_stack,
            max_locals,
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
        self.max_locals = max_locals;
        // Update is_jdk_class for correct fast/slow path dispatch
        self.is_jdk_class = class_name.starts_with("java/")
            || class_name.starts_with("jdk/")
            || class_name.starts_with("sun/")
            || class_name.starts_with("com/sun/");
        // Update inner metadata so class_name(), method_name(), exception_table() are correct
        self.inner = FrameInner::Owned {
            class_name,
            method_name,
            method_descriptor: descriptor,
            source_file,
            exception_table,
        };
        // Reset locals
        let n = max_locals as usize;
        self.local_vals.clear();
        self.local_vals.resize(n, 0u64);
        self.local_tags.clear();
        self.local_tags.resize(n, VTAG_UNINIT);
        copy_args_to_locals(&mut self.local_vals, &mut self.local_tags, args);
        // Reset operand stack
        self.stack.clear();
    }

    /// Return this frame's Vec allocations to the pool for reuse.
    pub fn recycle(
        self,
        locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
        stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    ) {
        locals_pool.push((self.local_vals, self.local_tags));
        stacks_pool.push(self.stack.into_inner());
    }

    /// T10.7 — consume the frame and return its four pooled `Vec`s as
    /// `(local_vals, local_tags, stack_vals, stack_tags)`.
    ///
    /// Used by `JvmThread::recycle_frame_with_shared` to decide per-vector
    /// whether to keep the allocation in the thread-local pool or spill it
    /// into the VM-wide `VecPool` on `SharedVm`.
    pub fn take_pool_parts(self) -> (Vec<u64>, Vec<u8>, Vec<u64>, Vec<u8>) {
        let (stack_vals, stack_tags) = self.stack.into_inner();
        (self.local_vals, self.local_tags, stack_vals, stack_tags)
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
        self.local_vals.len()
    }

    /// Get a local variable by index.
    pub fn get_local(&self, index: u16) -> Value {
        let i = index as usize;
        if i >= self.local_vals.len() {
            return Value::Uninitialized;
        }
        decode_value(self.local_vals[i], self.local_tags[i])
    }

    /// Get a local variable by index without error wrapping.
    /// Used by the fast-path interpreter for verified bytecode.
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn get_local_unchecked(&self, index: usize) -> Value {
        let v = self.local_vals[index];
        let t = self.local_tags[index];
        decode_value(v, t)
    }

    /// Set a local variable by index.
    pub fn set_local(&mut self, index: u16, value: Value) {
        let i = index as usize;
        if i < self.local_vals.len() {
            let (v, t) = encode_value(value);
            self.local_vals[i] = v;
            self.local_tags[i] = t;
        }
    }

    /// Set a local variable by index without error wrapping.
    /// Used by the fast-path interpreter for verified bytecode.
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn set_local_unchecked(&mut self, index: usize, value: Value) {
        let (v, t) = encode_value(value);
        self.local_vals[index] = v;
        self.local_tags[index] = t;
    }

    /// Get the raw u64 value of a local (for JIT/OSR interop).
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn get_local_raw(&self, index: usize) -> u64 {
        self.local_vals[index]
    }

    /// Get the tag of a local (for JIT/OSR interop).
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn get_local_tag(&self, index: usize) -> u8 {
        self.local_tags[index]
    }

    /// Get a local as a `CompactValue` without the `Value` enum round-trip
    /// (T10.9.D hot-path).  Bounds-safe: out-of-range returns
    /// `CompactValue::uninitialized()` to preserve prior `get_local` semantics.
    #[inline(always)]
    pub fn get_local_compact(&self, index: u16) -> CompactValue {
        let i = index as usize;
        if i >= self.local_vals.len() {
            return CompactValue::uninitialized();
        }
        local_slot_to_compact(self.local_vals[i], self.local_tags[i])
    }

    /// Set a local from a `CompactValue` (T10.9.D hot-path).  Silently no-ops
    /// on out-of-range index to mirror `set_local`.
    #[inline(always)]
    pub fn set_local_compact(&mut self, index: u16, cv: CompactValue) {
        let i = index as usize;
        if i < self.local_vals.len() {
            let (v, t) = compact_to_local_slot(cv);
            self.local_vals[i] = v;
            self.local_tags[i] = t;
        }
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
    pub fn locals_snapshot(&self) -> (Vec<u64>, Vec<u8>) {
        (self.local_vals.clone(), self.local_tags.clone())
    }

    /// Freeze this frame into a `FrozenFrame` that can be stored in a continuation.
    /// Captures all state needed to reconstruct the frame later.
    pub fn to_frozen_frame(&self) -> crate::threading::virtual_threads::FrozenFrame {
        let (stack_vals, stack_tags) = self.stack.snapshot_raw();
        crate::threading::virtual_threads::FrozenFrame {
            class_name: self.class_name().to_string(),
            method_name: self.method_name().to_string(),
            descriptor: self.method_descriptor().to_string(),
            bytecode_pc: self.pc,
            locals: self.local_vals.clone(),
            local_tags: self.local_tags.clone(),
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

        Self {
            class_id,
            pc: frozen.bytecode_pc,
            last_instr_pc: frozen.bytecode_pc,
            local_vals: frozen.locals,
            local_tags: frozen.local_tags,
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
    pub fn scan_local_objects(&self, roots: &mut Vec<ObjectRef>) {
        for i in 0..self.local_vals.len() {
            if is_object_tag(self.local_tags[i]) {
                let ptr = self.local_vals[i];
                if ptr != 0 {
                    roots.push(unsafe { ObjectRef::from_raw(ptr as *mut u8) });
                }
            } else if self.local_tags[i] == VTAG_NULL {
                // Null object — not a root
            }
        }
    }

    /// Update Object references in locals after GC using the pointer map.
    pub fn update_local_refs(&mut self, pointer_map: &HashMap<usize, usize>) {
        for i in 0..self.local_vals.len() {
            if is_object_tag(self.local_tags[i]) {
                let old_ptr = self.local_vals[i] as usize;
                if let Some(&new_addr) = pointer_map.get(&old_ptr) {
                    self.local_vals[i] = new_addr as u64;
                }
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
        assert_eq!(frame.get_local(1).as_long(), Some(100));
        // Slot 2 is the second half of the long → Uninitialized
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
        assert_eq!(frame.get_local(1).as_long(), Some(9999999999));
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

    #[test]
    fn frame_args_overflow_locals_silently_truncates() {
        // More args than local slots — only those that fit are copied
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            2, // only 2 local slots
            &[Value::Int(1), Value::Int(2), Value::Int(3)], // 3 args
        );
        assert_eq!(frame.get_local(0).as_int(), Some(1));
        assert_eq!(frame.get_local(1).as_int(), Some(2));
        // Third arg doesn't fit
        assert_eq!(frame.get_local(2), Value::Uninitialized);
    }
}
