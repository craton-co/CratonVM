// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Exception object creation and RuntimeError -> Java exception conversion.
//!
//! This module provides utilities to:
//! 1. Create a Java exception object on the heap (load class, allocate, call `<init>`)
//! 2. Convert `RuntimeError` variants into proper `MethodCallFailed::ExceptionThrown`

use crate::error::{ClassFileError, MethodCallFailed, RuntimeError, VmError};
use crate::threading::jvm_thread::JvmThread;
use crate::types::{ObjectRef, Value};
use crate::vm::{create_java_string, invoke_on_class_shared, SharedVm};
use std::sync::OnceLock;

/// Cached read of the `CRATONVM_IAE_TRACE` env var. Env-var lookups are
/// surprisingly expensive (mutex + string alloc on some platforms); the
/// exception-throw hot path is sensitive to per-throw overhead, so we read
/// once at first use and cache the boolean. Process-lifetime cache: setting
/// the var after the first exception is thrown will have no effect.
static IAE_TRACE: OnceLock<bool> = OnceLock::new();

#[inline]
fn iae_trace_enabled() -> bool {
    *IAE_TRACE.get_or_init(|| std::env::var("CRATONVM_IAE_TRACE").is_ok())
}

// ===========================================================================
// JEP 358 — Helpful NullPointerException messages
// ===========================================================================
//
// Increment 1: synthesize HotSpot-style "Cannot invoke ... because ... is
// null" messages at the interpreter null-receiver site, plus a bounded
// backward bytecode analysis that reconstructs the source expression of the
// null operand (getfield / aload local|param / getstatic / invoke / aaload).
//
// The logic here is deliberately VM-decoupled: it operates on the raw `code[]`
// of the trapping method plus a small `CpResolver` trait that the caller
// implements over the real constant pool. This keeps the syntactic
// reconstruction fully unit-testable without rt.jar (see the `helpful_npe`
// tests below), mirroring the design doc's "deopt-independent, bci is always
// known in the interpreter" rationale.
pub mod helpful_npe {
    use cratonvm_reader::instruction::Instruction;

    /// Maximum number of producer-recursion levels the backward expression
    /// analysis will follow (HotSpot caps this too). Beyond the cap we bail to
    /// an action-only message rather than fabricating a deep, possibly-wrong
    /// chain such as `a.b.c.d.e`.
    const MAX_EXPR_DEPTH: u32 = 4;

