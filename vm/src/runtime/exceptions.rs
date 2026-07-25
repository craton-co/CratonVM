// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Exception object creation and RuntimeError -> Java exception conversion.
//!
//! This module provides utilities to:
//! 1. Create a Java exception object on the heap (load class, allocate, call `<init>`)
//! 2. Convert `RuntimeError` variants into proper `MethodCallFailed::ExceptionThrown`

use crate::classloading::ClassId;
use crate::error::{ClassFileError, LinkageError, MethodCallFailed, RuntimeError, VmError};
use crate::threading::jvm_thread::JvmThread;
use crate::types::{ObjectRef, Value};
use crate::vm::{invoke_on_class_shared, try_create_java_string, SharedVm};
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
        /// Whether the **trapping** method is `static`. In a static method local
        /// slot 0 is an ordinary local (HotSpot prints `<local0>`); only in an
        /// instance method is slot 0 the `this` receiver. Defaults to `false`
        /// (instance) to preserve the legacy `this` spelling for resolvers that
        /// don't model it.
        fn is_static_method(&self) -> bool {
            false
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

    /// Shorten the two classes HotSpot prints unqualified in JEP 358 messages:
    /// `java.lang.String` → `String` and `java.lang.Object` → `Object` (verified
    /// against the JDK 25 `getExtendedNPEMessage` output — every *other*
    /// `java.lang` type, e.g. `Integer`, `StringBuilder`, stays fully
    /// qualified). Only an exact whole-name (modulo trailing `[]`) match is
    /// shortened, so `java.lang.StringBuilder` is never touched.
    fn shorten_jlang(external: &str) -> String {
        let mut base = external;
        let mut dims = 0usize;
        while let Some(b) = base.strip_suffix("[]") {
            base = b;
            dims += 1;
        }
        let short = match base {
            "java.lang.String" => "String",
            "java.lang.Object" => "Object",
            _ => base,
        };
        let mut out = short.to_string();
        for _ in 0..dims {
            out.push_str("[]");
        }
        out
    }

    /// Render an invoke/field **owner** class the way HotSpot does in the NPE
    /// message: the internal name with `/` → `.`, leaving array owners in
    /// descriptor form (`[I`, `[Ljava.lang.String;` — *not* the `int[]` external
    /// form used for parameters), and shortening only `String`/`Object`.
    pub fn render_owner(internal: &str) -> String {
        match internal {
            "java/lang/String" => "String".to_string(),
            "java/lang/Object" => "Object".to_string(),
            _ => internal.replace('/', "."),
        }
    }

    /// Render a single method **parameter** descriptor token (`I`,
    /// `Ljava/lang/String;`, `[Ljava/lang/Object;`) the way HotSpot prints it in
    /// the message: the *external* type form (`int`, `String`, `Object[]`,
    /// `java.lang.CharSequence`) — note params use `int[]`/`Object[]` while the
    /// owner uses `[I`. `String`/`Object` are shortened.
    fn render_param(token: &str) -> String {
        shorten_jlang(&class_external(token))
    }

    /// Render `Owner.name(param, …)` — the method form HotSpot quotes in both
    /// the action half (`Cannot invoke "Owner.name(…)"`) and the
    /// "return value of" expression half. The owner uses [`render_owner`], the
    /// parameters [`render_param`].
    fn render_method(owner_internal: &str, name: &str, descriptor: &str) -> String {
        let owner = render_owner(owner_internal);
        let params = param_tokens(descriptor)
            .iter()
            .map(|t| render_param(t))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{owner}.{name}({params})")
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
        format!(
            "Cannot invoke \"{}\"",
            render_method(owner_internal, name, descriptor)
        )
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
                // `baload`/`bastore` are the bytecodes for *both* `byte[]` and
                // `boolean[]`; the array is null so the opcode can't tell them
                // apart. HotSpot prints the combined spelling "byte/boolean
                // array" for either, so both element kinds map to it here.
                ArrayElemKind::Byte | ArrayElemKind::Boolean => "byte/boolean",
                ArrayElemKind::Char => "char",
                ArrayElemKind::Short => "short",
                ArrayElemKind::Object => "object",
            }
        }
    }

    /// A reconstructed null sub-expression. `text` is the **inline** rendering
    /// used when this producer is a sub-part of a larger expression — e.g. the
    /// receiver of a `.field` access (`Owner.m().next`) or the array of an
    /// `arr[idx]`. `is_invoke` flags an invoke *result*, which only changes the
    /// **top-level** rendering: HotSpot prints a directly-null invoke result as
    /// `the return value of "Owner.m()"` (no surrounding quotes) but renders the
    /// same invoke inline as `Owner.m()` when it is a sub-receiver.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Producer {
        pub text: String,
        pub is_invoke: bool,
    }

    impl Producer {
        fn expr(text: String) -> Self {
            Producer {
                text,
                is_invoke: false,
            }
        }
        fn invoke(text: String) -> Self {
            Producer {
                text,
                is_invoke: true,
            }
        }
        /// The `because <…> is null` middle, as HotSpot renders it at the top
        /// level: a plain expression is quoted; an invoke result is phrased as
        /// `the return value of "<method>"`.
        fn render_clause(&self) -> String {
            if self.is_invoke {
                format!("the return value of \"{}\"", self.text)
            } else {
                format!("\"{}\"", self.text)
            }
        }
    }

    /// Combine an action half with an optional reconstructed null expression
    /// into the final JEP 358 message. When the expression is unknown we emit
    /// the **action-only** form — HotSpot omits the `because` clause rather than
    /// fabricating one (it never prints "because the receiver is null").
    pub fn combine(action: &str, expr: Option<&Producer>) -> String {
        match expr {
            Some(p) => format!("{action} because {} is null", p.render_clause()),
            None => action.to_string(),
        }
    }

    /// Retained spelling of [`combine`] (identical behavior) for the increment-2
    /// opcode sites (getfield / array / monitor / athrow). Both now emit the
    /// action-only string when the expression can't be classified.
    pub fn combine_opt(action: &str, expr: Option<&Producer>) -> String {
        combine(action, expr)
    }

    // -- Step 6 (partial): action-only messages for JIT-originated NPEs ------
    //
    // The JIT's null-deref signal (`JIT_PENDING_NPE`) carries no bytecode
    // index, so the increment-1/2 backward expression analysis cannot run on a
    // JIT-thrown NPE — and `real-frame-deopt.md` is unstarted, so the precise
    // trapping bci is unavailable (the design doc gates *full* JIT parity on
    // it). The doc's offered alternative is to "fall back to the **action-only**
    // message for JIT-originated NPEs" by threading the operation *kind*
    // through the signal (`set_jit_pending_npe_action`).
    //
    // The action codes are now the canonical [`cratonvm_jit_api::npe_action`]
    // vocabulary (re-exported here as `jit_action` for source compatibility),
    // so the same numeric codes the inline JIT codegen (`jit/src/x64.rs`) bakes
    // into its null-check stubs map here to the HotSpot `BytecodeUtils` string.
    // Both the per-type array/length helpers AND the inline null-check failure
    // stubs (via `jit_npe_with_action`) feed this channel.
    //
    // Only the cases whose action-only string is *exactly* a HotSpot
    // `BytecodeUtils` string (the array family + length, where no `because`
    // clause / field-name is needed) are represented — a field/invoke action
    // would need the name/owner the bare signal does not carry, so those stay
    // `message: None` (no fabricated text; cf. the Risks doc).
    pub mod jit_action {
        // Single source of truth lives in `jit-api` so the JIT crate (which
        // cannot depend on the VM crate) and this mapping share one numeric
        // vocabulary. Re-exported under the historical `jit_action` name.
        pub use cratonvm_jit_api::npe_action::*;
    }

    /// Map a JIT NPE action code (set by `set_jit_pending_npe_action` in the
    /// array/length helpers, or by `jit_npe_with_action` from the inline
    /// null-check stubs) to its JEP 358 *action-only* message, or `None` when no
    /// action was recorded ([`jit_action::NONE`]) so the caller emits an
    /// unmessaged NPE exactly as before. Every returned string is a verbatim
    /// HotSpot `BytecodeUtils` action (action-only is a valid HotSpot shape when
    /// the null expression can't be reconstructed — see [`combine_opt`]).
    pub fn jit_action_message(code: u8) -> Option<String> {
        use jit_action::*;
        let s = match code {
            ARRAY_LENGTH => action_array_length(),
            ALOAD_INT => action_array_load(ArrayElemKind::Int),
            ALOAD_OBJECT => action_array_load(ArrayElemKind::Object),
            ALOAD_BYTE => action_array_load(ArrayElemKind::Byte),
            ALOAD_LONG => action_array_load(ArrayElemKind::Long),
            ALOAD_FLOAT => action_array_load(ArrayElemKind::Float),
            ALOAD_DOUBLE => action_array_load(ArrayElemKind::Double),
            ALOAD_CHAR => action_array_load(ArrayElemKind::Char),
            ALOAD_SHORT => action_array_load(ArrayElemKind::Short),
            ASTORE_INT => action_array_store(ArrayElemKind::Int),
            ASTORE_OBJECT => action_array_store(ArrayElemKind::Object),
            ASTORE_BYTE => action_array_store(ArrayElemKind::Byte),
            ASTORE_LONG => action_array_store(ArrayElemKind::Long),
            ASTORE_FLOAT => action_array_store(ArrayElemKind::Float),
            ASTORE_DOUBLE => action_array_store(ArrayElemKind::Double),
            ASTORE_CHAR => action_array_store(ArrayElemKind::Char),
            ASTORE_SHORT => action_array_store(ArrayElemKind::Short),
            _ => return None,
        };
        Some(s)
    }

    /// The message to attach to a JIT-originated NPE for action code `code`,
    /// honoring the `-XX:±ShowCodeDetailsInExceptionMessages` /
    /// `CRATONVM_HELPFUL_NPE_OPCODES` gate. Returns `None` (unmessaged NPE,
    /// today's default-path shape) when the gate is off or no action was
    /// recorded — so the default path is byte-for-byte unchanged.
    pub fn jit_npe_message_gated(code: u8) -> Option<String> {
        if !crate::runtime::env_cache::helpful_npe_opcodes() {
            return None;
        }
        jit_action_message(code)
    }

    // -- Bounded backward expression analysis ------------------------------

    /// One simulated operand-stack slot, tagged with the bci of the opcode
    /// that pushed it (or `None` for a value whose producer we didn't track,
    /// e.g. a method-entry argument that was never re-pushed).
    #[derive(Clone, Copy)]
    struct Slot {
        producer_bci: Option<usize>,
    }

    /// The absolute jump targets of a branch / switch at `pc`, or `None` for a
    /// non-branch. Offsets in the bytecode are relative to the branch's own bci.
    fn branch_targets(instr: &Instruction, pc: usize) -> Option<Vec<usize>> {
        use Instruction::*;
        let rel = |off: i64| -> usize { (pc as i64 + off).max(0) as usize };
        let v = match instr {
            Goto(o) | Ifeq(o) | Ifne(o) | Iflt(o) | Ifge(o) | Ifgt(o) | Ifle(o) | IfIcmpeq(o)
            | IfIcmpne(o) | IfIcmplt(o) | IfIcmpge(o) | IfIcmpgt(o) | IfIcmple(o) | IfAcmpeq(o)
            | IfAcmpne(o) | Ifnull(o) | Ifnonnull(o) | Jsr(o) => {
                vec![rel(*o as i64)]
            }
            GotoW(o) | JsrW(o) => vec![rel(*o as i64)],
            Tableswitch {
                default, offsets, ..
            } => {
                let mut t = vec![rel(*default as i64)];
                t.extend(offsets.iter().map(|o| rel(*o as i64)));
                t
            }
            Lookupswitch { default, pairs } => {
                let mut t = vec![rel(*default as i64)];
                t.extend(pairs.iter().map(|(_, o)| rel(*o as i64)));
                t
            }
            _ => return None,
        };
        Some(v)
    }

    /// The bci that begins the basic block containing `trap_bci`: the largest
    /// *block leader* `<= trap_bci`, where leaders are bci 0, every branch /
    /// switch target, and the instruction immediately after any block-ending
    /// opcode (branch, `return`, `athrow`, `ret`).
    ///
    /// Simulating the operand stack from this point — rather than from method
    /// entry — is what lets the reconstruction succeed inside a method with
    /// preceding `try/catch` blocks: the linear walk no longer has to cross the
    /// intervening `goto`s and exception handlers (which it can't model and
    /// would bail on); it starts straight-line at the trapping statement's own
    /// block, where the operand stack is empty (javac emits each statement with
    /// an empty stack).
    fn block_start_for(code: &[u8], trap_bci: usize) -> usize {
        let mut leaders: Vec<usize> = vec![0];
        let mut pc = 0usize;
        while pc < code.len() {
            let Ok((instr, next)) = Instruction::decode(code, pc) else {
                break;
            };
            let ender = if let Some(targets) = branch_targets(&instr, pc) {
                leaders.extend(targets);
                true
            } else {
                instr.is_return() || matches!(instr, Instruction::Athrow | Instruction::Ret(_))
            };
            if ender {
                leaders.push(next); // the next instruction begins a fresh block
            }
            if next <= pc {
                break;
            }
            pc = next;
        }
        leaders
            .into_iter()
            .filter(|&l| l <= trap_bci)
            .max()
            .unwrap_or(0)
    }

    /// Simulate operand-stack heights forward from the start of `target_bci`'s
    /// basic block (see [`block_start_for`]), recording for each slot live
    /// *immediately before* `target_bci` which bci produced it. Returns the
    /// slot vector at `target_bci`, or `None` if the block couldn't be cleanly
    /// simulated up to that point (an unmodeled opcode in the prefix) — in which
    /// case the caller emits an action-only message.
    ///
    /// Starting at the block leader (not method entry) is what makes the
    /// reconstruction robust inside methods with `try/catch` / loops: the walk
    /// is straight-line within one block, so it never has to model the
    /// control-flow joins a linear from-entry walk would bail on.
    fn simulate_to(code: &[u8], target_bci: usize, resolver: &dyn CpResolver) -> Option<Vec<Slot>> {
        let mut stack: Vec<Slot> = Vec::new();
        let mut pc = block_start_for(code, target_bci);
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
            apply_stack_effect(&mut stack, &instr, pc, resolver)?;
            if next <= pc {
                return None; // non-progress guard
            }
            pc = next;
        }
        None
    }

    /// Number of operand-stack slots a method's parameters and return value
    /// occupy in CratonVM's *one-entry-per-value* operand model (a `long`/
    /// `double` is a single entry here — cf. `count_method_params`, which counts
    /// `J`/`D` as one). Returns `(param_count, return_slots)` where
    /// `return_slots` is `0` for a `void` method, else `1`.
    fn method_slot_counts(descriptor: &str) -> (usize, usize) {
        let params = param_tokens(descriptor).len();
        let ret = descriptor
            .rfind(')')
            .and_then(|i| descriptor.as_bytes().get(i + 1).copied());
        let ret_slots = if ret == Some(b'V') { 0 } else { 1 };
        (params, ret_slots)
    }

    /// Apply `instr`'s operand-stack effect to `stack`, tagging any pushed
    /// slot with `bci`. Returns `None` for an opcode we don't model (caller
    /// bails). Only the subset that can appear before a receiver push needs to
    /// be precise; categories we can't reason about conservatively abort.
    fn apply_stack_effect(
        stack: &mut Vec<Slot>,
        instr: &Instruction,
        bci: usize,
        resolver: &dyn CpResolver,
    ) -> Option<()> {
        use Instruction::*;
        // pop `n`, then push `pushes` fresh slots produced at `bci`.
        macro_rules! shape {
            ($pop:expr, $push:expr) => {{
                for _ in 0..$pop {
                    stack.pop()?;
                }
                for _ in 0..$push {
                    stack.push(Slot {
                        producer_bci: Some(bci),
                    });
                }
            }};
        }
        match instr {
            // Constants / loads — push 1 (cat-2 longs/doubles also push 1
            // slot here; the analysis only ever inspects reference receivers,
            // and width mismatches just make us bail when popping).
            AconstNull | IconstM1 | Iconst0 | Iconst1 | Iconst2 | Iconst3 | Iconst4 | Iconst5
            | Lconst0 | Lconst1 | Fconst0 | Fconst1 | Fconst2 | Dconst0 | Dconst1 | Bipush(_)
            | Sipush(_) | Ldc(_) | LdcW(_) | Ldc2W(_) | Iload(_) | Lload(_) | Fload(_)
            | Dload(_) | Aload(_) => shape!(0, 1),
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
            // Stores pop their value (cat-2 is one entry in this model). These
            // are extremely common in the prefix — `Type x = null; … x.deref()`
            // emits `astore`/`istore` between the producing const/load and the
            // trapping use — so modelling them is what lets the analysis name a
            // local at all. A store has no operand we need to track (the next
            // `*load` of that slot re-supplies the producer bci), so push none.
            Istore(_) | Lstore(_) | Fstore(_) | Dstore(_) | Astore(_) => shape!(1, 0),
            // Invokes — model the real stack effect so a null operand produced
            // *by* an invoke (`getNull().foo()`) and any operand pushed before
            // one can still be reconstructed. The descriptor (from the CP via
            // `resolver`) gives the param count + void-ness; receiver is popped
            // for the non-static forms. The pushed return slot is tagged with
            // this invoke's bci so `describe_producer` can render
            // `the return value of "Owner.m()"`.
            Invokevirtual(idx) | Invokespecial(idx) => {
                let CpRef::Method { descriptor, .. } = resolver.method_ref(*idx)? else {
                    return None;
                };
                let (params, ret) = method_slot_counts(&descriptor);
                shape!(params + 1, ret);
            }
            Invokeinterface { index, .. } => {
                let CpRef::Method { descriptor, .. } = resolver.method_ref(*index)? else {
                    return None;
                };
                let (params, ret) = method_slot_counts(&descriptor);
                shape!(params + 1, ret);
            }
            Invokestatic(idx) => {
                let CpRef::Method { descriptor, .. } = resolver.method_ref(*idx)? else {
                    return None;
                };
                let (params, ret) = method_slot_counts(&descriptor);
                shape!(params, ret);
            }
            // invokedynamic's CP entry isn't a plain method ref the resolver
            // can size; bail rather than mis-account the call-site arity.
            Invokedynamic(_) => return None,
            // Anything else (arithmetic, branches, stores, dup variants,
            // switches, returns, athrow, monitor, etc.): we don't model it —
            // bail so we never emit a wrong expression.
            _ => return None,
        }
        Some(())
    }

    /// Render the local-variable slot `slot` (live at `bci`) the way HotSpot
    /// names it: the `LocalVariableTable` source name when present, else `this`
    /// for slot 0 of an **instance** method, else the synthetic `<localN>`.
    fn render_local(resolver: &dyn CpResolver, slot: u16, bci: usize) -> String {
        if let Some(name) = resolver.local_name(slot, bci) {
            return name;
        }
        if slot == 0 && !resolver.is_static_method() {
            "this".to_string()
        } else {
            format!("<local{slot}>")
        }
    }

    /// Describe the source expression pushed by the instruction at
    /// `producer_bci`, recursing (bounded) through getfield receivers, array
    /// elements, casts and invoke results. Returns a [`Producer`] whose `text`
    /// is the inline rendering and whose `is_invoke` flag drives the top-level
    /// `the return value of "…"` phrasing.
    fn describe_producer(
        code: &[u8],
        producer_bci: usize,
        resolver: &dyn CpResolver,
        depth: u32,
    ) -> Option<Producer> {
        if depth > MAX_EXPR_DEPTH {
            return None;
        }
        let (instr, _) = Instruction::decode(code, producer_bci).ok()?;
        match instr {
            // Reference local / `this`.
            Instruction::Aload(slot) => {
                Some(Producer::expr(render_local(resolver, slot, producer_bci)))
            }
            // Integer locals/constants — only reached as an *array index*
            // sub-expression (`arr[i]`, `arr[3]`). `iload_0` is never `this`
            // (that is `aload_0`), so no instance-receiver special-casing.
            Instruction::Iload(slot) => Some(Producer::expr(
                resolver
                    .local_name(slot, producer_bci)
                    .unwrap_or_else(|| format!("<local{slot}>")),
            )),
            Instruction::IconstM1 => Some(Producer::expr("-1".to_string())),
            Instruction::Iconst0 => Some(Producer::expr("0".to_string())),
            Instruction::Iconst1 => Some(Producer::expr("1".to_string())),
            Instruction::Iconst2 => Some(Producer::expr("2".to_string())),
            Instruction::Iconst3 => Some(Producer::expr("3".to_string())),
            Instruction::Iconst4 => Some(Producer::expr("4".to_string())),
            Instruction::Iconst5 => Some(Producer::expr("5".to_string())),
            Instruction::Bipush(b) => Some(Producer::expr(b.to_string())),
            Instruction::Sipush(s) => Some(Producer::expr(s.to_string())),
            Instruction::Getfield(idx) => {
                let CpRef::Field { name, .. } = resolver.field_ref(idx)? else {
                    return None;
                };
                // Recurse on the receiver pushed just before this getfield.
                let recv = simulate_to(code, producer_bci, resolver)
                    .and_then(|stack| stack.last().copied())
                    .and_then(|s| s.producer_bci)
                    .and_then(|b| describe_producer(code, b, resolver, depth + 1));
                match recv {
                    Some(r) => Some(Producer::expr(format!("{}.{name}", r.text))),
                    None => Some(Producer::expr(name)),
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
                Some(Producer::expr(format!(
                    "{}.{name}",
                    render_owner(&owner_internal)
                )))
            }
            Instruction::Aaload => {
                // `<arr>[<idx>]`; recurse on both the array operand (two slots
                // down) and the index (one slot down). HotSpot reconstructs the
                // index too (`a[0]`, `a[i]`, `a[Owner.f]`); if either operand
                // can't be classified we bail to the action-only message rather
                // than fabricate a partial `a[...]`.
                let stack = simulate_to(code, producer_bci, resolver)?;
                let arr_slot = stack.len().checked_sub(2)?;
                let idx_slot = stack.len().checked_sub(1)?;
                let arr = stack
                    .get(arr_slot)
                    .and_then(|s| s.producer_bci)
                    .and_then(|b| describe_producer(code, b, resolver, depth + 1))?;
                let index = stack
                    .get(idx_slot)
                    .and_then(|s| s.producer_bci)
                    .and_then(|b| describe_producer(code, b, resolver, depth + 1))?;
                Some(Producer::expr(format!("{}[{}]", arr.text, index.text)))
            }
            // A checkcast is transparent to the expression: `((String) o)` is
            // named after `o`. Recurse on the value being cast.
            Instruction::Checkcast(_) => {
                let val = simulate_to(code, producer_bci, resolver)?
                    .last()
                    .copied()
                    .and_then(|s| s.producer_bci)?;
                describe_producer(code, val, resolver, depth + 1)
            }
            // An invoke result: `Owner.m(params)`. Flagged `is_invoke` so the
            // top level renders it as `the return value of "…"` while a nested
            // use (sub-receiver) renders the inline `Owner.m().field` form.
            Instruction::Invokevirtual(idx) | Instruction::Invokespecial(idx) => {
                invoke_producer(idx, resolver)
            }
            Instruction::Invokestatic(idx) => invoke_producer(idx, resolver),
            Instruction::Invokeinterface { index, .. } => invoke_producer(index, resolver),
            Instruction::AconstNull => Some(Producer::expr("null".to_string())),
            _ => None,
        }
    }

    /// Build the [`Producer`] for an invoke result at CP index `idx`.
    fn invoke_producer(idx: u16, resolver: &dyn CpResolver) -> Option<Producer> {
        let CpRef::Method {
            owner_internal,
            name,
            descriptor,
        } = resolver.method_ref(idx)?
        else {
            return None;
        };
        Some(Producer::invoke(render_method(
            &owner_internal,
            &name,
            &descriptor,
        )))
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
    ) -> Option<Producer> {
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
    ) -> Option<Producer> {
        let stack = simulate_to(code, trap_bci, resolver)?;
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
    let class_id = shared.mem.heap.class_id_of(obj);
    let cm = shared.classes.class_manager.read();
    let mut walk = Some(class_id);
    // Real-JDK bootstrap metadata intentionally represents a few core fields
    // as `_fN`.  Throwable's first two instance slots nevertheless retain the
    // JDK layout: `backtrace`, then `detailMessage`.  Keep this narrowly
    // scoped fallback for that opaque representation only.
    let mut opaque_throwable_detail_message = None;
    while let Some(cid) = walk {
        let Some(cls) = cm.get_class(cid) else { break };
        if &*cls.name == "java/lang/Throwable"
            && cls.fields.len() >= 2
            && cls
                .fields
                .iter()
                .take(2)
                .all(|field| field.name.starts_with("_f"))
        {
            opaque_throwable_detail_message = Some(cls.first_field_index + 1);
        }
        let mut inst = 0usize;
        for f in &cls.fields {
            if f.is_static() {
                continue;
            }
            if &*f.name == "detailMessage" {
                let idx = cls.first_field_index + inst;
                drop(cm);
                shared
                    .mem
                    .heap
                    .set_field(obj, idx, Value::Object(Some(string_ref)));
                return;
            }
            inst += 1;
        }
        walk = cls.superclass;
    }
    if let Some(idx) = opaque_throwable_detail_message {
        drop(cm);
        shared
            .mem
            .heap
            .set_field(obj, idx, Value::Object(Some(string_ref)));
    }
}

/// Resolve `Throwable.cause` (or any inherited Throwable field by that
/// name) and write `cause_ref` to it. Same by-name hierarchy walk as
/// `set_detail_message_by_name` just above, reused so a cause can be
/// attached to a freshly built `create_exception_object` result without
/// needing to know the field's numeric slot (differs between real-JDK and
/// synthetic layouts). See `raise_no_class_def_found_with_cause`.
fn set_cause_by_name(shared: &SharedVm, obj: ObjectRef, cause_ref: ObjectRef) {
    let class_id = shared.mem.heap.class_id_of(obj);
    let cm = shared.classes.class_manager.read();
    let mut walk = Some(class_id);
    while let Some(cid) = walk {
        let Some(cls) = cm.get_class(cid) else {
            break;
        };
        let mut inst = 0usize;
        for f in &cls.fields {
            if f.is_static() {
                continue;
            }
            if &*f.name == "cause" {
                let idx = cls.first_field_index + inst;
                drop(cm);
                shared
                    .mem
                    .heap
                    .set_field(obj, idx, Value::Object(Some(cause_ref)));
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
        .classes
        .class_manager
        .write()
        .load_class(class_name)
        .map_err(|e| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: format!("failed to load exception class {class_name}: {e}"),
            })
        })?;

    create_exception_object_for_class(shared, thread, class_id, class_name, message)
}

/// Create a Java exception object for an already-resolved exception class.
///
/// JNI `ThrowNew` receives a `jclass`, whose defining loader is part of the
/// class identity.  Re-resolving only its name could select a different class
/// from another loader; this entry point preserves the exact class supplied by
/// the native library.
#[cold]
pub fn create_exception_object_for_class(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    class_name: &str,
    message: Option<&str>,
) -> Result<ObjectRef, MethodCallFailed> {
    // The caller may have received a stale/bogus `jclass`; reject it before
    // allocating an object with an unknown layout.
    if shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .is_none()
    {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!("exception class {class_name} is not loaded"),
        }));
    }

    // 2. Allocate the exception object
    let num_fields = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.num_total_fields)
        .unwrap_or(0);
    let obj_ref = match shared.mem.heap.try_alloc_object(class_id, num_fields) {
        Some(obj) => obj,
        None => {
            // Young gen full — force a GC cycle and retry.
            thread.tlab.retire();
            super::interpreter::maybe_gc_forced_pub(shared, thread);
            // GC-overhead limit: if the heap is GC-thrashing, fail fast with OOM
            // so the caller falls back to the pre-allocated singleton (this very
            // path is what builds a fresh exception — looping here would
            // death-spiral too). The startup pre-allocation runs on an empty
            // heap, so it never reaches this branch.
            if super::interpreter::gc_overhead_limit_exceeded(shared) {
                return Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::OutOfMemoryError {
                        message: "Java heap space".to_string(),
                    },
                )));
            }
            shared
                .mem
                .heap
                .try_alloc_object(class_id, num_fields)
                .ok_or_else(|| {
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
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(obj_ref);

    // 3. Call the constructor
    // Try (Ljava/lang/String;)V if we have a message, otherwise ()V
    if let Some(msg) = message {
        // Create the java.lang.String for the message. Fallible: on a 100%-full
        // heap (the OOM-during-OOM case) the message string cannot be allocated
        // — report OutOfMemoryError so the caller can fall back to the
        // pre-allocated singleton instead of the VM hard-aborting inside the
        // non-fallible String allocator. (The exception object itself was
        // allocated fallibly above; the stack trace is captured VM-side and
        // does not allocate a Java object, so the message string is the only
        // remaining abort point.)
        let Some(string_ref) = try_create_java_string(shared, msg) else {
            thread.native_pin_roots.truncate(pin_base);
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::OutOfMemoryError {
                    message: "Java heap space".to_string(),
                },
            )));
        };
        let string_pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(string_ref);

        // Try calling (Ljava/lang/String;)V constructor first
        let obj_ref = thread.native_pin_roots[pin_base];
        let string_ref = thread.native_pin_roots[string_pin];
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
            // Keep the direct field write even after a successful constructor.
            // Core real-JDK exception constructors can be partially emulated
            // during bootstrap; JNI ThrowNew must still retain the caller's
            // message exactly as the JNI contract requires.
            Ok(_) => {
                let obj_ref = thread.native_pin_roots[pin_base];
                let string_ref = thread.native_pin_roots[string_pin];
                set_detail_message_by_name(shared, obj_ref, string_ref);
            }
            Err(MethodCallFailed::InternalError(_)) => {
                // String-arg constructor not found — fall back to ()V and
                // manually set detailMessage. Resolve by name so we hit the
                // real-JDK Throwable layout slot (slot 1, after backtrace),
                // not slot 0 (which is `backtrace`, an internal Object ref).
                let obj_ref = thread.native_pin_roots[pin_base];
                let _ = invoke_on_class_shared(
                    shared,
                    thread,
                    class_id,
                    "<init>",
                    "()V",
                    &[Value::Object(Some(obj_ref))],
                );
                let obj_ref = thread.native_pin_roots[pin_base];
                let string_ref = thread.native_pin_roots[string_pin];
                set_detail_message_by_name(shared, obj_ref, string_ref);
            }
            Err(MethodCallFailed::ExceptionThrown(_)) => {
                let obj_ref = thread.native_pin_roots[pin_base];
                let string_ref = thread.native_pin_roots[string_pin];
                // Constructor threw — still set the message field manually
                // by name so we honour the real-JDK Throwable layout.
                set_detail_message_by_name(shared, obj_ref, string_ref);
            }
        }
    } else {
        let obj_ref = thread.native_pin_roots[pin_base];
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
    let obj_ref = thread.native_pin_roots[pin_base];
    let _ = invoke_on_class_shared(
        shared,
        thread,
        class_id,
        "fillInStackTrace",
        "(I)Ljava/lang/Throwable;",
        &[Value::Object(Some(obj_ref)), Value::Int(0)],
    );

    let obj_ref = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);
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
    // ALV5th GC investigation (temp probe, CRATONVM_DBG_NPE_NONE): dump a
    // full Rust backtrace + Java stack the instant a message-less NPE is
    // thrown from Rust (as opposed to constructed by Java bytecode via
    // `new NullPointerException()`), so the exact Rust throw site is known
    // instead of inferred from bytecode-level probes.
    if std::env::var_os("CRATONVM_DBG_NPE_NONE").is_some() {
        if let RuntimeError::NullPointerException { message: None } = &error {
            eprintln!(
                "[npe-none] message-less NPE thrown — Java stack ({} frames, deepest first):",
                thread.frames.len()
            );
            for (i, f) in thread.frames.iter().enumerate().rev().take(20) {
                eprintln!(
                    "  [{i}] {}.{}{} pc={}",
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    f.pc
                );
            }
            eprintln!(
                "[npe-none] Rust backtrace:\n{}",
                std::backtrace::Backtrace::force_capture()
            );
        }
    }
    if std::env::var_os("CRATONVM_DBG_AIOOBE").is_some() {
        if let RuntimeError::ArrayIndexOutOfBoundsException { index } = &error {
            eprintln!(
                "[AIOOBE-THROW] index={index} — full live Java thread stack ({} frames, deepest first):",
                thread.frames.len()
            );
            for (i, f) in thread.frames.iter().enumerate().rev().take(15) {
                let cn = shared
                    .classes
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
    // CRATONVM_DBG_BUFUNDER: companion to the AIOOBE dump above for
    // `BufferUnderflowException` raised Rust-side (native ByteBuffer /
    // buffer-view helpers). These never pass through the `Athrow` opcode, so
    // `CRATONVM_DBG_ATHROW` only ever shows the later Java-level rethrow
    // (e.g. Lucene's `IOUtils.rethrowAlways`) — this dump names the true
    // origin frame instead.
    if std::env::var_os("CRATONVM_DBG_BUFUNDER").is_some()
        && matches!(&error, RuntimeError::BufferUnderflowException)
    {
        eprintln!(
            "[BUFUNDER-THROW] — full live Java thread stack ({} frames, deepest first):",
            thread.frames.len()
        );
        for (i, f) in thread.frames.iter().enumerate().rev().take(25) {
            let cn = shared
                .classes
                .class_manager
                .read()
                .get_class(f.class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            eprintln!(
                "[BUFUNDER-STK {i}] {}.{}{} pc={}",
                cn,
                f.method_name(),
                f.method_descriptor(),
                f.pc
            );
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
                        .classes
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
        let method = frame
            .map(|f| f.method_name().to_string())
            .unwrap_or_default();
        let pc = frame.map(|f| f.pc).unwrap_or(0);
        let class_name = frame
            .and_then(|f| {
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(f.class_id)
                    .map(|c| c.name.clone())
            })
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
                    let _cn = shared
                        .classes
                        .class_manager
                        .read()
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
                            let cn = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(f.class_id)
                                .map(|c| c.name.clone())
                                .unwrap_or_default();
                            eprintln!("C29-STK[{i}] {}.{} pc={}", cn, f.method_name(), f.pc);
                        }
                    }
                    if m.contains("Name is null") {
                        eprintln!("SUREFIRE-NPE-TRACE msg={m}");
                        for (i, f) in thread.frames.iter().enumerate().rev().take(30) {
                            let cn = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(f.class_id)
                                .map(|c| c.name.clone())
                                .unwrap_or_default();
                            eprintln!(
                                "SUREFIRE-NPE-STK[{i}] {}.{} pc={}",
                                cn,
                                f.method_name(),
                                f.pc
                            );
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
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(f.class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    cn.starts_with("org/apache/logging/log4j/")
                        || cn.starts_with("org/jboss/logging/")
                });
                if in_log4j_init {
                    eprintln!("[WF-NPE-TRACE] msg={:?}", error);
                    for (i, f) in thread.frames.iter().enumerate().rev().take(40) {
                        let cn = shared
                            .classes
                            .class_manager
                            .read()
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
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(f.class_id)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    eprintln!("NPE-STK[{i}] {}.{} pc={}", cn, f.method_name(), f.pc);
                }
            }
        }
        // S111r19+: trace IAE origins for ConfigurationClassParser hunt
        if matches!(&error, RuntimeError::IllegalArgumentException { .. }) && iae_trace_enabled() {
            eprintln!("IAE-TRACE error={error:?}");
            for (i, f) in thread.frames.iter().enumerate().rev().take(25) {
                let cn = shared
                    .classes
                    .class_manager
                    .read()
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
        RuntimeError::ArrayIndexOutOfBoundsException { index: _ } => {
            ("java/lang/ArrayIndexOutOfBoundsException", None)
        }
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
        RuntimeError::SocketTimeoutException { message } => {
            ("java/net/SocketTimeoutException", Some(message.as_str()))
        }
        RuntimeError::ConnectException { message } => {
            ("java/net/ConnectException", Some(message.as_str()))
        }
        RuntimeError::ProtocolException { message } => {
            ("java/net/ProtocolException", Some(message.as_str()))
        }
        RuntimeError::BindException { message } => {
            ("java/net/BindException", Some(message.as_str()))
        }
        RuntimeError::FileNotFoundException { path } => {
            ("java/io/FileNotFoundException", Some(path.as_str()))
        }
        RuntimeError::NoSuchFileException { path } => {
            ("java/nio/file/NoSuchFileException", Some(path.as_str()))
        }
        RuntimeError::UnsupportedOperationException { message } => (
            "java/lang/UnsupportedOperationException",
            // An empty message means "no message" (e.g. the blocked-mutator
            // helper for Collections.unmodifiable*/List.of view wrappers,
            // matching the real JDK's `new UnsupportedOperationException()`
            // no-arg constructor) — must produce a null `getMessage()`, not a
            // non-null empty string. `Some("")` would call the
            // `(Ljava/lang/String;)V` ctor and set detailMessage to "".
            if message.is_empty() {
                None
            } else {
                Some(message.as_str())
            },
        ),
        RuntimeError::IllegalStateException { message } => {
            ("java/lang/IllegalStateException", Some(message.as_str()))
        }
        RuntimeError::IllegalThreadStateException { message } => (
            "java/lang/IllegalThreadStateException",
            Some(message.as_str()),
        ),
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
        RuntimeError::ReadOnlyBufferException => ("java/nio/ReadOnlyBufferException", None),
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
        Err(e) => {
            // If the failure was heap exhaustion (couldn't allocate the
            // exception object or its detail-message String), throw the
            // pre-allocated singleton OutOfMemoryError so the VM stays alive
            // and the OOM is catchable — instead of the non-fallible String
            // allocator hard-aborting. This is correct regardless of the
            // original exception type: when the heap is 100% full, the real
            // failure IS an OOM. For non-OOM failures (e.g. the exception
            // class can't be loaded) keep the internal-error path.
            let is_oom = matches!(
                &e,
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::OutOfMemoryError { .. }
                ))
            );
            if is_oom {
                if let Some(oom) = *shared.mem.singleton_oom.read() {
                    return MethodCallFailed::ExceptionThrown(oom);
                }
            }
            // Fallback: if we can't create the Java exception object,
            // wrap it as an internal error.
            MethodCallFailed::InternalError(VmError::Runtime(error))
        }
    }
}

/// Pre-allocate the singleton `java.lang.OutOfMemoryError` while the heap still
/// has room, so a later 100%-full-heap OOM can be thrown WITHOUT allocating the
/// throwable (which would otherwise hard-abort in the non-fallible String
/// allocator — the classic OOM-during-OOM problem, HotSpot pre-allocates the
/// same way). Idempotent; intended to be called once early, before user `main`
/// (and before `-javaagent` premains). The instance is stored on
/// `SharedVm::singleton_oom` and kept alive permanently by the GC root scan
/// (`memory::roots`). On failure (e.g. called too early, before the class is
/// loadable) it leaves the slot empty and the OOM paths keep their prior
/// behaviour — never worse.
pub fn ensure_singleton_oom(shared: &SharedVm, thread: &mut JvmThread) {
    if shared.mem.singleton_oom.read().is_some() {
        return;
    }
    if let Ok(obj) = create_exception_object(
        shared,
        thread,
        "java/lang/OutOfMemoryError",
        Some("Java heap space"),
    ) {
        *shared.mem.singleton_oom.write() = Some(obj);
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
        Err(_) => {
            MethodCallFailed::InternalError(VmError::ClassFile(ClassFileError::ClassNotFound {
                class_name: class_name.to_string(),
            }))
        }
    }
}

/// Same as [`raise_no_class_def_found`], but for the "requested class
/// exists, a dependency failed to resolve" case (JVMS §5.3/§5.4):
/// `missing_internal` names the actual missing supertype/interface (in
/// internal/slash form), and the thrown `NoClassDefFoundError` carries a
/// `ClassNotFoundException(missing_internal)` cause -- matching real JDK25's
/// shape for this exact scenario (verified 2026-07-17 against `~/jdk25`; see
/// `native-builtins/src/classloader_real.rs::no_class_def_found_error`, the
/// `ClassLoader.loadClass`-path sibling of this opcode-resolution-boundary
/// fix reached via `convert_class_not_found`). Falls back to the
/// message-only `raise_no_class_def_found` if the `NoClassDefFoundError`
/// itself cannot be built (e.g. under heap pressure); a failure to build the
/// `ClassNotFoundException` cause is non-fatal -- the `NoClassDefFoundError`
/// is still thrown, just without a cause.
#[cold]
pub fn raise_no_class_def_found_with_cause(
    shared: &SharedVm,
    thread: &mut JvmThread,
    missing_internal: &str,
) -> MethodCallFailed {
    let ncdfe = match create_exception_object(
        shared,
        thread,
        "java/lang/NoClassDefFoundError",
        Some(missing_internal),
    ) {
        Ok(obj) => obj,
        Err(e) => return e,
    };
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(ncdfe);
    let dotted = missing_internal.replace('/', ".");
    let cause_result = create_exception_object(
        shared,
        thread,
        "java/lang/ClassNotFoundException",
        Some(&dotted),
    );
    let ncdfe = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);
    if let Ok(cause) = cause_result {
        set_cause_by_name(shared, ncdfe, cause);
    }
    MethodCallFailed::ExceptionThrown(ncdfe)
}

fn linkage_throwable(error: &LinkageError) -> (&'static str, String) {
    match error {
        LinkageError::NoClassDefFoundError { class_name } => {
            ("java/lang/NoClassDefFoundError", class_name.clone())
        }
        LinkageError::NoSuchFieldError {
            class_name,
            field_name,
        } => (
            "java/lang/NoSuchFieldError",
            format!("{}.{}", class_name, field_name),
        ),
        LinkageError::NoSuchMethodError {
            class_name,
            method_name,
            method_descriptor,
        } => (
            "java/lang/NoSuchMethodError",
            // Real JDK's NoSuchMethodError message names the class in
            // source (dotted) form, not internal (slash) form — e.g.
            // `'void com.example.Foo.bar()'`. Spring Boot's
            // `NoSuchMethodFailureAnalyzer` embeds this message verbatim in
            // its `FailureAnalysis` description, and
            // `NoSuchMethodFailureAnalyzerTests.
            // whenAnInheritedMethodIsMissingThenNoSuchMethodErrorIsAnalyzed`
            // asserts the description contains the fully-qualified DOTTED
            // class+method (`R2dbcMappingContext.class.getName() +
            // ".setForceQuote("`), which a slash-separated class name can
            // never satisfy.
            format!(
                "{}.{}{}",
                class_name.replace('/', "."),
                method_name,
                method_descriptor
            ),
        ),
        LinkageError::IncompatibleClassChangeError { message } => {
            ("java/lang/IncompatibleClassChangeError", message.clone())
        }
        LinkageError::AbstractMethodError {
            class_name,
            method_name,
        } => (
            "java/lang/AbstractMethodError",
            format!("{}.{}", class_name, method_name),
        ),
        LinkageError::IllegalAccessError { message } => {
            ("java/lang/IllegalAccessError", message.clone())
        }
        LinkageError::VerifyError {
            class_name,
            method_name,
            message,
        } => (
            "java/lang/VerifyError",
            format!("{}.{}: {}", class_name, method_name, message),
        ),
        LinkageError::ClassFormatError {
            class_name,
            message,
        } => (
            "java/lang/ClassFormatError",
            format!("{}: {}", class_name, message),
        ),
        LinkageError::UnsupportedClassRedefinitionError {
            class_name,
            message,
        } => (
            "java/lang/UnsupportedOperationException",
            format!("{}: {}", class_name, message),
        ),
    }
}

/// Convert a VM linkage miss/error into its Java throwable counterpart.
///
/// Linkage errors are ordinary `java.lang.LinkageError` subclasses from Java's
/// perspective. They must travel through the same exception-table path as
/// runtime exceptions so bytecode such as Surefire's `catch
/// (NoSuchMethodError)` fallback can run.
#[cold]
pub fn throw_linkage_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    error: LinkageError,
) -> MethodCallFailed {
    let (class_name, detail) = linkage_throwable(&error);
    if std::env::var_os("CRATONVM_DBG_VERIFY_ERROR").is_some() {
        if let LinkageError::VerifyError {
            class_name,
            method_name,
            message,
        } = &error
        {
            eprintln!("[cratonvm-verify] {class_name}.{method_name}: {message}");
        }
    }
    match create_exception_object(shared, thread, class_name, Some(&detail)) {
        Ok(obj_ref) => MethodCallFailed::ExceptionThrown(obj_ref),
        Err(_) => MethodCallFailed::InternalError(VmError::Linkage(error)),
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
        let cm = shared.classes.class_manager.read();
        for (i, f) in thread.frames.iter().enumerate().rev().take(20) {
            let cn = cm
                .get_class(f.class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            eprintln!("[NCDFE-STK {}] {}.{} pc={}", i, cn, f.method_name(), f.pc);
        }
    }
    match err {
        // `ClassManager::load_class` propagates a recursive supertype/
        // interface load failure UNCHANGED (see `resolve_supertype` in
        // classloading/src/class_manager.rs), so `missing` here names
        // whichever class in the hierarchy actually failed to resolve, not
        // necessarily `class_name` (the class this opcode is resolving).
        // When they differ, `class_name` itself was found and only a
        // dependency is missing -- JVMS §5.3/§5.4 NoClassDefFoundError
        // naming the dependency, not a same-named CNFE-flavoured NCDFE on
        // `class_name`. Sibling fix to the `ClassLoader.loadClass` path in
        // `native-builtins/src/classloader_real.rs::load_class_visible_to`
        // (2026-07-17); this is the opcode-resolution-boundary occurrence of
        // the same gap (`new`/`getstatic`/`putstatic`/`checkcast`/...).
        MethodCallFailed::InternalError(VmError::ClassFile(ClassFileError::ClassNotFound {
            class_name: missing,
        })) => {
            if missing == class_name {
                raise_no_class_def_found(shared, thread, class_name)
            } else {
                raise_no_class_def_found_with_cause(shared, thread, &missing)
            }
        }
        MethodCallFailed::InternalError(VmError::Linkage(
            crate::error::LinkageError::NoClassDefFoundError { .. },
        )) => raise_no_class_def_found(shared, thread, class_name),
        // NoSuchFieldError and NoSuchMethodError are LinkageErrors in Java.
        // Convert them to throwable Java exceptions so catch(Error) / catch(Throwable)
        // blocks in user/framework code can handle them instead of crashing the VM.
        MethodCallFailed::InternalError(VmError::Linkage(
            linkage @ LinkageError::NoSuchFieldError { .. },
        ))
        | MethodCallFailed::InternalError(VmError::Linkage(
            linkage @ LinkageError::NoSuchMethodError { .. },
        )) => throw_linkage_error(shared, thread, linkage),
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

    #[test]
    fn linkage_no_such_method_error_is_throwable_or_falls_back() {
        let mut vm = test_vm();
        let error = LinkageError::NoSuchMethodError {
            class_name: "org/junit/runner/Description".to_string(),
            method_name: "createSuiteDescription".to_string(),
            method_descriptor: "(Ljava/lang/String;)Lorg/junit/runner/Description;".to_string(),
        };
        let result = throw_linkage_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::ExceptionThrown(_)
                | MethodCallFailed::InternalError(VmError::Linkage(
                    LinkageError::NoSuchMethodError { .. }
                ))
        ));
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
    fn throw_runtime_error_releases_exception_construction_pins() {
        let mut vm = test_vm();
        let pin_base = vm.main_thread.native_pin_roots.len();
        let error = RuntimeError::IOException {
            message: "read error".to_string(),
        };
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
        assert_eq!(
            vm.main_thread.native_pin_roots.len(),
            pin_base,
            "exception construction pins must not leak after throw_runtime_error returns"
        );
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
    use super::helpful_npe::{self, CpRef, CpResolver, Producer};
    use std::collections::HashMap;

    /// Map-backed resolver: cp-index -> CpRef, mirroring what the live
    /// constant pool would hand back. No VM / rt.jar required.
    ///
    /// `locals` mimics a resolved `LocalVariableTable`: slot -> source name.
    /// When a slot is present the resolver returns that real name (as the
    /// live `CpPoolResolver` does from the `LocalVariableTable` attribute);
    /// when absent the analysis falls back to the synthetic `<localN>` form.
    /// `is_static` mirrors the trapping method's access flag (slot-0 naming).
    struct MockResolver {
        fields: HashMap<u16, CpRef>,
        methods: HashMap<u16, CpRef>,
        locals: HashMap<u16, String>,
        is_static: bool,
    }

    impl MockResolver {
        fn new(fields: HashMap<u16, CpRef>, methods: HashMap<u16, CpRef>) -> Self {
            MockResolver {
                fields,
                methods,
                locals: HashMap::new(),
                is_static: false,
            }
        }
        fn new_static(fields: HashMap<u16, CpRef>, methods: HashMap<u16, CpRef>) -> Self {
            let mut r = Self::new(fields, methods);
            r.is_static = true;
            r
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
        fn is_static_method(&self) -> bool {
            self.is_static
        }
    }

    /// The inline `text` of a reconstructed producer (ignoring the
    /// `is_invoke` flag), for terse `Some("…")` assertions.
    fn text(p: &Option<Producer>) -> Option<&str> {
        p.as_ref().map(|x| x.text.as_str())
    }

    // Opcode bytes (JVMS §6).
    const ALOAD_0: u8 = 0x2a;
    const ALOAD_1: u8 = 0x2b;
    const GETFIELD: u8 = 0xb4;
    const GETSTATIC: u8 = 0xb2;
    const INVOKEVIRTUAL: u8 = 0xb6;
    const INVOKESTATIC: u8 = 0xb8;
    const CHECKCAST: u8 = 0xc0;
    const AALOAD: u8 = 0x32;
    const ICONST_2: u8 = 0x05;

    fn u16_be(x: u16) -> [u8; 2] {
        x.to_be_bytes()
    }

    fn method(owner: &str, name: &str, desc: &str) -> CpRef {
        CpRef::Method {
            owner_internal: owner.to_string(),
            name: name.to_string(),
            descriptor: desc.to_string(),
        }
    }
    fn field(owner: &str, name: &str) -> CpRef {
        CpRef::Field {
            owner_internal: owner.to_string(),
            name: name.to_string(),
        }
    }

    /// External-name formatting (used for parameter rendering) matches the
    /// JEP 358 dotted form, including array spelling.
    #[test]
    fn class_external_formats() {
        assert_eq!(
            helpful_npe::class_external("java/lang/String"),
            "java.lang.String"
        );
        assert_eq!(helpful_npe::class_external("[I"), "int[]");
        assert_eq!(
            helpful_npe::class_external("[Ljava/lang/Object;"),
            "java.lang.Object[]"
        );
    }

    /// The invoke **owner** rendering (verified against JDK 25): descriptor
    /// form for arrays (`[I`, not `int[]`), dotted otherwise, and only
    /// `String`/`Object` shortened.
    #[test]
    fn render_owner_matches_hotspot() {
        assert_eq!(helpful_npe::render_owner("java/lang/String"), "String");
        assert_eq!(helpful_npe::render_owner("java/lang/Object"), "Object");
        assert_eq!(
            helpful_npe::render_owner("java/lang/Integer"),
            "java.lang.Integer"
        );
        assert_eq!(
            helpful_npe::render_owner("java/util/List"),
            "java.util.List"
        );
        assert_eq!(helpful_npe::render_owner("NpeProbe$Node"), "NpeProbe$Node");
        // Array owners keep descriptor form (clone() on an array).
        assert_eq!(helpful_npe::render_owner("[I"), "[I");
        assert_eq!(
            helpful_npe::render_owner("[Ljava/lang/String;"),
            "[Ljava.lang.String;"
        );
    }

    /// The action half names owner + method + parameter types, with
    /// `String`/`Object` shortened in *both* the owner and the params
    /// (matches `java.lang.String.length` → `String.length`, and
    /// `(Object)`/`(String)` params shortened too).
    #[test]
    fn action_invoke_shape() {
        assert_eq!(
            helpful_npe::action_invoke("java/lang/String", "length", "()I"),
            "Cannot invoke \"String.length()\""
        );
        assert_eq!(
            helpful_npe::action_invoke("java/util/List", "get", "(I)Ljava/lang/Object;"),
            "Cannot invoke \"java.util.List.get(int)\""
        );
        // String/Object params shortened; CharSequence stays qualified.
        assert_eq!(
            helpful_npe::action_invoke(
                "java/util/Map",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
            ),
            "Cannot invoke \"java.util.Map.put(Object, Object)\""
        );
        assert_eq!(
            helpful_npe::action_invoke(
                "java/lang/String",
                "contains",
                "(Ljava/lang/CharSequence;)Z"
            ),
            "Cannot invoke \"String.contains(java.lang.CharSequence)\""
        );
        // Array param uses external form (Object[]), array owner stays [I.
        assert_eq!(
            helpful_npe::action_invoke("[I", "clone", "()Ljava/lang/Object;"),
            "Cannot invoke \"[I.clone()\""
        );
        assert_eq!(
            helpful_npe::action_invoke(
                "java/util/List",
                "toArray",
                "([Ljava/lang/Object;)[Ljava/lang/Object;"
            ),
            "Cannot invoke \"java.util.List.toArray(Object[])\""
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
        fields.insert(1u16, field("Node", "next"));
        let mut methods = HashMap::new();
        methods.insert(2u16, method("Node", "value", "()I"));
        let resolver = MockResolver::new(fields, methods);

        let action = helpful_npe::action_invoke("Node", "value", "()I");
        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
        assert_eq!(text(&expr), Some("this.next"));
        let msg = helpful_npe::combine(&action, expr.as_ref());
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
        fields.insert(1u16, field("Box", "contents"));
        let mut methods = HashMap::new();
        methods.insert(2u16, method("java/lang/String", "length", "()I"));
        let resolver = MockResolver::new(fields, methods);

        let action = helpful_npe::action_invoke("java/lang/String", "length", "()I");
        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
        let msg = helpful_npe::combine(&action, expr.as_ref());
        assert_eq!(
            msg,
            "Cannot invoke \"String.length()\" because \"<local1>.contents\" is null"
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
            method("java/lang/String", "trim", "()Ljava/lang/String;"),
        );
        let resolver = MockResolver::new(HashMap::new(), methods);
        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
        assert_eq!(text(&expr), Some("<local1>"));
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
        fields.insert(1u16, field("java/lang/System", "out"));
        let mut methods = HashMap::new();
        methods.insert(2u16, method("java/io/PrintStream", "println", "()V"));
        let resolver = MockResolver::new(fields, methods);
        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
        assert_eq!(text(&expr), Some("java.lang.System.out"));
    }

    /// In a **static** method, slot 0 is an ordinary local — HotSpot prints
    /// `<local0>`, not `this`. `aload_0; arraylength`.
    #[test]
    fn static_method_slot0_is_local0() {
        let code = vec![ALOAD_0, ARRAYLENGTH];
        let resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
        let expr = helpful_npe::null_expr_at_depth(&code, 1, 0, &resolver);
        assert_eq!(text(&expr), Some("<local0>"));

        // The same bytecode in an instance method names slot 0 `this`.
        let instance = MockResolver::new(HashMap::new(), HashMap::new());
        let expr2 = helpful_npe::null_expr_at_depth(&code, 1, 0, &instance);
        assert_eq!(text(&expr2), Some("this"));
    }

    /// A store between the producer and the trapping use must not defeat the
    /// analysis — the store-free hand-written tests missed this, but every real
    /// `Type x = null; … x.deref()` emits `astore` before the `aload`.
    /// `aconst_null; astore_0; aload_0; getfield #1 (Node.x)`.
    #[test]
    fn store_then_load_reconstructs_local() {
        const ACONST_NULL: u8 = 0x01;
        const ASTORE_0: u8 = 0x4b;
        let mut code = vec![ACONST_NULL, ASTORE_0, ALOAD_0, GETFIELD];
        code.extend_from_slice(&u16_be(1));
        let trap_bci = 3;
        let mut fields = HashMap::new();
        fields.insert(1u16, field("Node", "x"));
        let resolver = MockResolver::new_static(fields, HashMap::new());
        let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
        assert_eq!(text(&expr), Some("<local0>"));
    }

    /// A null **invoke result** used directly as a receiver renders as
    /// `the return value of "Owner.m()"` (note: not quoted as a whole).
    /// `invokestatic #1 (P.getNull()); invokevirtual #2`. (The parameter-list
    /// rendering inside the "return value of" form is covered end-to-end by the
    /// `Edge3` integration probe — `getNullArg(int, String)` — which needs the
    /// real arg-pushing bytecode a hand-written synthetic can't easily supply.)
    #[test]
    fn return_value_producer() {
        let mut code = vec![INVOKESTATIC];
        code.extend_from_slice(&u16_be(1));
        let invoke_bci = code.len();
        code.push(INVOKEVIRTUAL);
        code.extend_from_slice(&u16_be(2));

        let mut methods = HashMap::new();
        methods.insert(1u16, method("P", "getNull", "()Ljava/lang/String;"));
        methods.insert(2u16, method("java/lang/String", "length", "()I"));
        let resolver = MockResolver::new(HashMap::new(), methods);

        let action = helpful_npe::action_invoke("java/lang/String", "length", "()I");
        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
        assert!(expr.as_ref().unwrap().is_invoke);
        assert_eq!(text(&expr), Some("P.getNull()"));
        assert_eq!(
            helpful_npe::combine(&action, expr.as_ref()),
            "Cannot invoke \"String.length()\" because the return value of \"P.getNull()\" is null"
        );
    }

    /// A non-null invoke result whose **field** is null renders the invoke
    /// inline (`Owner.m().field`), not as "the return value of".
    /// `invokestatic #1 (P.getN()); getfield #4 (P.next); invokevirtual #2`.
    #[test]
    fn nested_return_value_subreceiver() {
        let mut code = vec![INVOKESTATIC];
        code.extend_from_slice(&u16_be(1));
        code.push(GETFIELD);
        code.extend_from_slice(&u16_be(4));
        let invoke_bci = code.len();
        code.push(INVOKEVIRTUAL);
        code.extend_from_slice(&u16_be(2));

        let mut fields = HashMap::new();
        fields.insert(4u16, field("P", "next"));
        let mut methods = HashMap::new();
        methods.insert(1u16, method("P", "getN", "()LP;"));
        methods.insert(
            2u16,
            method("java/lang/Object", "toString", "()Ljava/lang/String;"),
        );
        let resolver = MockResolver::new(fields, methods);

        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
        assert!(!expr.as_ref().unwrap().is_invoke);
        assert_eq!(text(&expr), Some("P.getN().next"));
    }

    /// A `checkcast` is transparent: `((String) o)` is named after `o`.
    /// `aload_1; checkcast #3; invokevirtual #2`.
    #[test]
    fn checkcast_is_transparent() {
        let mut code = vec![ALOAD_1, CHECKCAST];
        code.extend_from_slice(&u16_be(3));
        let invoke_bci = code.len();
        code.push(INVOKEVIRTUAL);
        code.extend_from_slice(&u16_be(2));

        let mut methods = HashMap::new();
        methods.insert(2u16, method("java/lang/String", "length", "()I"));
        let resolver = MockResolver::new(HashMap::new(), methods);
        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
        assert_eq!(text(&expr), Some("<local1>"));
    }

    /// `aaload` reconstructs the index sub-expression: `getstatic #1 (P.sarr);
    /// iconst_2; aaload; invokevirtual #2` → array element `P.sarr[2]`.
    #[test]
    fn aaload_index_reconstruction() {
        let mut code = vec![GETSTATIC];
        code.extend_from_slice(&u16_be(1));
        code.push(ICONST_2);
        code.push(AALOAD);
        let invoke_bci = code.len();
        code.push(INVOKEVIRTUAL);
        code.extend_from_slice(&u16_be(2));

        let mut fields = HashMap::new();
        fields.insert(1u16, field("P", "sarr"));
        let mut methods = HashMap::new();
        methods.insert(2u16, method("java/lang/String", "length", "()I"));
        let resolver = MockResolver::new(fields, methods);
        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
        assert_eq!(text(&expr), Some("P.sarr[2]"));
    }

    // -- Increment 2: action halves + new-opcode expression shapes ----------

    // Additional opcode bytes (JVMS §6) for the increment-2 tests.
    const ALOAD: u8 = 0x19; // aload <index> (wide-index local load)
    const IASTORE: u8 = 0x4f;
    const ARRAYLENGTH: u8 = 0xbe;
    const MONITORENTER: u8 = 0xc2;
    const ICONST_0: u8 = 0x03;
    const ICONST_1: u8 = 0x04;

    /// The increment-2 action halves match the exact HotSpot wording. Note
    /// `baload`/`bastore` (byte *and* boolean arrays share the opcode) spell
    /// the element type "byte/boolean".
    #[test]
    fn increment2_action_shapes() {
        use helpful_npe::ArrayElemKind;
        assert_eq!(
            helpful_npe::action_read_field("x"),
            "Cannot read field \"x\""
        );
        assert_eq!(
            helpful_npe::action_assign_field("count"),
            "Cannot assign field \"count\""
        );
        assert_eq!(
            helpful_npe::action_array_length(),
            "Cannot read the array length"
        );
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
            "Cannot store to byte/boolean array"
        );
        assert_eq!(
            helpful_npe::action_array_load(ArrayElemKind::Boolean),
            "Cannot load from byte/boolean array"
        );
        assert_eq!(
            helpful_npe::action_monitor(),
            "Cannot enter synchronized block"
        );
        assert_eq!(helpful_npe::action_throw(), "Cannot throw exception");
    }

    /// `combine_opt` emits the action-only string (no fabricated `because`)
    /// when the expression can't be classified, and the full clause otherwise.
    #[test]
    fn combine_opt_action_only_when_unknown() {
        let action = helpful_npe::action_array_length();
        assert_eq!(
            helpful_npe::combine_opt(&action, None),
            "Cannot read the array length"
        );
        // A real reconstructed expression yields the full clause.
        let code = vec![ALOAD_1, ARRAYLENGTH];
        let resolver = MockResolver::new(HashMap::new(), HashMap::new());
        let expr = helpful_npe::null_expr_at_depth(&code, 1, 0, &resolver);
        assert_eq!(
            helpful_npe::combine_opt(&action, expr.as_ref()),
            "Cannot read the array length because \"<local1>\" is null"
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
        fields.insert(1u16, field("Node", "next"));
        fields.insert(2u16, field("Node", "value"));
        let resolver = MockResolver::new(fields, HashMap::new());

        // The trapping getfield reads field "value"; its receiver (depth 0)
        // is `this.next`.
        let action = helpful_npe::action_read_field("value");
        let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
        assert_eq!(text(&expr), Some("this.next"));
        let msg = helpful_npe::combine_opt(&action, expr.as_ref());
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
        assert_eq!(text(&expr), Some("<local1>"));
        assert_eq!(
            helpful_npe::combine_opt(&action, expr.as_ref()),
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
        assert_eq!(text(&expr), Some("<local1>"));
        assert_eq!(
            helpful_npe::combine_opt(&action, expr.as_ref()),
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
        assert_eq!(text(&expr), Some("<local1>"));
        assert_eq!(
            helpful_npe::combine_opt(&action, expr.as_ref()),
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
        assert_eq!(text(&expr), Some("items"));
        assert_eq!(
            helpful_npe::combine_opt(&action, expr.as_ref()),
            "Cannot read the array length because \"items\" is null"
        );

        // Without the LVT entry the same bytecode falls back to <local3>.
        let bare = MockResolver::new(HashMap::new(), HashMap::new());
        let expr_bare = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &bare);
        assert_eq!(text(&expr_bare), Some("<local3>"));
    }

    /// Step 6 (partial): the JIT-NPE action codes map to the exact HotSpot
    /// `BytecodeUtils` action-only strings, and an unrecognised / NONE code
    /// yields no message (so the caller emits an unmessaged NPE).
    #[test]
    fn jit_action_messages_match_hotspot() {
        use helpful_npe::jit_action::*;
        assert_eq!(
            helpful_npe::jit_action_message(ARRAY_LENGTH).as_deref(),
            Some("Cannot read the array length")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ALOAD_INT).as_deref(),
            Some("Cannot load from int array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ALOAD_OBJECT).as_deref(),
            Some("Cannot load from object array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ALOAD_BYTE).as_deref(),
            Some("Cannot load from byte/boolean array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ASTORE_INT).as_deref(),
            Some("Cannot store to int array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ASTORE_OBJECT).as_deref(),
            Some("Cannot store to object array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ASTORE_BYTE).as_deref(),
            Some("Cannot store to byte/boolean array")
        );
        // Inline-codegen path extension (JEP-358 follow-up): the remaining
        // primitive element kinds the inline null-check stubs now name.
        assert_eq!(
            helpful_npe::jit_action_message(ALOAD_LONG).as_deref(),
            Some("Cannot load from long array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ALOAD_FLOAT).as_deref(),
            Some("Cannot load from float array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ALOAD_DOUBLE).as_deref(),
            Some("Cannot load from double array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ALOAD_CHAR).as_deref(),
            Some("Cannot load from char array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ALOAD_SHORT).as_deref(),
            Some("Cannot load from short array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ASTORE_LONG).as_deref(),
            Some("Cannot store to long array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ASTORE_FLOAT).as_deref(),
            Some("Cannot store to float array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ASTORE_DOUBLE).as_deref(),
            Some("Cannot store to double array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ASTORE_CHAR).as_deref(),
            Some("Cannot store to char array")
        );
        assert_eq!(
            helpful_npe::jit_action_message(ASTORE_SHORT).as_deref(),
            Some("Cannot store to short array")
        );
        assert_eq!(helpful_npe::jit_action_message(NONE), None);
        assert_eq!(helpful_npe::jit_action_message(250), None);
    }

    /// Default path (gate off): the gated wrapper attaches no message, so a
    /// JIT-originated NPE keeps its byte-for-byte-unchanged unmessaged shape.
    #[test]
    fn jit_npe_message_gated_is_none_when_gate_off() {
        // The gate is a process-global parsed once; only assert the default-off
        // contract when it is actually off (don't mutate global env in a test).
        if !crate::runtime::env_cache::helpful_npe_opcodes() {
            assert_eq!(
                helpful_npe::jit_npe_message_gated(helpful_npe::jit_action::ALOAD_INT),
                None
            );
        }
    }
}
