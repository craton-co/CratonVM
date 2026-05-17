use std::collections::HashMap;

use crate::error::RuntimeError;
use crate::memory::VmHeap;
use crate::types::{jlong_bits_as_aligned_object_ptr, CompactTag, CompactValue, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Diagnostic instrumentation (K1 stack-tag mismatch hunt).
//
// Compiled only in `debug_assertions` builds, and further gated at runtime by
// RUSTJVM_DEBUG_STACK_TAG=1.  Release builds see a pure no-op stub that the
// optimizer strips entirely.  The machinery remains in tree so future stack-
// tag regressions can be diagnosed without re-deriving the harness — reverse
// the `cfg(debug_assertions)` pair below to re-enable for release profiling.
// ---------------------------------------------------------------------------

#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(debug_assertions)]
static DIAG_ENABLED: AtomicBool = AtomicBool::new(false);
#[cfg(debug_assertions)]
static DIAG_INIT: std::sync::Once = std::sync::Once::new();

#[cfg(debug_assertions)]
#[inline(always)]
fn stack_diag_enabled() -> bool {
    DIAG_INIT.call_once(|| {
        if std::env::var("RUSTJVM_DEBUG_STACK_TAG").ok().as_deref() == Some("1") {
            DIAG_ENABLED.store(true, Ordering::Relaxed);
        }
    });
    DIAG_ENABLED.load(Ordering::Relaxed)
}

#[cfg(debug_assertions)]
thread_local! {
    static STACK_DIAG_CTX: std::cell::Cell<StackDiagCtx> = const { std::cell::Cell::new(StackDiagCtx::empty()) };
}

#[cfg(debug_assertions)]
#[derive(Clone, Copy)]
struct StackDiagCtx {
    pc: u32,
    opcode: u8,
    class_ptr: *const u8,
    class_len: u16,
    method_ptr: *const u8,
    method_len: u16,
}

#[cfg(debug_assertions)]
impl StackDiagCtx {
    const fn empty() -> Self {
        Self {
            pc: 0,
            opcode: 0,
            class_ptr: std::ptr::null(),
            class_len: 0,
            method_ptr: std::ptr::null(),
            method_len: 0,
        }
    }
}

/// Update the per-thread diagnostic context just before each opcode dispatch.
/// Release builds compile to an empty function; debug builds early-return
/// unless `RUSTJVM_DEBUG_STACK_TAG=1` is set.
///
/// SAFETY: the pointers stored here reference Arc<str> data owned by the
/// current Frame; the Cell is only read inside the same interpreter tick
/// (while the frame is still alive), so the pointers remain valid.
#[cfg(debug_assertions)]
#[inline(always)]
pub fn update_diag_ctx(class_name: &str, method_name: &str, pc: usize, opcode: u8) {
    if !stack_diag_enabled() {
        return;
    }
    STACK_DIAG_CTX.with(|c| {
        c.set(StackDiagCtx {
            pc: pc as u32,
            opcode,
            class_ptr: class_name.as_ptr(),
            class_len: class_name.len().min(u16::MAX as usize) as u16,
            method_ptr: method_name.as_ptr(),
            method_len: method_name.len().min(u16::MAX as usize) as u16,
        });
    });
}

#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn update_diag_ctx(_class_name: &str, _method_name: &str, _pc: usize, _opcode: u8) {}

#[cfg(debug_assertions)]
fn log_tag_mismatch(expected: &str, cv: CompactValue, stack_len: usize) {
    if !stack_diag_enabled() {
        return;
    }
    let ctx = STACK_DIAG_CTX.with(|c| c.get());
    let class = if !ctx.class_ptr.is_null() {
        // SAFETY: ctx pointers reference Arc<str> data alive for the current
        // opcode tick. Slice is reconstructed within the same tick.
        unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                ctx.class_ptr,
                ctx.class_len as usize,
            ))
            .to_string()
        }
    } else {
        String::from("<unknown>")
    };
    let method = if !ctx.method_ptr.is_null() {
        unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                ctx.method_ptr,
                ctx.method_len as usize,
            ))
            .to_string()
        }
    } else {
        String::from("<unknown>")
    };
    tracing::error!(
        target: "rustjvm_stack_tag_error",
        expected = expected,
        actual = ?cv.tag(),
        raw_bits = format!("{:#018x}", cv.to_bits()).as_str(),
        class = class.as_str(),
        method = method.as_str(),
        pc = ctx.pc,
        opcode = format!("{:#04x}", ctx.opcode).as_str(),
        slot_depth = stack_len,
        "stack-tag mismatch"
    );
}

#[cfg(not(debug_assertions))]
#[inline(always)]
fn log_tag_mismatch(_expected: &str, _cv: CompactValue, _stack_len: usize) {}

/// The operand stack for a single method frame.
///
/// T10.6 — the operand stack now stores slots as 8-byte NaN-boxed
/// [`CompactValue`]s, halving the per-slot footprint compared to the 16-byte
/// `Value` enum and shaving one byte per slot vs. the prior SoA
/// `(Vec<u64>, Vec<u8>)` layout.
///
/// The public API preserves the same `Value`-based signatures as the SoA
/// implementation so the 500+ interpreter call sites are unaffected; the
/// conversion `CompactValue::from_value` / `CompactValue::to_value` is
/// `#[inline(always)]` and degrades to a handful of bit-ops per push / pop.
#[derive(Debug)]
pub struct ValueStack {
    slots: Vec<CompactValue>,
    len: usize,
    max_size: usize,
}

impl ValueStack {
    pub fn new(max_size: usize) -> Self {
        let slots = vec![CompactValue::zero(); max_size];
        Self {
            slots,
            len: 0,
            max_size,
        }
    }

    /// Create a ValueStack reusing pooled Vecs (clears and resizes them).
    /// Ensures at least `max_size` capacity for unsafe push operations.
    ///
    /// The incoming `vals` is treated as a pool of raw u64 slots; the
    /// `tags` vec is ignored (tags are encoded inline via NaN-boxing) — the
    /// signature is preserved so existing pool callers stay unchanged.
    pub fn from_pooled(vals: Vec<u64>, _tags: Vec<u8>, max_size: usize) -> Self {
        // CompactValue is repr(transparent) over u64 — Vec<u64> can be
        // transmuted to Vec<CompactValue> without reallocation.
        let mut slots = u64_vec_to_compact(vals);
        slots.clear();
        slots.resize(max_size, CompactValue::zero());
        Self {
            slots,
            len: 0,
            max_size,
        }
    }

    /// Consume this stack and return the inner Vecs for pooling.
    ///
    /// The tag half is returned as an empty Vec (CompactValue encodes its
    /// type tag inline); callers that pool both halves will simply see a
    /// fresh empty allocation for the tag slot.
    pub fn into_inner(self) -> (Vec<u64>, Vec<u8>) {
        (compact_vec_to_u64(self.slots), Vec::new())
    }