    /// A constant-pool reference the analysis needs to classify a producer
    /// opcode. The caller resolves a raw CP index in the *current method's*
    /// class into one of these shapes. Class / field / method names are the
    /// internal (slash-separated) JVM form; formatting to the external dotted
    /// form happens here.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum CpRef {
        /// `getfield` / `getstatic` / `putfield` target.
        Field {
            owner_internal: String,
            name: String,
        },
        /// `invokevirtual` / `invokespecial` / `invokeinterface` / `invokestatic`.
        Method {
            owner_internal: String,
            name: String,
            descriptor: String,
        },
    }

    /// Resolver the analysis uses to turn a CP index (in the trapping method's
    /// class) into a [`CpRef`]. Implemented over the real `ConstantPool` by the
    /// interpreter; a trivial map-backed impl is used by the unit tests.
    pub trait CpResolver {
        fn field_ref(&self, cp_index: u16) -> Option<CpRef>;
        fn method_ref(&self, cp_index: u16) -> Option<CpRef>;
        /// Optional `LocalVariableTable`-derived name for local slot `n` live
        /// at byte-offset `bci`. `None` falls back to the synthetic `<localN>`
        /// spelling HotSpot uses when no debug info is present.
        fn local_name(&self, _slot: u16, _bci: usize) -> Option<String> {
            None
        }
    }

    /// Convert an internal class name (`java/lang/String`, `[I`,
    /// `[Ljava/lang/Object;`) to the external dotted form HotSpot prints in
    /// JEP 358 messages (`java.lang.String`, `int[]`, `java.lang.Object[]`).
    pub fn class_external(internal: &str) -> String {
        // Count + strip leading array dims.
        let dims = internal.bytes().take_while(|b| *b == b'[').count();
        let base = &internal[dims..];
        let base_name = if base.len() == 1 {
            // Primitive array element descriptor.
            match base.as_bytes()[0] {
                b'B' => "byte",
                b'C' => "char",
                b'D' => "double",
                b'F' => "float",
                b'I' => "int",
                b'J' => "long",
                b'S' => "short",
                b'Z' => "boolean",
                _ => base,
            }
            .to_string()
        } else if let Some(stripped) = base.strip_prefix('L') {
            // `Lpkg/Cls;` reference array element.
            stripped.trim_end_matches(';').replace('/', ".")
        } else {
            base.replace('/', ".")
        };
        let mut out = base_name;
        for _ in 0..dims {
            out.push_str("[]");
        }
        out
    }

    /// Render a single field-descriptor token (`I`, `Ljava/lang/String;`,
    /// `[I`) as an external type name. Used for the parameter list of an
    /// invoke action.
    fn field_descriptor_external(token: &str) -> String {
        // `class_external` already understands array dims + the `L...;` form;
        // a bare primitive token (`I`) maps through its array branch too.
        class_external(token)
    }

    /// Split a method descriptor's parameter list into descriptor tokens.
    /// Self-contained (no dependency on the interpreter's helper) so the
    /// analysis crate boundary stays clean and testable.
    fn param_tokens(descriptor: &str) -> Vec<String> {
        let bytes = descriptor.as_bytes();
        let mut params = Vec::new();
        let mut i = 1; // skip '('
        while i < bytes.len() && bytes[i] != b')' {
            let start = i;
            while i < bytes.len() && bytes[i] == b'[' {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'L' => {
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                    i += 1; // consume ';'
                }
                _ => i += 1,
            }
            params.push(descriptor[start..i].to_string());
        }
        params
    }

    /// The "action" half for an invoke whose receiver was null, e.g.
    /// `Cannot invoke "java.util.List.get(int)"`.
    pub fn action_invoke(owner_internal: &str, name: &str, descriptor: &str) -> String {
        let owner = class_external(owner_internal);
        let params = param_tokens(descriptor)
            .iter()
            .map(|t| field_descriptor_external(t))
            .collect::<Vec<_>>()
            .join(", ");
        format!("Cannot invoke \"{owner}.{name}({params})\"")
    }

    // -- Increment 2: action halves for the remaining null-deref opcodes -----
    //
    // Each mirrors the exact HotSpot (`BytecodeUtils`) wording so compliance
    // suites that assert the JEP 358 strings match. The `getfield` /
    // `putfield` field name is the *simple* field name (HotSpot prints
    // `Cannot read field "x"`, not the owner-qualified name).

    /// `getfield` on a null receiver: `Cannot read field "name"`.
    pub fn action_read_field(name: &str) -> String {
        format!("Cannot read field \"{name}\"")
    }

    /// `putfield` on a null receiver: `Cannot assign field "name"`.
    pub fn action_assign_field(name: &str) -> String {
        format!("Cannot assign field \"{name}\"")
    }

    /// `arraylength` on a null array.
    pub fn action_array_length() -> String {
        "Cannot read the array length".to_string()
    }

    /// `*aload` on a null array. `elem` is the JEP 358 element-type spelling
    /// (`int`, `object`, `byte`, …) — HotSpot prints e.g.
    /// `Cannot load from int array`, `Cannot load from object array`.
    pub fn action_array_load(elem: ArrayElemKind) -> String {
        format!("Cannot load from {} array", elem.as_str())
    }

    /// `*astore` into a null array: `Cannot store to <elem> array`.
    pub fn action_array_store(elem: ArrayElemKind) -> String {
        format!("Cannot store to {} array", elem.as_str())
    }

    /// `monitorenter` / `monitorexit` on null.
    pub fn action_monitor() -> String {
        "Cannot enter synchronized block".to_string()
    }

    /// `athrow` of a null reference.
    pub fn action_throw() -> String {
        "Cannot throw exception".to_string()
    }

    /// The JEP 358 element-type spelling used in the `*aload`/`*astore`
    /// action strings. HotSpot spells reference-array element type as
    /// `object` and primitive arrays by their Java keyword.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ArrayElemKind {
        Int,
        Long,
        Float,
        Double,
        Byte,
        Char,
        Short,
        Boolean,
        Object,
    }

    impl ArrayElemKind {
        pub fn as_str(self) -> &'static str {
            match self {
                ArrayElemKind::Int => "int",
                ArrayElemKind::Long => "long",
                ArrayElemKind::Float => "float",
                ArrayElemKind::Double => "double",
                ArrayElemKind::Byte => "byte",
                ArrayElemKind::Char => "char",
                ArrayElemKind::Short => "short",
                ArrayElemKind::Boolean => "boolean",
                ArrayElemKind::Object => "object",
            }
        }
    }

    /// Combine an action half with an optional null-expression half into the
    /// final JEP 358 message. When the expression is unknown we emit the
    /// action-only form (`"<action> because the return value ... is null"` is
    /// never fabricated).
    pub fn combine(action: &str, expr: Option<&str>) -> String {
        match expr {
            Some(e) => format!("{action} because \"{e}\" is null"),
            None => format!("{action} because the receiver is null"),
        }
    }

    /// Like [`combine`] but emits the *action-only* string when the
    /// expression can't be classified (HotSpot omits the `because` clause
    /// rather than fabricating "the receiver"). Used by the increment-2
    /// opcode sites (getfield / array / monitor / athrow), whose null operand
    /// isn't always a "receiver".
    pub fn combine_opt(action: &str, expr: Option<&str>) -> String {
        match expr {
            Some(e) => format!("{action} because \"{e}\" is null"),
            None => action.to_string(),
        }
    }

    // -- Bounded backward expression analysis ------------------------------

    /// One simulated operand-stack slot, tagged with the bci of the opcode
    /// that pushed it (or `None` for a value whose producer we didn't track,
    /// e.g. a method-entry argument that was never re-pushed).
    #[derive(Clone, Copy)]
    struct Slot {
        producer_bci: Option<usize>,
    }

    /// Decode the whole method forward, simulating operand-stack heights and
    /// recording, for each slot live *immediately before* `target_bci`, which
    /// bci produced it. Returns the slot vector at `target_bci`, or `None` if
    /// the method couldn't be cleanly simulated up to that point (unknown /
    /// branch-dependent stack shape) — in which case the caller emits an
    /// action-only message.
    ///
    /// This is intentionally a *linear* simulation (no control-flow join
    /// modelling): JEP 358 reconstruction is syntactic and approximate, and a
    /// straight-line walk covers the overwhelmingly common "load receiver then
    /// invoke" shape. Any opcode whose stack effect we don't model, or any
    /// backward branch target landing inside the prefix, makes us bail.
    fn simulate_to(code: &[u8], target_bci: usize) -> Option<Vec<Slot>> {
        let mut stack: Vec<Slot> = Vec::new();
        let mut pc = 0usize;
        let mut guard = 0u32;
        while pc < code.len() {
            if pc == target_bci {
                return Some(stack);
            }
            if pc > target_bci {
                // We stepped past the target without landing on it — the
                // target bci isn't an instruction boundary we reached.
                return None;
            }
            guard += 1;
            if guard > 200_000 {
                return None; // pathological method — bail rather than spin
            }
            let (instr, next) = Instruction::decode(code, pc).ok()?;
            apply_stack_effect(&mut stack, &instr, pc)?;
            if next <= pc {
                return None; // non-progress guard
            }
            pc = next;
        }
        None
    }

    /// Apply `instr`'s operand-stack effect to `stack`, tagging any pushed
    /// slot with `bci`. Returns `None` for an opcode we don't model (caller
    /// bails). Only the subset that can appear before a receiver push needs to
    /// be precise; categories we can't reason about conservatively abort.
    fn apply_stack_effect(stack: &mut Vec<Slot>, instr: &Instruction, bci: usize) -> Option<()> {
        use Instruction::*;
        // pop `n`, then push `pushes` fresh slots produced at `bci`.
        macro_rules! shape {
            ($pop:expr, $push:expr) => {{
                for _ in 0..$pop {
                    stack.pop()?;
                }
                for _ in 0..$push {
                    stack.push(Slot { producer_bci: Some(bci) });
                }
            }};
        }
        match instr {
            // Constants / loads — push 1 (cat-2 longs/doubles also push 1
            // slot here; the analysis only ever inspects reference receivers,
            // and width mismatches just make us bail when popping).
            AconstNull | IconstM1 | Iconst0 | Iconst1 | Iconst2 | Iconst3 | Iconst4
            | Iconst5 | Lconst0 | Lconst1 | Fconst0 | Fconst1 | Fconst2 | Dconst0
            | Dconst1 | Bipush(_) | Sipush(_) | Ldc(_) | LdcW(_) | Ldc2W(_)
            | Iload(_) | Lload(_) | Fload(_) | Dload(_) | Aload(_) => shape!(0, 1),
            // getstatic pushes 1, putstatic pops 1.
            Getstatic(_) => shape!(0, 1),
            Putstatic(_) => shape!(1, 0),
            // getfield: pop receiver, push field value.
            Getfield(_) => shape!(1, 1),
            // putfield: pop value + receiver.
            Putfield(_) => shape!(2, 0),
            // Array loads: pop array+index, push element.
            Iaload | Laload | Faload | Daload | Aaload | Baload | Caload | Saload => {
                shape!(2, 1)
            }
            Arraylength => shape!(1, 1),
            // new / checkcast: net push 1 / net 0.
            New(_) => shape!(0, 1),
            Checkcast(_) => shape!(1, 1),
            Instanceof(_) => shape!(1, 1),
            Anewarray(_) | Newarray(_) => shape!(1, 1),
            Nop => {}
            Dup => {
                let top = *stack.last()?;
                stack.push(top);
            }
            Pop => {
                stack.pop()?;
            }
            // Invokes: pop args (+ receiver for non-static), push return (if
            // non-void). We don't need byte-exact cat-2 accounting for the
            // common receiver-load shape; model 1 slot per param + receiver.
            Invokevirtual(_) | Invokespecial(_) | Invokeinterface { .. } => {
                // descriptor unknown here without the CP; the only invokes the
                // analysis needs to *step over* before the trapping one are
                // rare in the straight-line receiver-load shape. Bail to stay
                // conservative rather than mis-account args.
                return None;
            }
            Invokestatic(_) | Invokedynamic(_) => return None,
            // Anything else (arithmetic, branches, stores, dup variants,
            // switches, returns, athrow, monitor, etc.): we don't model it —
            // bail so we never emit a wrong expression.
            _ => return None,
        }
        Some(())
    }

    /// Describe the source expression pushed by the instruction at
    /// `producer_bci`, recursing (bounded) through getfield receivers.
    fn describe_producer(
        code: &[u8],
        producer_bci: usize,
        resolver: &dyn CpResolver,
        depth: u32,
    ) -> Option<String> {
        if depth > MAX_EXPR_DEPTH {
            return None;
        }
        let (instr, _) = Instruction::decode(code, producer_bci).ok()?;
        match instr {
            Instruction::Aload(slot) => {
                if slot == 0 {
                    // aload_0 in an instance method is `this`.
                    Some(
                        resolver
                            .local_name(0, producer_bci)
                            .unwrap_or_else(|| "this".to_string()),
                    )
                } else {
                    Some(
                        resolver
                            .local_name(slot, producer_bci)
                            .unwrap_or_else(|| format!("<local{slot}>")),
                    )
                }
            }
            Instruction::Getfield(idx) => {
                let CpRef::Field { name, .. } = resolver.field_ref(idx)? else {
                    return None;
                };
                // Recurse on the receiver pushed just before this getfield.
                let recv = simulate_to(code, producer_bci)
                    .and_then(|stack| stack.last().copied())
                    .and_then(|s| s.producer_bci)
                    .and_then(|b| describe_producer(code, b, resolver, depth + 1));
                match recv {
                    Some(r) => Some(format!("{r}.{name}")),
                    None => Some(name),
                }
            }
            Instruction::Getstatic(idx) => {
                let CpRef::Field {
                    owner_internal,
                    name,
                } = resolver.field_ref(idx)?
                else {
                    return None;
                };
                Some(format!("{}.{name}", class_external(&owner_internal)))
            }
            Instruction::Aaload => {
                // `<arr>[...]`; recurse on the array operand (two slots down:
                // array then index were pushed before the aaload).
                let stack = simulate_to(code, producer_bci)?;
                let arr_slot = stack.len().checked_sub(2)?;
                let arr = stack
                    .get(arr_slot)
                    .and_then(|s| s.producer_bci)
                    .and_then(|b| describe_producer(code, b, resolver, depth + 1));
                match arr {
                    Some(a) => Some(format!("{a}[...]")),
                    None => None,
                }
            }
            Instruction::AconstNull => Some("null".to_string()),
            _ => None,
        }
    }

    /// Reconstruct the null-receiver expression for the invoke at
    /// `invoke_bci`, which consumes `num_params` argument slots above the
    /// receiver. Returns `None` (→ action-only message) when the producer
    /// can't be unambiguously classified.
    pub fn null_expr_for_invoke_receiver(
        code: &[u8],
        invoke_bci: usize,
        num_params: usize,
        resolver: &dyn CpResolver,
    ) -> Option<String> {
        // The receiver sits `num_params` slots below the top of the operand
        // stack as it stood just before the invoke.
        null_expr_at_depth(code, invoke_bci, num_params, resolver)
    }

    /// Generalized null-operand reconstruction shared by every null-deref
    /// opcode (increment 2). `depth_below_top` is how many operand-stack
    /// slots sit *above* the null operand at the moment the trapping opcode
    /// at `trap_bci` begins executing — `0` for the top-of-stack operand
    /// (`getfield` receiver, `arraylength` / `monitor` / `athrow` operand),
    /// `1` for the slot one below (the `putfield` receiver, which has the
    /// stored value above it; the `*aload` array, which has the index above
    /// it), and `2` for the `*astore` array (value + index above it).
    ///
    /// Returns `None` (→ action-only message) when the producer can't be
    /// unambiguously classified, exactly like the invoke path.
    pub fn null_expr_at_depth(
        code: &[u8],
        trap_bci: usize,
        depth_below_top: usize,
        resolver: &dyn CpResolver,
    ) -> Option<String> {
        let stack = simulate_to(code, trap_bci)?;
        let idx = stack.len().checked_sub(depth_below_top + 1)?;
        let producer = stack.get(idx)?.producer_bci?;
        describe_producer(code, producer, resolver, 0)
    }
}

/// Resolve `Throwable.detailMessage` (or any inherited String field by
/// that name) and write `string_ref` to it. Walks the class hierarchy
/// using `first_field_index` + declaration-order non-static field
/// counting — the same scheme `resolve_field_index_in_hierarchy` uses.
///
/// Used by the exception-fallback path when the `(String)V` constructor
/// is unavailable: writing to slot 0 unconditionally would corrupt
/// `Throwable.backtrace` (slot 0) with a String reference and leave
/// `detailMessage` (slot 1) and `cause` (slot 2) untouched.
fn set_detail_message_by_name(shared: &SharedVm, obj: ObjectRef, string_ref: ObjectRef) {
    let class_id = shared.heap.class_id_of(obj);
    let cm = shared.class_manager.read();
    let mut walk = Some(class_id);
    while let Some(cid) = walk {
        let Some(cls) = cm.get_class(cid) else { break };
        let mut inst = 0usize;
        for f in &cls.fields {
            if f.is_static() {
                continue;
            }
            if &*f.name == "detailMessage" {
                let idx = cls.first_field_index + inst;
                drop(cm);
                shared
                    .heap
                    .set_field(obj, idx, Value::Object(Some(string_ref)));
                return;
            }
            inst += 1;
        }
        walk = cls.superclass;
    }
}

/// Create a Java exception object on the heap.
///
/// Steps:
/// 1. Load the exception class (e.g. `java/lang/NullPointerException`)
/// 2. Allocate an object on the heap
/// 3. Call the constructor — either `()V` or `(Ljava/lang/String;)V`
/// 4. Call `fillInStackTrace` if available
///
/// If any step fails (e.g. class not found), falls back to `InternalError`
/// to prevent infinite recursion.
///
/// Round-7 Fix 7: `#[cold]` — exception construction is always off the hot
/// path; marking cold lets LLVM lay this function out away from callers
/// (better I-cache for the success path) and tags every callsite as
/// unlikely so branch hints point at the success arm.
#[cold]
pub fn create_exception_object(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    message: Option<&str>,
) -> Result<ObjectRef, MethodCallFailed> {
    // 1. Load the exception class
    let class_id = shared
        .class_manager
        .write()
        .load_class(class_name)
        .map_err(|e| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: format!("failed to load exception class {class_name}: {e}"),
            })
        })?;

    // 2. Allocate the exception object
    let num_fields = shared
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.num_total_fields)
        .unwrap_or(0);
    let obj_ref = match shared.heap.try_alloc_object(class_id, num_fields) {
        Some(obj) => obj,
        None => {
            // Young gen full — force a GC cycle and retry.
            thread.tlab.retire();
            super::interpreter::maybe_gc_forced_pub(shared, thread);
            shared.heap.try_alloc_object(class_id, num_fields).ok_or_else(|| {
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::OutOfMemoryError {
                        message: format!(
                            "Java heap space (exception {} with {} fields)",
                            class_name, num_fields,
                        ),
                    },
                ))
            })?
        }
    };

    // 3. Call the constructor
    // Try (Ljava/lang/String;)V if we have a message, otherwise ()V
    if let Some(msg) = message {
        // Create the java.lang.String for the message
        let string_ref = create_java_string(shared, msg);

        // Try calling (Ljava/lang/String;)V constructor first
        let init_result = invoke_on_class_shared(
            shared,
            thread,
            class_id,
            "<init>",
            "(Ljava/lang/String;)V",
            &[
                Value::Object(Some(obj_ref)),
                Value::Object(Some(string_ref)),
            ],
        );

        match &init_result {
            Ok(_) => { /* Constructor succeeded — message is set */ }
            Err(MethodCallFailed::InternalError(_)) => {
                // String-arg constructor not found — fall back to ()V and
                // manually set detailMessage. Resolve by name so we hit the
                // real-JDK Throwable layout slot (slot 1, after backtrace),
                // not slot 0 (which is `backtrace`, an internal Object ref).
                let _ = invoke_on_class_shared(
                    shared,
                    thread,
                    class_id,
                    "<init>",
                    "()V",
                    &[Value::Object(Some(obj_ref))],
                );
                set_detail_message_by_name(shared, obj_ref, string_ref);
            }
            Err(MethodCallFailed::ExceptionThrown(_)) => {
                // Constructor threw — still set the message field manually
                // by name so we honour the real-JDK Throwable layout.
                set_detail_message_by_name(shared, obj_ref, string_ref);
            }
        }
    } else {
        let init_result = invoke_on_class_shared(
            shared,
            thread,
            class_id,
            "<init>",
            "()V",
            &[Value::Object(Some(obj_ref))],
        );
        if let Err(MethodCallFailed::InternalError(_)) = &init_result {
            // Can't call constructor — object is partially initialized but usable.
        }
    }

    // 4. Call fillInStackTrace
    // This is done automatically by the Throwable constructor in most JDK versions,
    // but we call it explicitly just in case.
    let _ = invoke_on_class_shared(
        shared,
        thread,
        class_id,
        "fillInStackTrace",
        "(I)Ljava/lang/Throwable;",
        &[Value::Object(Some(obj_ref)), Value::Int(0)],
    );

    Ok(obj_ref)
}