    pub fn push(&mut self, value: Value) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack overflow".to_string(),
            });
        }
        self.slots[self.len] = CompactValue::from_value(value);
        self.len += 1;
        Ok(())
    }

    /// Push without error wrapping. Used by the fast-path interpreter
    /// for verified bytecode where stack overflow is impossible.
    ///
    /// # Panics
    /// Panics if the stack is full. This is a safety net — verified bytecode
    /// should never trigger this.
    #[inline(always)]
    pub fn push_unchecked(&mut self, value: Value) {
        assert!(self.len < self.max_size, "stack overflow in push_unchecked");
        self.slots[self.len] = CompactValue::from_value(value);
        self.len += 1;
    }

    /// Push a pre-encoded `CompactValue` directly — zero-cost over writing
    /// `slots[n] = cv`. Used internally where the compact form is already
    /// in hand (e.g. `dup`, `swap`, local reload).
    ///
    /// # Panics
    /// Panics if the stack is full.
    #[inline(always)]
    pub fn push_compact(&mut self, cv: CompactValue) {
        assert!(self.len < self.max_size, "stack overflow in push_compact");
        self.slots[self.len] = cv;
        self.len += 1;
    }

    /// Push an int directly as a CompactValue (T10.9.D hot-path).
    ///
    /// Returns `Err` on overflow so the signature mirrors `push(Value)`.
    #[inline(always)]
    pub fn push_int(&mut self, v: i32) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack overflow".to_string(),
            });
        }
        self.slots[self.len] = CompactValue::int(v);
        self.len += 1;
        Ok(())
    }

    /// Push a long directly as a CompactValue (T10.9.D hot-path).
    #[inline(always)]
    pub fn push_long(&mut self, v: i64) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack overflow".to_string(),
            });
        }
        self.slots[self.len] = CompactValue::long(v);
        self.len += 1;
        Ok(())
    }

    /// Push a float directly as a CompactValue (T10.9.D hot-path).
    #[inline(always)]
    pub fn push_float(&mut self, v: f32) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack overflow".to_string(),
            });
        }
        self.slots[self.len] = CompactValue::float(v);
        self.len += 1;
        Ok(())
    }

    /// Push a double directly as a CompactValue (T10.9.D hot-path).
    #[inline(always)]
    pub fn push_double(&mut self, v: f64) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack overflow".to_string(),
            });
        }
        self.slots[self.len] = CompactValue::double(v);
        self.len += 1;
        Ok(())
    }

    /// Push `null` directly as a CompactValue (T10.9.D hot-path).
    #[inline(always)]
    pub fn push_null(&mut self) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack overflow".to_string(),
            });
        }
        self.slots[self.len] = CompactValue::null();
        self.len += 1;
        Ok(())
    }

    pub fn pop(&mut self) -> Result<Value, RuntimeError> {
        if self.len == 0 {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack underflow".to_string(),
            });
        }
        self.len -= 1;
        Ok(self.slots[self.len].to_value())
    }

    /// Pop without error wrapping. Used by the fast-path interpreter
    /// for verified bytecode where stack underflow is impossible.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn pop_unchecked(&mut self) -> Value {
        assert!(self.len > 0, "stack underflow in pop_unchecked");
        self.len -= 1;
        self.slots[self.len].to_value()
    }

    /// Pop a raw `CompactValue` slot without decoding to `Value`.
    /// Used by `dup`/`swap`/`dup2`-style opcodes that simply copy bits.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn pop_compact(&mut self) -> CompactValue {
        assert!(self.len > 0, "stack underflow in pop_compact");
        self.len -= 1;
        self.slots[self.len]
    }

    // ── AUDIT CRIT-4: int-specialised push/pop (no Value enum round-trip) ──
    //
    // Hot int opcodes (`iadd`, `imul`, `iload_*`, `istore_*`, `if_icmp*` …)
    // previously paid 3× 8-arm `match` per execution: pop → Value::Int, pop
    // → Value::Int, push Value::Int(_). These helpers stay in CompactValue
    // form throughout, bypassing the `to_value` / `from_value` arms.
    //
    // Bytecode verifier guarantees the operand types, so the unchecked
    // helpers do not type-check — the same contract as `pop_unchecked` /
    // `push_unchecked`.

    /// Push an i32 directly as a `CompactValue::int(_)` without going
    /// through `Value::Int(_) → from_value(_)`.
    ///
    /// # Panics
    /// Panics if the stack is full.
    #[inline(always)]
    pub fn push_int_unchecked(&mut self, v: i32) {
        assert!(self.len < self.max_size, "stack overflow in push_int_unchecked");
        self.slots[self.len] = CompactValue::int(v);
        self.len += 1;
    }

    /// Pop an i32 directly from a `CompactValue::int(_)` slot without
    /// going through `to_value()` / `Value::Int(_)`.
    ///
    /// Bytecode verifier guarantees the top-of-stack is an Int slot at
    /// every site that calls this helper.  If the slot is something else
    /// (`Null`, `Uninitialized`) this returns 0 — matching the slow-path
    /// `pop_int` semantics for the K1-family null/uninit coercion.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn pop_int_unchecked(&mut self) -> i32 {
        assert!(self.len > 0, "stack underflow in pop_int_unchecked");
        self.len -= 1;
        // CompactValue exposes `as_int() -> Option<i32>` (None when the slot
        // is not an Int tag).  Verified bytecode is Int-typed at this site,
        // so `unwrap_or(0)` matches the slow-path null/uninit coercion and
        // compiles down to the same single payload mask + cast after
        // inlining.
        self.slots[self.len].as_int().unwrap_or(0)
    }

    /// Push an f32 directly as a `CompactValue::float(_)` (skip `Value` decode).
    ///
    /// # Panics
    /// Panics if the stack is full.
    #[inline(always)]
    pub fn push_float_unchecked(&mut self, v: f32) {
        assert!(self.len < self.max_size, "stack overflow in push_float_unchecked");
        self.slots[self.len] = CompactValue::float(v);
        self.len += 1;
    }

    /// Pop an f32 directly from a `CompactValue::float(_)` slot.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn pop_float_unchecked(&mut self) -> f32 {
        assert!(self.len > 0, "stack underflow in pop_float_unchecked");
        self.len -= 1;
        self.slots[self.len].as_float().unwrap_or(0.0)
    }

    /// Push an f64 directly as a `CompactValue::double(_)` (skip `Value` decode).
    ///
    /// Round-2 VM review: mirrors `push_float_unchecked` for the d-arithmetic
    /// hot path (`dadd`, `dsub`, `dmul`, `ddiv`).
    ///
    /// # Panics
    /// Panics if the stack is full.
    #[inline(always)]
    pub fn push_double_unchecked(&mut self, v: f64) {
        assert!(self.len < self.max_size, "stack overflow in push_double_unchecked");
        self.slots[self.len] = CompactValue::double(v);
        self.len += 1;
    }

    /// Pop an f64 directly from a `CompactValue::double(_)` slot.
    ///
    /// Doubles are stored as raw f64 bits in the CompactValue (with NaN-tag
    /// collisions canonicalised on write).  Verified bytecode guarantees this
    /// slot was produced by a Double-producing opcode (dconst, dload, dadd …),
    /// so reading the raw bits is the JVMS-correct decode.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn pop_double_unchecked(&mut self) -> f64 {
        assert!(self.len > 0, "stack underflow in pop_double_unchecked");
        self.len -= 1;
        // CompactValue::double(v) stores `v.to_bits()` verbatim (or canonical
        // NaN on collision), and there is no separate Double sub-tag — the
        // slot's raw u64 IS the double's bit pattern. Same decode policy as
        // pop_double's CompactTag::Double arm.
        f64::from_bits(self.slots[self.len].to_bits())
    }

    /// Push an i64 directly as a `CompactValue::long(_)` (skip `Value` decode).
    ///
    /// Round-2 VM review: mirrors `push_int_unchecked` for long ops where the
    /// caller has the raw i64 in hand.
    ///
    /// # Panics
    /// Panics if the stack is full.
    #[inline(always)]
    pub fn push_long_unchecked(&mut self, v: i64) {
        assert!(self.len < self.max_size, "stack overflow in push_long_unchecked");
        self.slots[self.len] = CompactValue::long(v);
        self.len += 1;
    }

    /// Pop an i64 directly from the top stack slot.
    ///
    /// Verified bytecode guarantees the slot is a long (either tagged Long or
    /// untagged Double — both store raw i64 bits in the CompactValue
    /// representation). Mirrors `pop_long`'s fast-path arms without the
    /// per-slot tag-match overhead.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn pop_long_unchecked(&mut self) -> i64 {
        assert!(self.len > 0, "stack underflow in pop_long_unchecked");
        self.len -= 1;
        // Both CompactTag::Long (NaN-tag collision) and CompactTag::Double
        // (untagged) store the raw i64 bits directly in self.0 — see
        // `pop_long`'s Long|Double arm.
        self.slots[self.len].as_long_unchecked()
    }

    /// Peek the top-of-stack as an i32 without popping.  Returns 0 for
    /// non-Int slots (matches `pop_int_unchecked`).
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn peek_int_unchecked(&self) -> i32 {
        assert!(self.len > 0, "stack underflow in peek_int_unchecked");
        self.slots[self.len - 1].as_int().unwrap_or(0)
    }

    /// Pop raw u64 value without decoding to Value enum.
    /// Used by JIT dispatch to avoid decode/re-encode overhead.
    #[inline(always)]
    pub fn pop_raw(&mut self) -> u64 {
        assert!(self.len > 0, "stack underflow in pop_raw");
        self.len -= 1;
        self.slots[self.len].to_bits()
    }

    pub fn peek_checked(&self) -> Result<Value, RuntimeError> {
        if self.len == 0 {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack underflow".to_string(),
            });
        }
        Ok(self.slots[self.len - 1].to_value())
    }

    /// Peek at the top value. Used by the fast-path interpreter.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn peek(&self) -> Value {
        assert!(self.len > 0, "stack underflow in peek");
        self.slots[self.len - 1].to_value()
    }

    /// Peek the top slot as a raw `CompactValue` (zero-copy).
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn peek_compact(&self) -> CompactValue {
        assert!(self.len > 0, "stack underflow in peek_compact");
        self.slots[self.len - 1]
    }

    /// Peek at a value at `offset` positions from the top (0 = top, 1 = second from top, etc.).
    /// Used by invokevirtual fast path to read the receiver under method arguments.
    ///
    /// # Panics
    /// Panics if `offset_from_top >= self.len`.
    #[inline(always)]
    pub fn peek_at(&self, offset_from_top: usize) -> Value {
        assert!(offset_from_top < self.len, "stack underflow in peek_at");
        self.slots[self.len - 1 - offset_from_top].to_value()
    }

    pub fn pop_int(&mut self) -> Result<i32, RuntimeError> {
        match self.pop()? {
            Value::Int(v) => Ok(v),
            Value::Long(v) => Ok(v as i32),
            // Coerce a null reference to zero.  Null is the only safe
            // conversion — an actual object pointer must NOT be treated as
            // an int, since downstream code often branches on bitwise
            // masks that would alias legitimate pointers and subsequently
            // dereference the resulting "int" back as an object.
            Value::Object(None) => Ok(0),
            // Uninitialized slot reads default-zero — primitive fields whose
            // <clinit> hasn't yet run, or local-variable slots before their
            // first write.  Same K1-family fix as Object(None).
            Value::Uninitialized => Ok(0),
            other => Err(RuntimeError::NotImplemented {
                feature: format!("expected int on stack, got {other}"),
            }),
        }
    }

    pub fn pop_long(&mut self) -> Result<i64, RuntimeError> {
        if self.len == 0 {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack underflow".to_string(),
            });
        }
        // For a Long slot the raw 64-bit pattern IS the i64 value (CompactValue
        // stores longs untagged).  Inspect the slot directly and widen ints
        // that the bytecode may have left where a long was expected.
        let idx = self.len - 1;
        let cv = self.slots[idx];
        match cv.tag() {
            // KC26 K1: Long values stored via `CompactValue::long(v)` are raw
            // i64 bits.  For "normal" values the bit pattern is untagged, so
            // `tag()` returns `Double`.  But for values whose high bits happen
            // to collide with the NaN-tag pattern (e.g. `-1_i64`, `i64::MIN`),
            // `tag()` returns `Long` (SUB_LONG_LO/HI). Both cases are simply
            // raw i64 storage — reinterpret the bits.
            CompactTag::Double | CompactTag::Long => {
                self.len -= 1;
                Ok(cv.as_long_unchecked())
            }
            CompactTag::Int => {
                self.len -= 1;
                // Sign-extend int → long.
                Ok(cv.as_int().unwrap_or(0) as i64)
            }
            _ => {
                // Slow path: defer to the full pop() + value coercion.
                // Capture the raw slot before decode so the diagnostic sees
                // the exact bit pattern the interpreter produced.
                let raw_cv = cv;
                let v = self.pop()?;
                match v {
                    Value::Long(l) => Ok(l),
                    Value::Int(i) => Ok(i as i64),
                    // K1-family null/uninit-coercion: a primitive long field
                    // whose default-zero hasn't been written produces null or
                    // Uninitialized at slot read.  Standard JDK semantics is
                    // to read 0L from such slots.
                    Value::Object(None) => Ok(0),
                    Value::Uninitialized => Ok(0),
                    other => {
                        log_tag_mismatch("long", raw_cv, self.len + 1);
                        Err(RuntimeError::NotImplemented {
                            feature: format!("expected long on stack, got {other}"),
                        })
                    }
                }
            }
        }
    }

    pub fn pop_float(&mut self) -> Result<f32, RuntimeError> {
        match self.pop()? {
            Value::Float(v) => Ok(v),
            Value::Int(v) => Ok(v as f32),
            other => Err(RuntimeError::NotImplemented {
                feature: format!("expected float on stack, got {other}"),
            }),
        }
    }

    pub fn pop_double(&mut self) -> Result<f64, RuntimeError> {
        if self.len == 0 {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack underflow".to_string(),
            });
        }
        // Untagged slots ARE doubles (raw f64 bits).  Handle the common case
        // without going through the full Value decode.
        let idx = self.len - 1;
        let cv = self.slots[idx];
        match cv.tag() {
            // KC26 K1: mirror pop_long's handling — Long-tagged values
            // (SUB_LONG_LO/HI) also store raw bits untagged; they arise
            // when a Long's bit pattern happens to collide with the NaN-tag
            // mask.  For dstore / d-arithmetic paths that coerce Long→Double,
            // reinterpret the bits.
            CompactTag::Double => {
                self.len -= 1;
                // as_double returns None only for NaN-tagged slots, which we've
                // excluded; but be defensive in case of edge cases.
                Ok(cv.as_double().unwrap_or(f64::from_bits(cv.to_bits())))
            }
            CompactTag::Long => {
                // A raw i64 value landed here (e.g. via `CompactValue::long`
                // producing a bit-pattern collision). Coerce to double the
                // same way a Value::Long would via the slow-path match.
                self.len -= 1;
                Ok(cv.as_long_unchecked() as f64)
            }
            _ => {
                let v = self.pop()?;
                match v {
                    Value::Double(d) => Ok(d),
                    Value::Float(f) => Ok(f as f64),
                    Value::Int(i) => Ok(i as f64),
                    Value::Long(l) => Ok(l as f64),
                    other => Err(RuntimeError::NotImplemented {
                        feature: format!("expected double on stack, got {other}"),
                    }),
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    // ── GC scanning and pointer update helpers ─────────────────────────

    /// Collect all non-null object references from the stack for GC root scanning.
    ///
    /// Per JVM spec, an operand-stack `Long` slot IS a primitive long, never a heap
    /// reference, so it is intentionally NOT rooted here. Smuggling references through
    /// long slots (the JNI `jobject`-as-`jlong` contract used by some natives / JIT
    /// glue) is handled at those specific sites, not in the generic GC root scan —
    /// treating arbitrary primitive longs as roots can SEGV when their bits happen to
    /// look like an aligned heap pointer (see `applogs/letsgo-segv-L1-diagnosis.md`).
    ///
    /// Scanned slot kinds:
    /// - NaN-boxed object slots (`CompactValue::is_object`).
    /// - Untagged raw slots ([`CompactTag::Double`] in the compact encoding — ambiguous
    ///   JVM long vs double): only values that pass `heap.is_object_address` are rooted,
    ///   so numeric longs and ordinary doubles are not mistaken for references.
    pub fn scan_object_refs(&self, roots: &mut Vec<ObjectRef>, heap: &VmHeap) {
        for i in 0..self.len {
            let cv = self.slots[i];
            if cv.is_object() {
                if let Some(ptr) = cv.as_object_ptr() {
                    if ptr != 0 {
                        // SAFETY: ptr was stored by CompactValue::object from a
                        // valid ObjectRef.
                        roots.push(unsafe { ObjectRef::from_raw(ptr as *mut u8) });
                    }
                }
            } else if matches!(cv.tag(), CompactTag::Long | CompactTag::Double) {
                // O1 hybrid: tagged Long OR untagged Double bits — both
                // could be smuggled jobject refs (JNI long-as-jobject pattern
                // used by WildFly's jboss-modules bootloader). Validate via
                // is_heap_addr (loose arena+alignment check) which doesn't
                // require a parseable object header (some refs are at
                // interior offsets or mid-init objects). The generational
                // collector's MAX_SANE_OBJECT_SIZE guard in forward_object
                // handles bogus roots gracefully — over-retention is the only
                // cost, vs. WildFly SEGV with the strict is_object_address.
                if let Some(p) = jlong_bits_as_aligned_object_ptr(cv.to_bits()) {
                    if let Some(r) = heap.is_heap_addr(p) {
                        roots.push(r);
                    }
                }
            }
        }
    }

    /// Update object references after GC using the pointer map.
    ///
    /// Symmetric to [`Self::scan_object_refs`]: only tagged object slots are
    /// remapped. Primitive `Long` slots are never treated as references — the
    /// matching restriction in `scan_object_refs` means a primitive long bit
    /// pattern would never have been rooted, so the GC pointer_map cannot legitimately
    /// contain an entry for it. Untagged `Double` slots that happen to encode a
    /// jlong-shaped pointer were rooted (filtered through `heap.is_object_address`)
    /// and ARE remapped here so the post-GC slot points at the moved object.
    pub fn update_object_refs(&mut self, pointer_map: &HashMap<usize, usize>) {
        for i in 0..self.len {
            let cv = self.slots[i];
            if cv.is_object() {
                if let Some(old_ptr) = cv.as_object_ptr() {
                    if let Some(&new_addr) = pointer_map.get(&(old_ptr as usize)) {
                        self.slots[i].update_object_ptr(new_addr as u64);
                    }
                }
            } else if cv.tag() == CompactTag::Double {
                let bits = cv.to_bits();
                if let Some(old_ptr) = jlong_bits_as_aligned_object_ptr(bits) {
                    if let Some(&new_addr) = pointer_map.get(&old_ptr) {
                        // Preserve the Double tag (untagged raw bits) so the slot's
                        // type doesn't change across GC — interpreter dispatch on
                        // this slot expects the same tag it had pre-GC.
                        self.slots[i] = CompactValue::from_bits(new_addr as u64);
                    }
                }
            }
        }
    }

    // ── Snapshot/restore for continuation freeze/thaw ───────────────────

    /// Snapshot the active portion of the stack (vals + tags) for continuation freeze.
    /// Returns only the `len` live entries, not the full pre-allocated buffer.
    ///
    /// Tags are reconstructed from CompactValue tags so the FrozenFrame
    /// wire format (Vec<u64> + Vec<u8>) is preserved for continuation thaw.
    pub fn snapshot_raw(&self) -> (Vec<u64>, Vec<u8>) {
        let vals: Vec<u64> = self.slots[..self.len].iter().map(|cv| cv.to_bits()).collect();
        let tags: Vec<u8> = self.slots[..self.len]
            .iter()
            .map(|cv| compact_tag_to_vtag(*cv))
            .collect();
        (vals, tags)
    }

    /// Restore a ValueStack from a raw snapshot (produced by `snapshot_raw`).
    /// `max_size` is the original max_stack from the Code attribute.
    ///
    /// The (vals, tags) pair mirrors the legacy SoA wire format; each slot is
    /// re-encoded into its NaN-boxed form using the provided tag to
    /// disambiguate Long vs Double.
    pub fn from_snapshot(vals: Vec<u64>, tags: Vec<u8>, max_size: usize) -> Self {
        let len = vals.len();
        let mut slots: Vec<CompactValue> = Vec::with_capacity(max_size);
        // If the tag vec is shorter (e.g. empty from into_inner), treat
        // missing entries as double so the raw u64 is preserved verbatim.
        for i in 0..len {
            let v = vals[i];
            let t = tags.get(i).copied().unwrap_or(crate::types::VTAG_DOUBLE);
            slots.push(vtag_to_compact(v, t));
        }
        // Extend to max_size so push operations have space.
        slots.resize(max_size.max(len), CompactValue::zero());
        Self {
            slots,
            len,
            max_size: max_size.max(len),
        }
    }

    // ── Backward-compatible Value-based accessors for cold paths ────────

    /// Get a decoded Value at index (cold path — for type-checked operations).
    pub fn get_value(&self, index: usize) -> Value {
        if index >= self.len {
            return Value::Uninitialized;
        }
        self.slots[index].to_value()
    }

    /// Get a raw `CompactValue` at an index (cold path — for GC, freeze).
    #[inline(always)]
    pub fn get_compact(&self, index: usize) -> Option<CompactValue> {
        if index >= self.len {
            None
        } else {
            Some(self.slots[index])
        }
    }
}

// ---------------------------------------------------------------------------
// Layout-transmute helpers (safe under CompactValue's repr(transparent)).
// ---------------------------------------------------------------------------

/// Reinterpret a `Vec<u64>` as `Vec<CompactValue>` without reallocation.
///
/// Safe because `CompactValue` is `#[repr(transparent)]` over `u64`, giving
/// identical size and alignment — a requirement for `Vec` transmutation as
/// documented in the Rust reference (nomicon §5.1).
#[inline(always)]
fn u64_vec_to_compact(v: Vec<u64>) -> Vec<CompactValue> {
    // SAFETY: CompactValue is repr(transparent) over u64, so the memory
    // layout is identical.  Vec's invariants (length, capacity, allocator)
    // are preserved.
    let mut v = std::mem::ManuallyDrop::new(v);
    let len = v.len();
    let cap = v.capacity();
    let ptr = v.as_mut_ptr() as *mut CompactValue;
    unsafe { Vec::from_raw_parts(ptr, len, cap) }
}

/// Inverse of [`u64_vec_to_compact`].
#[inline(always)]
fn compact_vec_to_u64(v: Vec<CompactValue>) -> Vec<u64> {
    let mut v = std::mem::ManuallyDrop::new(v);
    let len = v.len();
    let cap = v.capacity();
    let ptr = v.as_mut_ptr() as *mut u64;
    unsafe { Vec::from_raw_parts(ptr, len, cap) }
}

// ---------------------------------------------------------------------------
// Legacy VTAG <-> CompactValue conversion (for continuation snapshot/restore).
// ---------------------------------------------------------------------------

#[inline(always)]
fn compact_tag_to_vtag(cv: CompactValue) -> u8 {
    use crate::types::{
        VTAG_DOUBLE, VTAG_FLOAT, VTAG_INT, VTAG_LONG, VTAG_NULL, VTAG_OBJECT, VTAG_RETADDR,
        VTAG_UNINIT,
    };
    match cv.tag() {
        CompactTag::Int => VTAG_INT,
        CompactTag::Long => VTAG_LONG,
        CompactTag::Float => VTAG_FLOAT,
        CompactTag::Double => VTAG_DOUBLE,
        CompactTag::Object => VTAG_OBJECT,
        CompactTag::Null => VTAG_NULL,
        CompactTag::Uninitialized => VTAG_UNINIT,
        CompactTag::ReturnAddress => VTAG_RETADDR,
    }
}

#[inline(always)]
fn vtag_to_compact(val: u64, tag: u8) -> CompactValue {
    use crate::types::{
        VTAG_DOUBLE, VTAG_FLOAT, VTAG_INT, VTAG_LONG, VTAG_NULL, VTAG_OBJECT, VTAG_RETADDR,
        VTAG_UNINIT,
    };
    match tag {
        VTAG_INT => CompactValue::int(val as i32),
        VTAG_LONG => CompactValue::long(val as i64),
        VTAG_FLOAT => CompactValue::float(f32::from_bits(val as u32)),
        VTAG_DOUBLE => CompactValue::from_bits(val),
        VTAG_OBJECT => {
            if val == 0 || (val as usize) % 8 != 0 {
                CompactValue::null()
            } else {
                CompactValue::object(val)
            }
        }
        VTAG_NULL => CompactValue::null(),
        VTAG_UNINIT => CompactValue::uninitialized(),
        VTAG_RETADDR => CompactValue::return_address(val as u32),
        _ => CompactValue::uninitialized(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_pop() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(42)).unwrap();
        stack.push(Value::Int(99)).unwrap();
        assert_eq!(stack.pop_int().unwrap(), 99);
        assert_eq!(stack.pop_int().unwrap(), 42);
    }

    #[test]
    fn pop_empty_fails() {
        let mut stack = ValueStack::new(10);
        assert!(stack.pop().is_err());
    }

    #[test]
    fn overflow_fails() {
        let mut stack = ValueStack::new(2);
        stack.push(Value::Int(1)).unwrap();
        stack.push(Value::Int(2)).unwrap();
        assert!(stack.push(Value::Int(3)).is_err());
    }

    #[test]
    fn soa_roundtrip_all_types() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(42)).unwrap();
        stack.push(Value::Long(999999999999)).unwrap();
        stack.push(Value::Float(3.15)).unwrap();
        stack.push(Value::Double(2.719)).unwrap();
        stack.push(Value::Object(None)).unwrap();
        stack.push(Value::Uninitialized).unwrap();

        assert_eq!(stack.pop().unwrap(), Value::Uninitialized);
        assert!(stack.pop().unwrap().is_null());
        assert!((stack.pop_double().unwrap() - 2.719).abs() < 1e-9);
        assert!((stack.pop_float().unwrap() - 3.15).abs() < 1e-6);
        assert_eq!(stack.pop_long().unwrap(), 999999999999);
        assert_eq!(stack.pop_int().unwrap(), 42);
    }

    #[test]
    fn peek_works() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(42)).unwrap();
        stack.push(Value::Int(99)).unwrap();
        assert_eq!(stack.peek().as_int(), Some(99));
        assert_eq!(stack.peek_at(1).as_int(), Some(42));
    }

    #[test]
    #[should_panic(expected = "stack overflow")]
    fn push_unchecked_panics_on_overflow() {
        let mut stack = ValueStack::new(1);
        stack.push_unchecked(Value::Int(1));
        stack.push_unchecked(Value::Int(2)); // should panic
    }

    #[test]
    #[should_panic(expected = "stack underflow")]
    fn pop_unchecked_panics_on_underflow() {
        let mut stack = ValueStack::new(10);
        stack.pop_unchecked(); // should panic
    }

    #[test]
    #[should_panic(expected = "stack underflow")]
    fn peek_panics_on_empty() {
        let stack = ValueStack::new(10);
        let _ = stack.peek(); // should panic
    }

    #[test]
    #[should_panic(expected = "stack underflow")]
    fn peek_at_panics_out_of_range() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(1)).unwrap();
        let _ = stack.peek_at(5); // should panic
    }

    #[test]
    fn peek_checked_empty_returns_err() {
        let stack = ValueStack::new(10);
        assert!(stack.peek_checked().is_err());
    }

    #[test]
    fn get_value_out_of_range_returns_uninitialized() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(42)).unwrap();
        assert_eq!(stack.get_value(99), Value::Uninitialized);
    }

    #[test]
    fn clear_resets_stack() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(1)).unwrap();
        stack.push(Value::Int(2)).unwrap();
        assert_eq!(stack.len(), 2);
        stack.clear();
        assert_eq!(stack.len(), 0);
        assert!(stack.is_empty());
    }

    #[test]
    fn pop_wrong_type_returns_err() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Float(1.0)).unwrap();
        assert!(stack.pop_int().is_err());
    }

    #[test]
    fn pop_long_widens_int() {
        // Session 71 stack-widening: pop_long accepts Int by sign-extension
        // (compensates for native methods or bytecode sequences that leave
        // an int where a long is expected).
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(115)).unwrap();
        assert_eq!(stack.pop_long().unwrap(), 115i64);
    }

    #[test]
    fn pop_float_widens_int() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(7)).unwrap();
        assert_eq!(stack.pop_float().unwrap(), 7.0f32);
    }

    #[test]
    fn pop_double_widens_int() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(9)).unwrap();
        assert_eq!(stack.pop_double().unwrap(), 9.0f64);
    }

    #[test]
    fn from_pooled_creates_valid_stack() {
        let vals = Vec::new();
        let tags = Vec::new();
        let mut stack = ValueStack::from_pooled(vals, tags, 5);
        stack.push(Value::Int(42)).unwrap();
        assert_eq!(stack.pop_int().unwrap(), 42);
    }

    #[test]
    fn into_inner_returns_vecs() {
        let mut stack = ValueStack::new(5);
        stack.push(Value::Int(1)).unwrap();
        let (vals, _tags) = stack.into_inner();
        assert!(vals.len() >= 5);
        // tags are now stored inline — an empty Vec is returned for the
        // legacy pool signature.
    }

    // ── Additional edge case tests ────────────────────────────────────

    #[test]
    fn stack_size_zero() {
        let stack = ValueStack::new(0);
        assert!(stack.is_empty());
        assert_eq!(stack.len(), 0);
    }

    #[test]
    fn stack_size_zero_push_fails() {
        let mut stack = ValueStack::new(0);
        assert!(stack.push(Value::Int(1)).is_err());
    }

    #[test]
    fn stack_size_zero_pop_fails() {
        let mut stack = ValueStack::new(0);
        assert!(stack.pop().is_err());
    }

    #[test]
    fn stack_size_one_push_pop() {
        let mut stack = ValueStack::new(1);
        stack.push(Value::Int(77)).unwrap();
        assert_eq!(stack.len(), 1);
        assert!(!stack.is_empty());
        assert_eq!(stack.pop_int().unwrap(), 77);
        assert!(stack.is_empty());
    }

    #[test]
    fn stack_size_one_overflow() {
        let mut stack = ValueStack::new(1);
        stack.push(Value::Int(1)).unwrap();
        assert!(stack.push(Value::Int(2)).is_err());
    }

    #[test]
    #[should_panic(expected = "stack overflow")]
    fn push_unchecked_panics_on_full_size_zero() {
        let mut stack = ValueStack::new(0);
        stack.push_unchecked(Value::Int(1));
    }

    #[test]
    #[should_panic(expected = "stack underflow")]
    fn peek_at_panics_on_empty_stack() {
        let stack = ValueStack::new(5);
        let _ = stack.peek_at(0);
    }

    #[test]
    fn gc_update_object_refs_empty_map() {
        let mut stack = ValueStack::new(5);
        stack.push(Value::Int(42)).unwrap();
        let empty_map = HashMap::new();
        stack.update_object_refs(&empty_map);
        // Stack should be unchanged
        assert_eq!(stack.pop_int().unwrap(), 42);
    }

    #[test]
    fn from_pooled_with_undersized_vecs() {
        // Start with tiny vecs that are smaller than max_size
        let vals = vec![0u64; 1];
        let tags = vec![0u8; 1];
        let mut stack = ValueStack::from_pooled(vals, tags, 10);
        // Should be able to push up to 10 elements
        for i in 0..10 {
            stack.push(Value::Int(i)).unwrap();
        }
        assert_eq!(stack.len(), 10);
        assert!(stack.push(Value::Int(99)).is_err());
    }

    #[test]
    fn from_pooled_with_oversized_vecs() {
        // Start with vecs larger than max_size -- should resize down
        let vals = vec![0xFFu64; 100];
        let tags = vec![0xFFu8; 100];
        let mut stack = ValueStack::from_pooled(vals, tags, 3);
        stack.push(Value::Int(1)).unwrap();
        stack.push(Value::Int(2)).unwrap();
        stack.push(Value::Int(3)).unwrap();
        assert!(stack.push(Value::Int(4)).is_err());
    }

    #[test]
    fn into_inner_preserves_capacity() {
        let mut stack = ValueStack::new(8);
        stack.push(Value::Int(10)).unwrap();
        stack.push(Value::Long(20)).unwrap();
        let (vals, tags) = stack.into_inner();
        // Vecs should have at least max_size elements
        assert!(vals.len() >= 8);
        // Tags are no longer stored; an empty Vec is returned.
        assert!(tags.is_empty());
        // Reuse them
        let mut stack2 = ValueStack::from_pooled(vals, tags, 8);
        stack2.push(Value::Int(30)).unwrap();
        assert_eq!(stack2.pop_int().unwrap(), 30);
    }

    #[test]
    fn multiple_push_pop_cycles() {
        let mut stack = ValueStack::new(4);
        // Cycle 1
        stack.push(Value::Int(1)).unwrap();
        stack.push(Value::Int(2)).unwrap();
        assert_eq!(stack.pop_int().unwrap(), 2);
        assert_eq!(stack.pop_int().unwrap(), 1);
        assert!(stack.is_empty());

        // Cycle 2
        stack.push(Value::Long(100)).unwrap();
        stack.push(Value::Float(1.5)).unwrap();
        stack.push(Value::Double(2.5)).unwrap();
        assert!((stack.pop_double().unwrap() - 2.5).abs() < 1e-9);
        assert!((stack.pop_float().unwrap() - 1.5).abs() < 1e-6);
        assert_eq!(stack.pop_long().unwrap(), 100);
        assert!(stack.is_empty());

        // Cycle 3: fill to capacity
        for i in 0..4 {
            stack.push(Value::Int(i)).unwrap();
        }
        assert_eq!(stack.len(), 4);
        stack.clear();
        assert!(stack.is_empty());
    }

    #[test]
    fn stack_depth_tracking_accuracy() {
        let mut stack = ValueStack::new(10);
        assert_eq!(stack.len(), 0);

        stack.push(Value::Int(1)).unwrap();
        assert_eq!(stack.len(), 1);

        stack.push(Value::Int(2)).unwrap();
        assert_eq!(stack.len(), 2);

        let _ = stack.pop();
        assert_eq!(stack.len(), 1);

        stack.push(Value::Int(3)).unwrap();
        stack.push(Value::Int(4)).unwrap();
        assert_eq!(stack.len(), 3);

        stack.clear();
        assert_eq!(stack.len(), 0);

        // push_unchecked also tracks depth
        stack.push_unchecked(Value::Int(10));
        assert_eq!(stack.len(), 1);

        let _ = stack.pop_unchecked();
        assert_eq!(stack.len(), 0);
    }

    #[test]
    fn peek_at_all_positions() {
        let mut stack = ValueStack::new(5);
        stack.push(Value::Int(10)).unwrap();
        stack.push(Value::Int(20)).unwrap();
        stack.push(Value::Int(30)).unwrap();

        assert_eq!(stack.peek_at(0).as_int(), Some(30)); // top
        assert_eq!(stack.peek_at(1).as_int(), Some(20)); // second
        assert_eq!(stack.peek_at(2).as_int(), Some(10)); // bottom
    }

    #[test]
    fn get_value_within_bounds() {
        let mut stack = ValueStack::new(5);
        stack.push(Value::Int(10)).unwrap();
        stack.push(Value::Int(20)).unwrap();
        assert_eq!(stack.get_value(0).as_int(), Some(10));
        assert_eq!(stack.get_value(1).as_int(), Some(20));
    }

    #[test]
    fn snapshot_and_restore() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(1)).unwrap();
        stack.push(Value::Int(2)).unwrap();
        stack.push(Value::Int(3)).unwrap();
        assert_eq!(stack.len(), 3);

        let (vals, tags) = stack.snapshot_raw();
        assert_eq!(vals.len(), 3);
        assert_eq!(tags.len(), 3);

        let mut restored = ValueStack::from_snapshot(vals, tags, 10);
        assert_eq!(restored.len(), 3);
        assert_eq!(restored.pop_int().unwrap(), 3);
        assert_eq!(restored.pop_int().unwrap(), 2);
        assert_eq!(restored.pop_int().unwrap(), 1);
        assert!(restored.is_empty());

        // Can still push after restore (max_size preserved)
        restored.push(Value::Int(99)).unwrap();
        assert_eq!(restored.len(), 1);
    }

    #[test]
    fn snapshot_empty_stack() {
        let stack = ValueStack::new(5);
        let (vals, tags) = stack.snapshot_raw();
        assert!(vals.is_empty());
        assert!(tags.is_empty());

        let restored = ValueStack::from_snapshot(vals, tags, 5);
        assert!(restored.is_empty());
    }

    // ── T10.6 CompactValue-focused tests ───────────────────────────────

    #[test]
    fn t10_compact_value_stack_push_pop_roundtrip() {
        let mut stack = ValueStack::new(16);
        let cases = [
            Value::Int(i32::MIN),
            Value::Int(-1),
            Value::Int(0),
            Value::Int(i32::MAX),
            Value::Long(i64::MIN),
            Value::Long(0),
            Value::Long(i64::MAX),
            Value::Float(3.5),
            Value::Float(-0.0),
            Value::Double(std::f64::consts::PI),
            Value::Double(f64::INFINITY),
            Value::Object(None),
            Value::Uninitialized,
            Value::ReturnAddress(12345),
        ];
        for v in cases.iter().copied() {
            stack.push(v).unwrap();
        }
        // Pop in reverse, comparing each value.
        for v in cases.iter().rev() {
            let popped = stack.pop().unwrap();
            match (v, &popped) {
                // Long decodes as Double (untagged) so compare bit patterns
                // instead of enum variants.
                (Value::Long(l), Value::Double(d)) => {
                    assert_eq!(*l as u64, d.to_bits());
                }
                _ => assert_eq!(&popped, v, "roundtrip failed for {v}"),
            }
        }
    }

    #[test]
    fn t10_compact_value_stack_memory_footprint() {
        // CompactValue itself is 8 bytes.
        assert_eq!(std::mem::size_of::<CompactValue>(), 8);

        // Each slot in the stack's internal Vec is 8 bytes.
        let stack = ValueStack::new(4);
        assert_eq!(std::mem::size_of_val(&stack.slots[0]), 8);

        // The total underlying buffer is 8 bytes * slot_count.
        let byte_total = stack.slots.len() * std::mem::size_of::<CompactValue>();
        assert_eq!(byte_total, stack.slots.len() * 8);
    }

    #[test]
    fn t10_compact_value_reference_roundtrip() {
        let mut stack = ValueStack::new(4);
        // Use an 8-byte-aligned fake pointer.
        let fake_ptr = 0x1234_5678_ABC0_u64 as *mut u8;
        let obj = unsafe { ObjectRef::from_raw(fake_ptr) };
        stack.push(Value::Object(Some(obj))).unwrap();
        let v = stack.pop().unwrap();
        match v {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr() as u64, 0x1234_5678_ABC0),
            other => panic!("expected Object ref, got {other:?}"),
        }
    }

    #[test]
    fn t10_compact_value_dup_preserves_tag() {
        // Dup-2 semantics for category-2 values: two slots round-trip correctly.
        let mut stack = ValueStack::new(8);
        stack.push(Value::Long(0x0BAD_BEEF_DEAD_CAFE)).unwrap();
        stack.push(Value::Uninitialized).unwrap(); // high half of long
        // Simulate dup2: copy the two top slots.
        let high = stack.pop_compact();
        let low = stack.pop_compact();
        stack.push_compact(low);
        stack.push_compact(high);
        stack.push_compact(low);
        stack.push_compact(high);
        // Top of stack: [long_lo, long_hi, long_lo, long_hi]
        let _top_hi = stack.pop_compact(); // high pad
        let top_lo = stack.pop_compact();
        assert_eq!(top_lo.as_long_unchecked(), 0x0BAD_BEEF_DEAD_CAFE);
    }

    #[test]
    fn t10_compact_value_fibonacci_smoke() {
        // Fib(10) computed by push/pop sequences; verifies basic arithmetic
        // round-trips correctly through the CompactValue-backed stack.
        let mut stack = ValueStack::new(32);
        let mut a: i32 = 0;
        let mut b: i32 = 1;
        for _ in 0..10 {
            stack.push(Value::Int(a)).unwrap();
            stack.push(Value::Int(b)).unwrap();
            let vb = stack.pop_int().unwrap();
            let va = stack.pop_int().unwrap();
            let c = va.wrapping_add(vb);
            a = vb;
            b = c;
        }
        assert_eq!(a, 55);
    }

    #[test]
    fn t10_compact_value_push_compact_roundtrip() {
        let mut stack = ValueStack::new(4);
        let cv = CompactValue::int(42);
        stack.push_compact(cv);
        assert_eq!(stack.peek_compact().as_int(), Some(42));
        let popped = stack.pop_compact();
        assert_eq!(popped.as_int(), Some(42));
    }

    #[test]
    fn t10_gc_scan_object_refs_from_compact_stack() {
        let mut stack = ValueStack::new(4);
        let fake = 0x0000_1000_u64 as *mut u8;
        let obj = unsafe { ObjectRef::from_raw(fake) };
        stack.push(Value::Object(Some(obj))).unwrap();
        stack.push(Value::Int(99)).unwrap();
        stack.push(Value::Object(None)).unwrap();

        let mut roots = Vec::new();
        let heap = crate::memory::vm_heap::VmHeap::new(
            crate::memory::vm_heap::GcBackend::Generational,
            1024 * 1024,
        );
        stack.scan_object_refs(&mut roots, &heap);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].as_ptr() as u64, 0x1000);
    }

    // ── T10.9.D direct-push/pop helpers ────────────────────────────────

    #[test]
    fn push_int_roundtrip() {
        let mut stack = ValueStack::new(4);
        stack.push_int(42).unwrap();
        assert_eq!(stack.peek_compact().as_int(), Some(42));
        assert_eq!(stack.pop_int().unwrap(), 42);
    }

    #[test]
    fn push_long_roundtrip_negative() {
        let mut stack = ValueStack::new(4);
        stack.push_long(-i64::MAX).unwrap();
        // Untagged — read via unchecked long accessor.
        assert_eq!(stack.peek_compact().as_long_unchecked(), -i64::MAX);
        assert_eq!(stack.pop_long().unwrap(), -i64::MAX);
    }

    #[test]
    fn push_float_roundtrip() {
        let mut stack = ValueStack::new(4);
        stack.push_float(1.5).unwrap();
        assert_eq!(stack.peek_compact().as_float(), Some(1.5f32));
        assert!((stack.pop_float().unwrap() - 1.5f32).abs() < 1e-6);
    }

    #[test]
    fn push_double_roundtrip() {
        let mut stack = ValueStack::new(4);
        stack.push_double(std::f64::consts::PI).unwrap();
        assert_eq!(stack.peek_compact().as_double(), Some(std::f64::consts::PI));
        assert!((stack.pop_double().unwrap() - std::f64::consts::PI).abs() < 1e-12);
    }

    #[test]
    fn push_null_roundtrip() {
        let mut stack = ValueStack::new(4);
        stack.push_null().unwrap();
        assert!(stack.peek_compact().is_null());
        assert!(stack.pop().unwrap().is_null());
    }

    #[test]
    fn push_int_overflow_returns_err() {
        let mut stack = ValueStack::new(1);
        stack.push_int(1).unwrap();
        assert!(stack.push_int(2).is_err());
    }

    #[test]
    fn t10_gc_update_object_refs_rewrites_pointer() {
        let mut stack = ValueStack::new(4);
        let old_ptr = 0x0000_1000_u64;
        let new_ptr = 0x0000_2000_u64;
        let obj = unsafe { ObjectRef::from_raw(old_ptr as *mut u8) };
        stack.push(Value::Object(Some(obj))).unwrap();

        let mut map = HashMap::new();
        map.insert(old_ptr as usize, new_ptr as usize);
        stack.update_object_refs(&map);

        let popped = stack.pop().unwrap();
        match popped {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr() as u64, new_ptr),
            other => panic!("expected Object, got {other:?}"),
        }
    }

    /// Per JVM spec a `CompactTag::Long` operand slot IS a primitive long, never
    /// a reference. The scan must NOT root such slots — this was the SEGV trigger
    /// described in `applogs/letsgo-segv-L1-diagnosis.md`. To exercise the buggy
    /// path we need a slot whose `tag()` actually returns `Long`, i.e. a u64 whose
    /// high bits collide with the NaN-box marker AND whose 3-bit sub-tag is
    /// SUB_LONG_LO (110) or SUB_LONG_HI (111). `CompactValue::long` only produces
    /// such a tag for specific bit patterns; build one directly via `from_bits`.
    #[test]
    fn t10_gc_scan_skips_long_slot_even_if_bits_resemble_heap_pointer() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);

        // NANBOX_BITS (0xFFFC_0000_0000_0000) | SUB_LONG_LO (6 << 47)
        //   | aligned-pointer-shaped low 47 bits (0x10000).
        // tag() returns CompactTag::Long for this pattern.
        let long_bits: u64 = 0xFFFC_0000_0000_0000 | (6u64 << 47) | 0x1_0000;

        let mut stack = ValueStack::new(4);
        stack.push_compact(CompactValue::from_bits(long_bits));
        assert_eq!(stack.get_compact(0).unwrap().tag(), CompactTag::Long,
                   "test setup must produce a CompactTag::Long slot");

        let mut roots = Vec::new();
        stack.scan_object_refs(&mut roots, &heap);
        assert!(roots.is_empty(),
                "primitive Long slots must not be treated as GC roots");
    }

    /// Untagged raw bits (CompactTag::Double — the ambiguous JVM long/double
    /// slot in this encoding) that pass `heap.is_object_address` ARE rooted.
    /// This is the safe surviving path after the Long-arm fix.
    #[test]
    fn t10_gc_scan_roots_untagged_bits_when_heap_validates() {
        use crate::classloading::ClassId;
        use crate::memory::vm_heap::{GcBackend, VmHeap};

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as usize;

        let mut stack = ValueStack::new(4);
        // Untagged Double-encoded raw bits (the ambiguous slot kind).
        stack.push_compact(CompactValue::from_bits(addr as u64));

        let mut roots = Vec::new();
        stack.scan_object_refs(&mut roots, &heap);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].as_ptr() as usize, addr);
    }

    #[test]
    fn t10_gc_scan_skips_aligned_integer_long_not_in_heap() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let mut stack = ValueStack::new(2);
        stack.push_compact(CompactValue::long(8));

        let mut roots = Vec::new();
        stack.scan_object_refs(&mut roots, &heap);
        assert!(roots.is_empty());
    }

    /// `update_object_refs` must NOT touch `CompactTag::Long` slots — they are
    /// primitive longs, never references. A pointer_map collision against a
    /// primitive's bits would otherwise silently corrupt the long value.
    /// Uses a hand-rolled bit pattern (see scan-side test) to guarantee a
    /// `Long`-tagged slot.
    #[test]
    fn t10_gc_update_object_refs_preserves_long_slot() {
        let mut stack = ValueStack::new(4);
        // NANBOX_BITS | SUB_LONG_HI (7 << 47) | aligned low bits (0x1000).
        let long_bits: u64 = 0xFFFC_0000_0000_0000 | (7u64 << 47) | 0x1000;
        let new_ptr: usize = 0x2000;
        stack.push_compact(CompactValue::from_bits(long_bits));
        assert_eq!(stack.get_compact(0).unwrap().tag(), CompactTag::Long,
                   "test setup must produce a CompactTag::Long slot");

        let mut map = HashMap::new();
        // A malicious pointer_map entry keyed off the Long's low pointer-shaped
        // bits — the fix must NOT remap because Long slots are never roots.
        map.insert(0x1000_usize, new_ptr);
        // Also map the raw long_bits itself (defensive — neither key should fire).
        map.insert(long_bits as usize, new_ptr);
        stack.update_object_refs(&map);

        // Bits unchanged: primitive long is left strictly alone.
        assert_eq!(stack.get_compact(0).unwrap().to_bits(), long_bits);
    }

    /// Untagged raw `Double` slot carrying an aligned, mapped pointer IS
    /// remapped (these are the only ambiguous slots `scan_object_refs` rooted).
    #[test]
    fn t10_gc_update_object_refs_rewrites_untagged_pointer_shaped_slot() {
        let mut stack = ValueStack::new(4);
        let old_ptr: usize = 0x1000;
        let new_ptr: usize = 0x2000;
        stack.push_compact(CompactValue::from_bits(old_ptr as u64));

        let mut map = HashMap::new();
        map.insert(old_ptr, new_ptr);
        stack.update_object_refs(&map);

        // Slot now carries the remapped raw bits.
        let cv = stack.get_compact(0).expect("slot present");
        assert_eq!(cv.to_bits(), new_ptr as u64);
    }
}