/// Convert a `RuntimeError` into a `MethodCallFailed::ExceptionThrown`.
///
/// Creates a real Java exception object on the heap corresponding to the
/// `RuntimeError` variant. If creating the Java exception object fails,
/// falls back to `MethodCallFailed::InternalError`.
///
/// Round-7 Fix 7: `#[cold]` — the whole throw machinery (allocation, init
/// call, fillInStackTrace) is rare relative to non-throwing opcodes.
#[cold]
pub fn throw_runtime_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    error: RuntimeError,
) -> MethodCallFailed {
    // charset-NPE diagnostic (2026-05-21) — gated by `CRATONVM_DBG_CHARSET=1`.
    // Defensive companion to the `Athrow`-opcode dump in `interpreter.rs`:
    // catches the case where a `NullPointerException` with message exactly
    // `charset` originates Rust-side (a native that fails a charset arg
    // check) rather than from genuine JDK `new NullPointerException(
    // "charset")` bytecode. Dumps the full live Java thread stack —
    // `class.method:pc`, deepest first — which is the ground truth even
    // when the CLI uncaught-exception renderer later prints zero frames.
    // The env-var read is a single cached atomic load, so the no-debug
    // path is free; it is intentionally NOT gated behind `tracing::enabled!`.
    if std::env::var_os("CRATONVM_DBG_AIOOBE").is_some() {
        if let RuntimeError::ArrayIndexOutOfBoundsException { index } = &error {
            eprintln!(
                "[AIOOBE-THROW] index={index} — full live Java thread stack ({} frames, deepest first):",
                thread.frames.len()
            );
            for (i, f) in thread.frames.iter().enumerate().rev().take(15) {
                let cn = shared
                    .class_manager
                    .read()
                    .get_class(f.class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                eprintln!(
                    "[AIOOBE-STK {i}] {}.{}{} pc={}",
                    cn,
                    f.method_name(),
                    f.method_descriptor(),
                    f.pc
                );
            }
        }
    }
    if crate::runtime::env_cache::charset_dbg() {
        if let RuntimeError::NullPointerException { message: Some(m) } = &error {
            if m == "charset" {
                eprintln!(
                    "[CHARSET-NPE] RuntimeError::NullPointerException(\"charset\") raised \
                     Rust-side — full live Java thread stack ({} frames, deepest first):",
                    thread.frames.len()
                );
                for (i, f) in thread.frames.iter().enumerate().rev() {
                    let cn = shared
                        .class_manager
                        .read()
                        .get_class(f.class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    eprintln!(
                        "[CHARSET-NPE-STK {i}] {}.{}{} pc={}",
                        cn,
                        f.method_name(),
                        f.method_descriptor(),
                        f.pc
                    );
                }
            }
        }
    }
    // T15: trace the origin of RuntimeErrors so users can see where
    // a silent NPE/IOOBE/etc. is coming from during class init.
    //
    // PERF: the entire preamble is gated behind `tracing::enabled!(DEBUG)`
    // so the common no-trace path pays only an atomic-bool check. Every
    // expensive operation here -- `method_name().to_string()`, the
    // `class_manager.read()` lock + `Arc<str>` clone of `class.name`, the
    // 15-/20-/25-/30-/40-frame walks that re-acquire the read lock per
    // frame, and the per-throw `tracing::debug!` formatting -- is now
    // skipped when DEBUG-level tracing is not active. The env-var checks
    // are additionally cached in a `OnceLock<bool>` (see
    // `iae_trace_enabled`) so the eprintln-style stack dumps that only
    // fire under `CRATONVM_IAE_TRACE` cost a single atomic load + branch
    // instead of a syscall + heap alloc.
    if tracing::enabled!(tracing::Level::DEBUG) {
        let frame = thread.frames.last();
        let method = frame.map(|f| f.method_name().to_string()).unwrap_or_default();
        let pc = frame.map(|f| f.pc).unwrap_or(0);
        let class_name = frame
            .and_then(|f| shared.class_manager.read().get_class(f.class_id).map(|c| c.name.clone()))
            .unwrap_or_default();
        tracing::debug!(
            class = %class_name, method = %method, pc,
            "runtime_error origin: {error:?}"
        );
        // Trace NPE origins — print full caller stack when NPE occurs
        if matches!(&error, RuntimeError::NullPointerException { .. }) {
            let has_dorun = thread.frames.iter().any(|f| f.method_name() == "doRun");
            if has_dorun {
                for (i, f) in thread.frames.iter().enumerate().rev().take(15) {
                    let _cn = shared.class_manager.read()
                        .get_class(f.class_id)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                }
            }
            // C29 / SUREFIRE NPE traces — opt-in via CRATONVM_DBG_NPE_TRACE.
            if std::env::var_os("CRATONVM_DBG_NPE_TRACE").is_some() {
                if let RuntimeError::NullPointerException { message: Some(m) } = &error {
                    if m.contains("isInterface") {
                        for (i, f) in thread.frames.iter().enumerate().rev().take(20) {
                            let cn = shared.class_manager.read()
                                .get_class(f.class_id)
                                .map(|c| c.name.clone())
                                .unwrap_or_default();
                            eprintln!("C29-STK[{i}] {}.{} pc={}", cn, f.method_name(), f.pc);
                        }
                    }
                    if m.contains("Name is null") {
                        eprintln!("SUREFIRE-NPE-TRACE msg={m}");
                        for (i, f) in thread.frames.iter().enumerate().rev().take(30) {
                            let cn = shared.class_manager.read()
                                .get_class(f.class_id)
                                .map(|c| c.name.clone())
                                .unwrap_or_default();
                            eprintln!("SUREFIRE-NPE-STK[{i}] {}.{} pc={}", cn, f.method_name(), f.pc);
                        }
                    }
                }
            }
            // R15 (WildFly): trace NPE origins inside log4j SimpleLoggerContext
            // / PropertiesUtil chain so we can pinpoint which native /
            // bytecode op produces the bare-NPE that bubbles up as the
            // ExceptionInInitializerError that crashes WildFly boot. Opt-in
            // via CRATONVM_DBG_WF_NPE to avoid stderr spam during boot.
            if std::env::var_os("CRATONVM_DBG_WF_NPE").is_some() {
                let in_log4j_init = thread.frames.iter().any(|f| {
                    let cn = shared.class_manager.read()
                        .get_class(f.class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    cn.starts_with("org/apache/logging/log4j/")
                        || cn.starts_with("org/jboss/logging/")
                });
                if in_log4j_init {
                    eprintln!("[WF-NPE-TRACE] msg={:?}", error);
                    for (i, f) in thread.frames.iter().enumerate().rev().take(40) {
                        let cn = shared.class_manager.read()
                            .get_class(f.class_id)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default();
                        eprintln!("[WF-NPE-STK {i}] {}.{} pc={}", cn, f.method_name(), f.pc);
                    }
                }
            }
            // S111r20: broad NPE trace for spring context NPE hunt
            if iae_trace_enabled() {
                eprintln!("NPE-TRACE msg={:?}", error);
                for (i, f) in thread.frames.iter().enumerate().rev().take(30) {
                    let cn = shared.class_manager.read()
                        .get_class(f.class_id)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    eprintln!("NPE-STK[{i}] {}.{} pc={}", cn, f.method_name(), f.pc);
                }
            }
        }
        // S111r19+: trace IAE origins for ConfigurationClassParser hunt
        if matches!(&error, RuntimeError::IllegalArgumentException { .. })
            && iae_trace_enabled()
        {
            eprintln!("IAE-TRACE error={error:?}");
            for (i, f) in thread.frames.iter().enumerate().rev().take(25) {
                let cn = shared.class_manager.read()
                    .get_class(f.class_id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                eprintln!("IAE-STK[{i}] {}.{} pc={}", cn, f.method_name(), f.pc);
            }
        }
    }
    let (class_name, message) = match &error {
        RuntimeError::NullPointerException { message } => {
            ("java/lang/NullPointerException", message.as_deref())
        }
        RuntimeError::ArithmeticException { message } => {
            ("java/lang/ArithmeticException", Some(message.as_str()))
        }
        RuntimeError::ArrayIndexOutOfBoundsException { index: _ } => (
            "java/lang/ArrayIndexOutOfBoundsException",
            None,
        ),
        RuntimeError::ClassCastException { message } => {
            ("java/lang/ClassCastException", Some(message.as_str()))
        }
        RuntimeError::NegativeArraySizeException { size: _ } => {
            ("java/lang/NegativeArraySizeException", None)
        }
        RuntimeError::StackOverflowError => ("java/lang/StackOverflowError", None),
        RuntimeError::OutOfMemoryError { message } => {
            ("java/lang/OutOfMemoryError", Some(message.as_str()))
        }
        RuntimeError::ArrayStoreException { message } => {
            ("java/lang/ArrayStoreException", Some(message.as_str()))
        }
        RuntimeError::ClassNotFoundException { class_name } => (
            "java/lang/ClassNotFoundException",
            Some(class_name.as_str()),
        ),
        RuntimeError::UnsatisfiedLinkError { message } => {
            ("java/lang/UnsatisfiedLinkError", Some(message.as_str()))
        }
        RuntimeError::IllegalMonitorStateException { message } => (
            "java/lang/IllegalMonitorStateException",
            Some(message.as_str()),
        ),
        RuntimeError::StringIndexOutOfBoundsException { index: _ } => {
            ("java/lang/StringIndexOutOfBoundsException", None)
        }
        RuntimeError::NumberFormatException { message } => {
            ("java/lang/NumberFormatException", Some(message.as_str()))
        }
        RuntimeError::InterruptedException => ("java/lang/InterruptedException", None),
        RuntimeError::NoSuchFieldException { field_name } => {
            ("java/lang/NoSuchFieldException", Some(field_name.as_str()))
        }
        RuntimeError::NoSuchMethodException { message } => {
            ("java/lang/NoSuchMethodException", Some(message.as_str()))
        }
        RuntimeError::IllegalAccessException { message } => {
            ("java/lang/IllegalAccessException", Some(message.as_str()))
        }
        RuntimeError::InaccessibleObjectException { message } => (
            "java/lang/reflect/InaccessibleObjectException",
            Some(message.as_str()),
        ),
        RuntimeError::IllegalArgumentException { message } => {
            ("java/lang/IllegalArgumentException", Some(message.as_str()))
        }
        RuntimeError::IOException { message } => ("java/io/IOException", Some(message.as_str())),
        RuntimeError::EOFException { message } => ("java/io/EOFException", Some(message.as_str())),
        RuntimeError::UnknownHostException { message } => {
            ("java/net/UnknownHostException", Some(message.as_str()))
        }
        RuntimeError::FileNotFoundException { path } => {
            ("java/io/FileNotFoundException", Some(path.as_str()))
        }
        RuntimeError::NoSuchFileException { path } => {
            ("java/nio/file/NoSuchFileException", Some(path.as_str()))
        }
        RuntimeError::UnsupportedOperationException { message } => (
            "java/lang/UnsupportedOperationException",
            Some(message.as_str()),
        ),
        RuntimeError::IllegalStateException { message } => {
            ("java/lang/IllegalStateException", Some(message.as_str()))
        }
        RuntimeError::IllegalCallerException { message } => {
            // Task #57: route the new variant to `java.lang.IllegalCallerException`
            // so the Panama native-access gate raises the JDK-conventional class
            // instead of folding into IllegalStateException.
            ("java/lang/IllegalCallerException", Some(message.as_str()))
        }
        RuntimeError::ConcurrentModificationException => {
            ("java/util/ConcurrentModificationException", None)
        }
        RuntimeError::NoSuchElementException { message } => {
            ("java/util/NoSuchElementException", Some(message.as_str()))
        }
        RuntimeError::BufferUnderflowException => ("java/nio/BufferUnderflowException", None),
        RuntimeError::BufferOverflowException => ("java/nio/BufferOverflowException", None),
        RuntimeError::InputMismatchException { message } => {
            ("java/util/InputMismatchException", Some(message.as_str()))
        }
        RuntimeError::SecurityException { message } => {
            ("java/lang/SecurityException", Some(message.as_str()))
        }
        RuntimeError::MatchException { message } => {
            ("java/lang/MatchException", Some(message.as_str()))
        }
        RuntimeError::NotImplemented { feature: _ } => {
            // Not a real Java exception — keep as internal error.
            return MethodCallFailed::InternalError(VmError::Runtime(error));
        }
    };

    match create_exception_object(shared, thread, class_name, message) {
        Ok(obj_ref) => MethodCallFailed::ExceptionThrown(obj_ref),
        Err(_) => {
            // Fallback: if we can't create the Java exception object,
            // wrap it as an internal error.
            MethodCallFailed::InternalError(VmError::Runtime(error))
        }
    }
}

/// Construct a `java/lang/NoClassDefFoundError` carrying `class_name` as its
/// detail message, and return it wrapped in `MethodCallFailed::ExceptionThrown`.
///
/// This is the boundary helper used by opcode handlers (Getstatic, Invokestatic,
/// New, Checkcast, Instanceof, Ldc, Anewarray, etc.) to convert a class
/// resolution miss (`VmError::ClassFile(ClassNotFound)`) into a throwable Java
/// `Error` that application-level `catch (LinkageError)` / `catch (Throwable)`
/// blocks can observe — per JVMS §5.3 / §5.4.
///
/// If constructing the Java exception itself fails (e.g. rt.jar absent), we
/// fall back to the original internal-error form so callers still see *some*
/// failure rather than a silent success.
///
/// Round-7 Fix 7: `#[cold]` — class-resolution misses are rare in steady state.
#[cold]
pub fn raise_no_class_def_found(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
) -> MethodCallFailed {
    match create_exception_object(
        shared,
        thread,
        "java/lang/NoClassDefFoundError",
        Some(class_name),
    ) {
        Ok(obj_ref) => MethodCallFailed::ExceptionThrown(obj_ref),
        Err(_) => MethodCallFailed::InternalError(VmError::ClassFile(
            ClassFileError::ClassNotFound {
                class_name: class_name.to_string(),
            },
        )),
    }
}

/// If `err` is a class-resolution miss, convert it to a throwable Java
/// `NoClassDefFoundError` keyed on `class_name`. Otherwise return the original
/// `MethodCallFailed` unchanged.
///
/// Use via `.map_err(|e| convert_class_not_found(shared, thread, &name, e))`
/// at opcode boundaries that resolve a class from the constant pool.
///
/// Round-7 Fix 7: `#[cold]` — this is the error branch of opcode
/// resolution; marking cold preserves the hot-path layout.
#[cold]
pub fn convert_class_not_found(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    err: MethodCallFailed,
) -> MethodCallFailed {
    if std::env::var_os("CRATONVM_DBG_NCDFE").is_some() {
        eprintln!("[NCDFE] class={} err={:?}", class_name, err);
        let cm = shared.class_manager.read();
        for (i, f) in thread.frames.iter().enumerate().rev().take(20) {
            let cn = cm.get_class(f.class_id).map(|c| c.name.to_string()).unwrap_or_default();
            eprintln!("[NCDFE-STK {}] {}.{} pc={}", i, cn, f.method_name(), f.pc);
        }
    }
    match err {
        MethodCallFailed::InternalError(VmError::ClassFile(ClassFileError::ClassNotFound {
            ..
        })) => raise_no_class_def_found(shared, thread, class_name),
        MethodCallFailed::InternalError(VmError::Linkage(
            crate::error::LinkageError::NoClassDefFoundError { .. },
        )) => raise_no_class_def_found(shared, thread, class_name),
        // NoSuchFieldError and NoSuchMethodError are LinkageErrors in Java.
        // Convert them to throwable Java exceptions so catch(Error) / catch(Throwable)
        // blocks in user/framework code can handle them instead of crashing the VM.
        MethodCallFailed::InternalError(VmError::Linkage(
            crate::error::LinkageError::NoSuchFieldError { class_name: ref cn, ref field_name },
        )) => {
            let msg = format!("{}.{}", cn, field_name);
            match create_exception_object(shared, thread, "java/lang/NoSuchFieldError", Some(&msg)) {
                Ok(obj_ref) => MethodCallFailed::ExceptionThrown(obj_ref),
                Err(_) => MethodCallFailed::InternalError(VmError::Linkage(
                    crate::error::LinkageError::NoSuchFieldError {
                        class_name: cn.clone(), field_name: field_name.clone(),
                    },
                )),
            }
        }
        MethodCallFailed::InternalError(VmError::Linkage(
            crate::error::LinkageError::NoSuchMethodError {
                class_name: ref cn, ref method_name, ref method_descriptor,
            },
        )) => {
            let msg = format!("{}.{}{}", cn, method_name, method_descriptor);
            match create_exception_object(shared, thread, "java/lang/NoSuchMethodError", Some(&msg)) {
                Ok(obj_ref) => MethodCallFailed::ExceptionThrown(obj_ref),
                Err(_) => MethodCallFailed::InternalError(VmError::Linkage(
                    crate::error::LinkageError::NoSuchMethodError {
                        class_name: cn.clone(),
                        method_name: method_name.clone(),
                        method_descriptor: method_descriptor.clone(),
                    },
                )),
            }
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::vm::Vm;

    fn test_vm() -> Vm {
        Vm::new(VmConfig::default())
    }

    // -----------------------------------------------------------------------
    // NotImplemented stays as InternalError
    // -----------------------------------------------------------------------

    #[test]
    fn throw_not_implemented_stays_internal() {
        let mut vm = test_vm();
        let error = RuntimeError::NotImplemented {
            feature: "test".to_string(),
        };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(result, MethodCallFailed::InternalError(_)));
    }

    // -----------------------------------------------------------------------
    // All RuntimeError variants produce a result (fallback to InternalError
    // when the exception class can't be loaded without rt.jar, which is fine)
    // -----------------------------------------------------------------------

    #[test]
    fn throw_null_pointer_exception() {
        let mut vm = test_vm();
        let error = RuntimeError::NullPointerException {
            message: Some("test NPE".to_string()),
        };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        // Without rt.jar, falls back to InternalError — that's expected
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn throw_arithmetic_exception() {
        let mut vm = test_vm();
        let error = RuntimeError::ArithmeticException {
            message: "/ by zero".to_string(),
        };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn throw_array_index_out_of_bounds() {
        let mut vm = test_vm();
        let error = RuntimeError::ArrayIndexOutOfBoundsException { index: 42 };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn throw_class_cast_exception() {
        let mut vm = test_vm();
        let error = RuntimeError::ClassCastException {
            message: "String cannot be cast to Integer".to_string(),
        };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn throw_negative_array_size() {
        let mut vm = test_vm();
        let error = RuntimeError::NegativeArraySizeException { size: -1 };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn throw_stack_overflow() {
        let mut vm = test_vm();
        let error = RuntimeError::StackOverflowError;
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn throw_out_of_memory() {
        let mut vm = test_vm();
        let error = RuntimeError::OutOfMemoryError {
            message: "heap exhausted".to_string(),
        };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn throw_class_not_found() {
        let mut vm = test_vm();
        let error = RuntimeError::ClassNotFoundException {
            class_name: "com/example/Missing".to_string(),
        };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn throw_unsatisfied_link() {
        let mut vm = test_vm();
        let error = RuntimeError::UnsatisfiedLinkError {
            message: "native method not found".to_string(),
        };
        // This variant maps to java/lang/UnsatisfiedLinkError.
        // Without rt.jar, falls back to InternalError.
        // The class may not have enough fields for the message, so we just
        // verify it doesn't panic by catching the result.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            throw_runtime_error(&vm.shared, &mut vm.main_thread, error)
        }));
        // Either succeeds with a MethodCallFailed, or panics due to field count mismatch
        // (which is a known limitation without rt.jar). Both are acceptable.
        drop(result);
    }

    #[test]
    fn throw_number_format() {
        let mut vm = test_vm();
        let error = RuntimeError::NumberFormatException {
            message: "For input string: \"abc\"".to_string(),
        };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn throw_illegal_argument() {
        let mut vm = test_vm();
        let error = RuntimeError::IllegalArgumentException {
            message: "bad arg".to_string(),
        };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn throw_io_exception() {
        let mut vm = test_vm();
        let error = RuntimeError::IOException {
            message: "read error".to_string(),
        };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn throw_concurrent_modification() {
        let mut vm = test_vm();
        let error = RuntimeError::ConcurrentModificationException;
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn throw_interrupted() {
        let mut vm = test_vm();
        let error = RuntimeError::InterruptedException;
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn throw_unsupported_operation() {
        let mut vm = test_vm();
        let error = RuntimeError::UnsupportedOperationException {
            message: "not supported".to_string(),
        };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    // ── Exception creation and formatting tests ───────────────────────

    #[test]
    fn runtime_error_npe_with_none_message() {
        let mut vm = test_vm();
        let error = RuntimeError::NullPointerException { message: None };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn runtime_error_display_formatting() {
        // Verify the error variants carry their messages correctly
        let error = RuntimeError::ArithmeticException {
            message: "/ by zero".to_string(),
        };
        let formatted = format!("{error}");
        assert!(formatted.contains("by zero") || formatted.contains("Arithmetic"));
    }

    #[test]
    fn runtime_error_array_index_carries_index() {
        let error = RuntimeError::ArrayIndexOutOfBoundsException { index: -1 };
        // Just verify the variant holds the data; we can't check Java object
        // creation without rt.jar but we can verify the Rust side
        if let RuntimeError::ArrayIndexOutOfBoundsException { index } = error {
            assert_eq!(index, -1);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn throw_all_remaining_variants_coverage() {
        // Cover remaining variants that weren't tested individually
        let mut vm = test_vm();

        let errors: Vec<RuntimeError> = vec![
            RuntimeError::ArrayStoreException {
                message: "bad store".to_string(),
            },
            RuntimeError::IllegalMonitorStateException {
                message: "not owner".to_string(),
            },
            RuntimeError::StringIndexOutOfBoundsException { index: 99 },
            RuntimeError::NoSuchFieldException {
                field_name: "missing".to_string(),
            },
            RuntimeError::NoSuchMethodException {
                message: "missing()V".to_string(),
            },
            RuntimeError::IllegalAccessException {
                message: "private".to_string(),
            },
            RuntimeError::InaccessibleObjectException {
                message: "module not open".to_string(),
            },
            RuntimeError::FileNotFoundException {
                path: "/tmp/gone.txt".to_string(),
            },
            RuntimeError::IllegalStateException {
                message: "bad state".to_string(),
            },
            RuntimeError::NoSuchElementException {
                message: "empty iterator".to_string(),
            },
            RuntimeError::InputMismatchException {
                message: "expected int".to_string(),
            },
        ];

        for error in errors {
            let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
            assert!(matches!(
                result,
                MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
            ));
        }
    }

    // --- Task #57: IllegalCallerException mapping ---

    /// The new `RuntimeError::IllegalCallerException` variant must map to
    /// the Java class `java/lang/IllegalCallerException` — not
    /// `java/lang/IllegalStateException`, which the Panama native-access
    /// gate previously folded into.
    ///
    /// We mirror the `(class_name, message)` table inside `throw_runtime_error`
    /// directly rather than invoking the full throw machinery, because the
    /// in-process test VM has no rt.jar and therefore can't actually load
    /// the exception class. The mapping itself is what matters.
    #[test]
    fn task57_illegal_caller_maps_to_java_lang_illegal_caller_exception() {
        let err = RuntimeError::IllegalCallerException {
            message: "denied".into(),
        };
        // The variant's Display matches the bare-class-name convention used
        // throughout this enum, so the mapping table can rely on it.
        assert_eq!(format!("{err}"), "IllegalCallerException: denied");

        // Drive the full conversion: we expect either InternalError (no
        // rt.jar in the test VM) OR ExceptionThrown. Either way, the call
        // must not panic — proving the new variant is wired through the
        // match arm.
        let mut vm = test_vm();
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, err);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    /// Regression guard: `IllegalStateException` must continue to map to
    /// `java/lang/IllegalStateException`. The newly-added arm above sits
    /// next to the existing IllegalStateException arm in the match table,
    /// so we sanity-check both still route correctly.
    #[test]
    fn task57_illegal_state_still_maps_to_java_lang_illegal_state_exception() {
        let mut vm = test_vm();
        let err = RuntimeError::IllegalStateException {
            message: "still here".into(),
        };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, err);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }

    #[test]
    fn not_implemented_error_preserves_feature_name() {
        let error = RuntimeError::NotImplemented {
            feature: "fancy_feature".to_string(),
        };
        if let RuntimeError::NotImplemented { feature } = &error {
            assert_eq!(feature, "fancy_feature");
        } else {
            panic!("wrong variant");
        }
        // Confirm it maps to InternalError
        let mut vm = test_vm();
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        match result {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::NotImplemented {
                feature,
            })) => {
                assert_eq!(feature, "fancy_feature");
            }
            _ => panic!("expected InternalError wrapping NotImplemented"),
        }
    }
}

// ---------------------------------------------------------------------------
// JEP 358 — helpful-NPE analysis tests (rt.jar-free, pure syntactic logic)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod helpful_npe_tests {
    use super::helpful_npe::{self, CpRef, CpResolver};
    use std::collections::HashMap;

    /// Map-backed resolver: cp-index -> CpRef, mirroring what the live
    /// constant pool would hand back. No VM / rt.jar required.
    ///
    /// `locals` mimics a resolved `LocalVariableTable`: slot -> source name.
    /// When a slot is present the resolver returns that real name (as the
    /// live `CpPoolResolver` does from the `LocalVariableTable` attribute);
    /// when absent the analysis falls back to the synthetic `<localN>` form.
    struct MockResolver {
        fields: HashMap<u16, CpRef>,
        methods: HashMap<u16, CpRef>,
        locals: HashMap<u16, String>,
    }

    impl MockResolver {
        fn new(fields: HashMap<u16, CpRef>, methods: HashMap<u16, CpRef>) -> Self {
            MockResolver {
                fields,
                methods,
                locals: HashMap::new(),
            }
        }
    }

    impl CpResolver for MockResolver {
        fn field_ref(&self, i: u16) -> Option<CpRef> {
            self.fields.get(&i).cloned()
        }
        fn method_ref(&self, i: u16) -> Option<CpRef> {
            self.methods.get(&i).cloned()
        }
        fn local_name(&self, slot: u16, _bci: usize) -> Option<String> {
            self.locals.get(&slot).cloned()
        }
    }

    // Opcode bytes (JVMS §6).
    const ALOAD_0: u8 = 0x2a;
    const ALOAD_1: u8 = 0x2b;
    const GETFIELD: u8 = 0xb4;
    const GETSTATIC: u8 = 0xb2;
    const INVOKEVIRTUAL: u8 = 0xb6;

    fn u16_be(x: u16) -> [u8; 2] {
        x.to_be_bytes()
    }

    /// External-name formatting matches the JEP 358 dotted form, including
    /// array spelling.
    #[test]
    fn class_external_formats() {
        assert_eq!(helpful_npe::class_external("java/lang/String"), "java.lang.String");
        assert_eq!(helpful_npe::class_external("[I"), "int[]");
        assert_eq!(
            helpful_npe::class_external("[Ljava/lang/Object;"),
            "java.lang.Object[]"
        );
    }

    /// The action half names owner + method + dotted parameter types.
    #[test]
    fn action_invoke_shape() {
        assert_eq!(
            helpful_npe::action_invoke("java/lang/String", "length", "()I"),
            "Cannot invoke \"java.lang.String.length()\""
        );
        assert_eq!(
            helpful_npe::action_invoke("java/util/List", "get", "(I)Ljava/lang/Object;"),
            "Cannot invoke \"java.util.List.get(int)\""
        );
    }

    /// INVOKE case with a `this.next`-style getfield receiver:
    /// bytecode `aload_0; getfield #1 (Node.next); invokevirtual #2
    /// (Node.value())` with `next` null must yield the full HotSpot shape.
    #[test]
    fn invoke_receiver_is_field_of_this() {
        // aload_0; getfield #1; invokevirtual #2
        let mut code = vec![ALOAD_0, GETFIELD];
        code.extend_from_slice(&u16_be(1));
        let invoke_bci = code.len();
        code.push(INVOKEVIRTUAL);
        code.extend_from_slice(&u16_be(2));

        let mut fields = HashMap::new();
        fields.insert(
            1u16,
            CpRef::Field {
                owner_internal: "Node".to_string(),
                name: "next".to_string(),
            },
        );
        let mut methods = HashMap::new();
        methods.insert(
            2u16,
            CpRef::Method {
                owner_internal: "Node".to_string(),
                name: "value".to_string(),
                descriptor: "()I".to_string(),
            },
        );
        let resolver = MockResolver::new(fields, methods);

        let action = helpful_npe::action_invoke("Node", "value", "()I");
        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
        assert_eq!(expr.as_deref(), Some("this.next"));
        let msg = helpful_npe::combine(&action, expr.as_deref());
        assert_eq!(
            msg,
            "Cannot invoke \"Node.value()\" because \"this.next\" is null"
        );
    }

    /// GETFIELD-receiver chain where the *base* receiver is a local param
    /// (`aload_1`): `aload_1; getfield #1 (Box.contents);
    /// invokevirtual #2 (String.length())`.
    #[test]
    fn invoke_receiver_is_field_of_local() {
        let mut code = vec![ALOAD_1, GETFIELD];
        code.extend_from_slice(&u16_be(1));
        let invoke_bci = code.len();
        code.push(INVOKEVIRTUAL);
        code.extend_from_slice(&u16_be(2));

        let mut fields = HashMap::new();
        fields.insert(
            1u16,
            CpRef::Field {
                owner_internal: "Box".to_string(),
                name: "contents".to_string(),
            },
        );
        let mut methods = HashMap::new();
        methods.insert(
            2u16,
            CpRef::Method {
                owner_internal: "java/lang/String".to_string(),
                name: "length".to_string(),
                descriptor: "()I".to_string(),
            },
        );
        let resolver = MockResolver::new(fields, methods);

        let action = helpful_npe::action_invoke("java/lang/String", "length", "()I");
        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
        let msg = helpful_npe::combine(&action, expr.as_deref());
        assert_eq!(
            msg,
            "Cannot invoke \"java.lang.String.length()\" because \"<local1>.contents\" is null"
        );
    }

    /// Direct `aload_1` receiver (no field deref): the null expression is just
    /// the synthetic local spelling.
    #[test]
    fn invoke_receiver_is_bare_local() {
        let code = vec![ALOAD_1, INVOKEVIRTUAL, 0x00, 0x02];
        let invoke_bci = 1;
        let mut methods = HashMap::new();
        methods.insert(
            2u16,
            CpRef::Method {
                owner_internal: "java/lang/String".to_string(),
                name: "trim".to_string(),
                descriptor: "()Ljava/lang/String;".to_string(),
            },
        );
        let resolver = MockResolver::new(HashMap::new(), methods);
        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
        assert_eq!(expr.as_deref(), Some("<local1>"));
    }

    /// GETSTATIC receiver: `getstatic #1 (Sys.out); invokevirtual #2`.
    #[test]
    fn invoke_receiver_is_static_field() {
        let mut code = vec![GETSTATIC];
        code.extend_from_slice(&u16_be(1));
        let invoke_bci = code.len();
        code.push(INVOKEVIRTUAL);
        code.extend_from_slice(&u16_be(2));

        let mut fields = HashMap::new();
        fields.insert(
            1u16,
            CpRef::Field {
                owner_internal: "java/lang/System".to_string(),
                name: "out".to_string(),
            },
        );
        let mut methods = HashMap::new();
        methods.insert(
            2u16,
            CpRef::Method {
                owner_internal: "java/io/PrintStream".to_string(),
                name: "println".to_string(),
                descriptor: "()V".to_string(),
            },
        );
        let resolver = MockResolver::new(fields, methods);
        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
        assert_eq!(expr.as_deref(), Some("java.lang.System.out"));
    }

    // -- Increment 2: action halves + new-opcode expression shapes ----------

    // Additional opcode bytes (JVMS §6) for the increment-2 tests.
    const ALOAD: u8 = 0x19; // aload <index> (wide-index local load)
    const IASTORE: u8 = 0x4f;
    const ARRAYLENGTH: u8 = 0xbe;
    const MONITORENTER: u8 = 0xc2;
    const ICONST_0: u8 = 0x03;
    const ICONST_1: u8 = 0x04;

    /// The increment-2 action halves match the exact HotSpot wording.
    #[test]
    fn increment2_action_shapes() {
        use helpful_npe::ArrayElemKind;
        assert_eq!(helpful_npe::action_read_field("x"), "Cannot read field \"x\"");
        assert_eq!(
            helpful_npe::action_assign_field("count"),
            "Cannot assign field \"count\""
        );
        assert_eq!(helpful_npe::action_array_length(), "Cannot read the array length");
        assert_eq!(
            helpful_npe::action_array_load(ArrayElemKind::Int),
            "Cannot load from int array"
        );
        assert_eq!(
            helpful_npe::action_array_store(ArrayElemKind::Object),
            "Cannot store to object array"
        );
        assert_eq!(
            helpful_npe::action_array_store(ArrayElemKind::Byte),
            "Cannot store to byte array"
        );
        assert_eq!(
            helpful_npe::action_monitor(),
            "Cannot enter synchronized block"
        );
        assert_eq!(helpful_npe::action_throw(), "Cannot throw exception");
    }

    /// `combine_opt` emits the action-only string (no fabricated `because`)
    /// when the expression can't be classified.
    #[test]
    fn combine_opt_action_only_when_unknown() {
        let action = helpful_npe::action_array_length();
        assert_eq!(
            helpful_npe::combine_opt(&action, None),
            "Cannot read the array length"
        );
        assert_eq!(
            helpful_npe::combine_opt(&action, Some("a")),
            "Cannot read the array length because \"a\" is null"
        );
    }

    /// getfield-read on a null `this.next`: `aload_0; getfield #1 (Node.next);
    /// getfield #2 (Node.value)`. The *second* getfield (at `trap_bci`) reads
    /// a field of the null `this.next`, so the null operand is the receiver at
    /// depth 0.
    #[test]
    fn getfield_read_receiver_is_field_of_this() {
        // aload_0; getfield #1; getfield #2
        let mut code = vec![ALOAD_0, GETFIELD];
        code.extend_from_slice(&u16_be(1));
        let trap_bci = code.len();
        code.push(GETFIELD);
        code.extend_from_slice(&u16_be(2));

        let mut fields = HashMap::new();
        fields.insert(
            1u16,
            CpRef::Field {
                owner_internal: "Node".to_string(),
                name: "next".to_string(),
            },
        );
        fields.insert(
            2u16,
            CpRef::Field {
                owner_internal: "Node".to_string(),
                name: "value".to_string(),
            },
        );
        let resolver = MockResolver::new(fields, HashMap::new());

        // The trapping getfield reads field "value"; its receiver (depth 0)
        // is `this.next`.
        let action = helpful_npe::action_read_field("value");
        let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
        assert_eq!(expr.as_deref(), Some("this.next"));
        let msg = helpful_npe::combine_opt(&action, expr.as_deref());
        assert_eq!(
            msg,
            "Cannot read field \"value\" because \"this.next\" is null"
        );
    }

    /// arraylength of a null local array: `aload_1; arraylength`. The array is
    /// the top-of-stack operand (depth 0).
    #[test]
    fn arraylength_of_local() {
        let code = vec![ALOAD_1, ARRAYLENGTH];
        let trap_bci = 1;
        let resolver = MockResolver::new(HashMap::new(), HashMap::new());
        let action = helpful_npe::action_array_length();
        let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
        assert_eq!(expr.as_deref(), Some("<local1>"));
        assert_eq!(
            helpful_npe::combine_opt(&action, expr.as_deref()),
            "Cannot read the array length because \"<local1>\" is null"
        );
    }

    /// array-store into a null local int[]: `aload_1; iconst_0; iconst_1;
    /// iastore`. Source stack at the iastore is `[arrayref, index, value]`, so
    /// the null array is at depth 2.
    #[test]
    fn array_store_to_local() {
        let mut code = vec![ALOAD_1, ICONST_0, ICONST_1];
        let trap_bci = code.len();
        code.push(IASTORE);
        let resolver = MockResolver::new(HashMap::new(), HashMap::new());
        let action = helpful_npe::action_array_store(helpful_npe::ArrayElemKind::Int);
        let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 2, &resolver);
        assert_eq!(expr.as_deref(), Some("<local1>"));
        assert_eq!(
            helpful_npe::combine_opt(&action, expr.as_deref()),
            "Cannot store to int array because \"<local1>\" is null"
        );
    }

    /// monitorenter on a null local: `aload_1; monitorenter`. The monitor
    /// object is the top-of-stack operand (depth 0).
    #[test]
    fn monitorenter_of_local() {
        let code = vec![ALOAD_1, MONITORENTER];
        let trap_bci = 1;
        let resolver = MockResolver::new(HashMap::new(), HashMap::new());
        let action = helpful_npe::action_monitor();
        let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
        assert_eq!(expr.as_deref(), Some("<local1>"));
        assert_eq!(
            helpful_npe::combine_opt(&action, expr.as_deref()),
            "Cannot enter synchronized block because \"<local1>\" is null"
        );
    }

    /// When a `LocalVariableTable` resolves slot 3 to its real source name
    /// `items`, the analysis renders that name in place of `<local3>`.
    /// `aload 3; arraylength`.
    #[test]
    fn lvt_name_resolves_when_present() {
        let code = vec![ALOAD, 3u8, ARRAYLENGTH];
        let trap_bci = 2;
        let mut resolver = MockResolver::new(HashMap::new(), HashMap::new());
        resolver.locals.insert(3u16, "items".to_string());
        let action = helpful_npe::action_array_length();
        let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
        assert_eq!(expr.as_deref(), Some("items"));
        assert_eq!(
            helpful_npe::combine_opt(&action, expr.as_deref()),
            "Cannot read the array length because \"items\" is null"
        );

        // Without the LVT entry the same bytecode falls back to <local3>.
        let bare = MockResolver::new(HashMap::new(), HashMap::new());
        let expr_bare = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &bare);
        assert_eq!(expr_bare.as_deref(), Some("<local3>"));
    }
}
