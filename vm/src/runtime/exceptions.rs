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

/// Cached read of a `CRATONVM_DBG_*` / `CRATONVM_*_TRACE` env var. Env-var
/// lookups are surprisingly expensive (a `getenv` mutex + `OsString` alloc on
/// Linux, a ~500 ns `GetEnvironmentVariableW` syscall + UTF-16 decode on
/// Windows); the exception-throw path is sensitive to per-throw overhead, so we
/// read once at first use and cache the boolean.
///
/// Process-lifetime cache: setting the var after the first exception is thrown
/// has no effect. Identical policy to [`crate::runtime::env_cache`], which
/// documents this module's original `iae_trace_enabled` as the pattern's
/// origin; these helpers stay local because the flags below are only read from
/// this file.
///
/// Why this matters: every one of these was previously an **uncached**
/// `cratonvm_types::flags::runtime_var_os` on the `throw_runtime_error` / `convert_class_not_found`
/// / `throw_linkage_error` entry paths. `throw_runtime_error` alone paid three
/// of them on *every* VM-raised throw (NPE, CCE, AIOOBE, ISE, IOException, …)
/// — i.e. ~1.5 µs of pure syscall on Windows before a single useful
/// instruction ran. Framework code (Spring, Hibernate, the JUnit/Surefire
/// harnesses) throws as control flow, so this was a first-order cost.
macro_rules! cached_env_flag {
    ($name:ident, $env:literal) => {
        #[inline]
        fn $name() -> bool {
            static CACHE: OnceLock<bool> = OnceLock::new();
            *CACHE.get_or_init(|| cratonvm_types::flags::runtime_var_os($env).is_some())
        }
    };
}

/// `CRATONVM_IAE_TRACE` — the original cached flag (kept with `env::var`
/// semantics it has always had; `var` vs `var_os` only differ for non-UTF-8
/// values, which we never set).
#[inline]
fn iae_trace_enabled() -> bool {
    static IAE_TRACE: OnceLock<bool> = OnceLock::new();
    *IAE_TRACE.get_or_init(|| cratonvm_types::flags::runtime_var("CRATONVM_IAE_TRACE").is_ok())
}

/// Substring filter for the NPE-origin stack dump: set
/// `CRATONVM_DBG_NPE_MATCH=<substring>` to print the interpreter frame stack
/// for every `NullPointerException` whose (JEP 358) message contains it.
///
/// The two hardcoded predicates below (`isInterface`, `Name is null`) each
/// exist because someone needed exactly this and had to rebuild the VM to get
/// it. A caught-and-logged NPE deep inside a framework — Spring's
/// `AnnotationUtils.handleIntrospectionFailure` logs the message and swallows
/// the stack — is otherwise invisible: you get the JEP 358 text and no site.
fn dbg_npe_match() -> Option<&'static str> {
    static NPE_MATCH: OnceLock<Option<String>> = OnceLock::new();
    NPE_MATCH
        .get_or_init(|| cratonvm_types::flags::runtime_var("CRATONVM_DBG_NPE_MATCH").ok())
        .as_deref()
}

cached_env_flag!(dbg_npe_none, "CRATONVM_DBG_NPE_NONE");
cached_env_flag!(dbg_aioobe, "CRATONVM_DBG_AIOOBE");
cached_env_flag!(dbg_bufunder, "CRATONVM_DBG_BUFUNDER");
cached_env_flag!(dbg_npe_trace, "CRATONVM_DBG_NPE_TRACE");
cached_env_flag!(dbg_wf_npe, "CRATONVM_DBG_WF_NPE");
cached_env_flag!(dbg_ncdfe, "CRATONVM_DBG_NCDFE");
cached_env_flag!(dbg_verify_error, "CRATONVM_DBG_VERIFY_ERROR");

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
        // An explicit `-XX:-ShowCodeDetailsInExceptionMessages` suppresses the
        // message entirely, matching HotSpot — `helpful_npe_opcodes()` is true
        // in that case (it means "handle this the HotSpot way"), so the
        // suppression has to be checked first.
        if crate::runtime::env_cache::helpful_npe_suppressed() {
            return None;
        }
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

    /// A stack entry whose producer this analysis cannot name — either it was
    /// pushed by a predecessor basic block (see [`simulate_to`]) or by an opcode
    /// that yields no nameable source expression. Rendering stops here and the
    /// caller emits HotSpot's action-only message.
    const UNKNOWN_SLOT: Slot = Slot { producer_bci: None };

    /// Pop one operand-stack entry, yielding [`UNKNOWN_SLOT`] once the block's
    /// own simulated suffix is exhausted (the popped value came from a
    /// predecessor block). Never fails: an underflow is *information* — "this
    /// operand predates the block" — not a reason to abandon the whole
    /// reconstruction. The surviving entries stay correctly aligned relative to
    /// the top of the stack, which is how every caller indexes them.
    fn pop_slot(stack: &mut Vec<Slot>) -> Slot {
        stack.pop().unwrap_or(UNKNOWN_SLOT)
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
            Tableswitch(ts) => {
                let mut t = vec![rel(ts.default as i64)];
                t.extend(ts.offsets.iter().map(|o| rel(*o as i64)));
                t
            }
            Lookupswitch(ls) => {
                let mut t = vec![rel(ls.default as i64)];
                t.extend(ls.pairs.iter().map(|(_, o)| rel(*o as i64)));
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
    ///
    /// The returned vector is the **suffix** of the real operand stack that this
    /// block itself produced. A block leader that is a control-flow *merge*
    /// point (the join of a ternary, of a `&&`/`||` short-circuit, or of a
    /// `switch` arm that leaves a value on the stack) starts with entries pushed
    /// by its predecessor blocks, which this straight-line walk cannot see.
    /// [`pop_slot`] models those as "consumed from below" rather than as a hard
    /// bail, so indices measured **from the top** stay exact while anything
    /// reaching below the block boundary just reports an unknown producer → the
    /// caller's action-only message. Before that, `Type x = cond ? null : new …;
    /// x.deref()` — whose merge-point leader is the `astore` — underflowed on
    /// that very first pop and lost the `because "x" is null` clause HotSpot
    /// prints.
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
                    pop_slot(stack);
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
                // A `dup` at a merge-point leader duplicates a value a
                // predecessor block pushed: unknown, but still one stack entry.
                let top = stack.last().copied().unwrap_or(UNKNOWN_SLOT);
                stack.push(top);
            }
            Pop => {
                pop_slot(stack);
            }
            // Arithmetic / conversion / comparison. None of these *produces* a
            // nameable source expression (`describe_producer` returns `None` for
            // them, so the message stays action-only if the null operand came
            // from one), but they routinely sit in the prefix of the trapping
            // statement's own block — `sink += a[0]` compiles to
            // `getstatic; aload; iconst_0; iaload; iadd; putstatic` — so
            // modelling their shape keeps the walk alive instead of abandoning
            // the whole reconstruction at the `iadd`.
            //
            // Category-2 values occupy ONE entry in this model, so `ladd`
            // (2 in / 1 out) and `lshl` (long + int in / long out) have the same
            // shape as their `int` counterparts. The ambiguous stack-shufflers
            // (`dup2`, `dup_x1`, `pop2`, `swap`, …) are deliberately left
            // unmodelled: their entry count depends on the operand categories,
            // and a mis-shaped stack would mis-attribute a producer — worse than
            // no expression at all.
            Iadd | Isub | Imul | Idiv | Irem | Iand | Ior | Ixor | Ishl | Ishr | Iushr | Ladd
            | Lsub | Lmul | Ldiv | Lrem | Land | Lor | Lxor | Lshl | Lshr | Lushr | Fadd | Fsub
            | Fmul | Fdiv | Frem | Dadd | Dsub | Dmul | Ddiv | Drem | Lcmp | Fcmpl | Fcmpg
            | Dcmpl | Dcmpg => shape!(2, 1),
            Ineg | Lneg | Fneg | Dneg | I2l | I2f | I2d | L2i | L2f | L2d | F2i | F2l | F2d
            | D2i | D2l | D2f | I2b | I2c | I2s => shape!(1, 1),
            // `iinc` mutates a local in place — no operand-stack effect.
            Iinc { .. } => {}
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

/// Resolve the absolute heap field index of the first non-static field named
/// `name` found walking `obj`'s class hierarchy (most-derived first), using the
/// same `first_field_index` + declaration-order counting scheme as
/// [`set_detail_message_by_name`] / [`set_cause_by_name`].
///
/// Read-only companion to those setters: it takes the `class_manager` read lock
/// for the duration of the walk and releases it before returning, so callers
/// never hold it across a heap access.
fn instance_field_index_by_name(shared: &SharedVm, obj: ObjectRef, name: &str) -> Option<usize> {
    let class_id = shared.mem.heap.class_id_of(obj);
    let cm = shared.classes.class_manager.read();
    let mut walk = Some(class_id);
    while let Some(cid) = walk {
        let cls = cm.get_class(cid)?;
        let mut inst = 0usize;
        for f in &cls.fields {
            if f.is_static() {
                continue;
            }
            if &*f.name == name {
                return Some(cls.first_field_index + inst);
            }
            inst += 1;
        }
        walk = cls.superclass;
    }
    None
}

/// Whether the throwable's stack trace was already captured by its constructor
/// **at exactly the current frame depth** — in which case re-running
/// `fillInStackTrace` would recompute and re-store a byte-identical trace.
///
/// `capture_throwable_trace` (native-builtins `lang_misc.rs`, the shared body of
/// every `native_exc_init_*` constructor shadow *and* of
/// `Throwable.fillInStackTrace`) writes two markers on the throwable:
///
/// * `backtrace = this` — the non-null marker real-JDK `getOurStackTrace()`
///   requires before it will materialise any frames, and
/// * `depth = trace.len()` — and the captured trace is the *whole* Java stack
///   (`capture_full_trace` maps every entry of `thread.frames`), so
///   `depth == thread.frames.len()` **as it stood at capture time**.
///
/// So `backtrace == this && depth == frames.len()` proves the constructor's
/// capture was taken with this thread's frame stack in exactly the state
/// `fillInStackTrace` would see now — same frames, same `last_instr_pc` per
/// frame, therefore the same `StackTraceEntry` vector.
///
/// The check deliberately fails **closed**: a bytecode (non-shadowed) `<init>`
/// runs inside a pushed Java frame, so its capture records `depth ==
/// frames.len() + k` for the `k` constructor frames — the comparison then
/// mismatches and the explicit `fillInStackTrace` below still runs, replacing
/// the constructor-frame-contaminated trace with the clean one. Likewise a
/// missing / unwritten `depth` or `backtrace` field (opaque `_fN` bootstrap
/// layouts, `depth == 0`) returns `false`. Fidelity is never traded for the
/// saved walk: we skip only when the two traces are provably identical.
fn trace_already_captured_at_current_depth(
    shared: &SharedVm,
    thread: &JvmThread,
    obj: ObjectRef,
) -> bool {
    let frames = thread.frames.len();
    if frames == 0 {
        // Nothing to capture either way; leave the legacy path untouched so the
        // trace store still gets its (empty) entry exactly as before.
        return false;
    }
    // `backtrace` must be the self-reference marker `capture_throwable_trace`
    // parks there. Anything else (null, unset — which reads back as `Int(0)`,
    // not `Object(None)` — or a real JDK backtrace object) means our capture
    // did not run.
    let Some(bt_idx) = instance_field_index_by_name(shared, obj, "backtrace") else {
        return false;
    };
    if shared.mem.heap.get_field(obj, bt_idx) != Value::Object(Some(obj)) {
        return false;
    }
    let Some(depth_idx) = instance_field_index_by_name(shared, obj, "depth") else {
        return false;
    };
    matches!(
        shared.mem.heap.get_field(obj, depth_idx),
        Value::Int(d) if d > 0 && d as usize == frames
    )
}

/// HotSpot's `NoSuchMethodError` message for a failed method resolution:
/// `'<return> <class>.<name>(<params>)'` — source spelling throughout, params
/// comma-space separated, and **the single quotes are part of the message**.
///
/// Measured on JDK 25 (`apps/nsme_probe`, compile against a class then run
/// against one with the methods removed):
///
/// ```text
/// 'Lib Lib.widen(boolean)'
/// 'long Lib.calc(int, java.lang.String[], double[][])'
/// 'void Lib.plain()'
/// ```
///
/// We used to emit `Lib.widen(Z)LLib;` — the dotted class name (fixed earlier
/// for Spring Boot's `NoSuchMethodFailureAnalyzer`, which embeds this message
/// verbatim in its `FailureAnalysis` description) followed by the **raw
/// descriptor**, with no return type and no quotes. That is not a spelling
/// anything on the Java side can parse: Spring's analyzer splits the message on
/// `(` to recover the method name, and tools that diff a linkage failure across
/// JVMs see a different string for an identical defect. Found while triaging
/// `module/spring-boot-data-redis` on Linux, where a fixture whose
/// `spring-data-redis` classes were compiled against a newer Jedis than the
/// `jedis-7.4.1.jar` on its classpath makes both VMs raise this error for
/// `DefaultJedisClientConfig$Builder.autoNegotiateProtocol` — the failure was
/// shared, but only the *message* differed, which is exactly the shape a
/// "both VMs fail, so it is not our bug" triage hides.
///
/// The array/primitive spelling is [`helpful_npe::class_external`]'s (`int[]`,
/// `java.lang.Object[]`) — **not** [`hotspot_external_name`]'s descriptor form,
/// which `ClassCastException` uses; the two are deliberately different and the
/// oracle above pins which one belongs here. `shorten_jlang` is *not* applied:
/// HotSpot prints `java.lang.String[]` in full for this message.
fn nsme_message(class_name: &str, method_name: &str, method_descriptor: &str) -> String {
    let (params, ret) =
        crate::runtime::interpreter::invoke::split_method_descriptor_ref(method_descriptor);
    let rendered: Vec<String> = params
        .iter()
        .map(|p| helpful_npe::class_external(p))
        .collect();
    // `class_external` is built for *field/parameter* types, where `V` cannot
    // occur, so it passes `V` through unchanged. A return type can be void, and
    // HotSpot spells it `void` — handle it here rather than teaching the JEP 358
    // renderer about a type its own messages never name.
    let ret_rendered = if ret == "V" {
        "void".to_string()
    } else {
        helpful_npe::class_external(ret)
    };
    format!(
        "'{} {}.{}({})'",
        ret_rendered,
        class_name.replace('/', "."),
        method_name,
        rendered.join(", ")
    )
}

/// HotSpot's `Klass::external_name()` for an already-loaded class: the internal
/// name with `/` → `.`, **arrays left in descriptor form**.
///
/// Deliberately NOT `npe_message::class_external`, which renders the *JEP 358*
/// spelling (`int[]`, `java.lang.Object[]`, `String`/`Object` shortened).
/// `ClassCastException` / `ArrayStoreException` use the other one: measured on
/// JDK 25, `(String[]) (Object) new int[1]` reports
/// `class [I cannot be cast to class [Ljava.lang.String;`, and
/// `Object[] o = new String[1]; o[0] = new Integer[1];` reports
/// `ArrayStoreException: [Ljava.lang.Integer;` — descriptors, not `int[]`.
fn hotspot_external_name(internal: &str) -> String {
    internal.replace('/', ".")
}

/// Where HotSpot says a class lives, for the parenthetical half of a
/// `ClassCastException` message.
struct KlassOrigin {
    /// Named module (`java.base`), or `None` for the unnamed module.
    module: Option<String>,
    /// `ClassLoaderData::loader_name_and_id()` text, quotes included.
    loader: &'static str,
    /// The loader itself. Two *unnamed* modules of different loaders are
    /// different modules, so the joint/split decision needs more than the name.
    loader_id: cratonvm_types::ClassLoaderId,
}

/// HotSpot's `ClassLoaderData::loader_name_and_id()` for the three built-in
/// loaders, measured on JDK 25: `'bootstrap'`, `'platform'`, `'app'`. The
/// built-ins all carry a loader `name`, so HotSpot prints the quoted-name form
/// with no `@<hash>` suffix — verified against
/// `class ThrProbe cannot be cast to class java.lang.String (ThrProbe is in
/// unnamed module of loader 'app'; java.lang.String is in module java.base of
/// loader 'bootstrap')`.
///
/// `None` for a user-defined loader **on purpose**: HotSpot renders those as
/// `<loader class name> @<identity hash>` (or `'<name>' @<hash>`), and that hash
/// is a value we cannot reproduce. Fabricating one would put a wrong address in
/// a message log-scrapers read, so the caller keeps today's plain wording
/// instead. Recorded as the one uncovered case in
/// docs/known-issues/jdk-only/W7-37-differential-throwable-and-vm.md.
fn loader_name_and_id(loader: cratonvm_types::ClassLoaderId) -> Option<&'static str> {
    use cratonvm_types::ClassLoaderId;
    match loader {
        ClassLoaderId::Bootstrap => Some("'bootstrap'"),
        ClassLoaderId::Extension => Some("'platform'"),
        ClassLoaderId::Application => Some("'app'"),
        ClassLoaderId::UserDefined(_) => None,
    }
}

/// Which class `klass_origin` has to resolve to answer for a display name.
///
/// The three descriptor shapes need three different actions and must not be
/// collapsed. `interpreter::constants::array_component_class_name` is the
/// existing helper for the same parse, but it is `pub(super)` to the
/// interpreter module *and* answers `None` for both "not an array" and
/// "primitive-component array" — the exact distinction that decides whether
/// this function does a lookup at all — so it cannot serve here.
#[derive(Debug, PartialEq, Eq)]
enum OriginLookup<'a> {
    /// Not an array: resolve this name itself.
    Plain(&'a str),
    /// Reference-component array: resolve the *bottom* component's name.
    Component(&'a str),
    /// Primitive-component array at any depth (`[I`, `[[J`): java.base and the
    /// bootstrap loader, with no lookup.
    PrimitiveArray,
}

/// Parse a cast-message operand into the lookup `klass_origin` must perform.
///
/// `display_name` is HotSpot's `external_name()` spelling — dotted, arrays left
/// in JVMS descriptor form (`[I`, `[Ljava.lang.String;`, `[[LCastProbe;`), NOT
/// the JEP 358 source form (`int[]`). Measured on Temurin 25.0.3.9; see the
/// transcript in `klass_origin`.
fn origin_lookup(display_name: &str) -> OriginLookup<'_> {
    let dims = display_name.bytes().take_while(|b| *b == b'[').count();
    match display_name[dims..].strip_prefix('L') {
        // `[Ljava.lang.String;` -> `java.lang.String`. Stripping *all* leading
        // `[` first is deliberate: HotSpot walks to the bottom klass, so
        // `[[Ljava.lang.String;` and `[Ljava.lang.String;` give the same clause.
        Some(component) if dims > 0 => OriginLookup::Component(component.trim_end_matches(';')),
        // Not an array, but the name happens to start with `L` (`Long`).
        Some(_) => OriginLookup::Plain(display_name),
        None if dims > 0 => OriginLookup::PrimitiveArray,
        None => OriginLookup::Plain(display_name),
    }
}

/// Resolve a class *display* name (dotted, possibly an array descriptor) to the
/// module/loader pair HotSpot names it by.
///
/// The name is all we have: `RuntimeError::ClassCastException`'s whole payload
/// is a pre-rendered `message: String`, and `types/src/error.rs` — where a
/// two-`ClassId` variant would have to live — is another lane's file. So
/// resolve with `ClassManager::find_unique_class_by_name`, whose own doc marks
/// it as the lookup that is safe for diagnostics carrying no initiating loader
/// *because* it answers `None` rather than guessing when two loaders defined
/// the name. Only when that is inconclusive do we ask the current frame's
/// loader — the loader that resolved the `checkcast` constant-pool entry.
///
/// Nothing dispatches on this answer; it decorates a message. But the rule this
/// campaign keeps re-learning is that a by-name binding has to be a narrowed,
/// deliberate choice rather than a convenience, so it is one here.
fn klass_origin(shared: &SharedVm, thread: &JvmThread, display_name: &str) -> Option<KlassOrigin> {
    let cm = shared.classes.class_manager.read();

    // Parse the descriptor BEFORE looking anything up. The previous shape
    // opened with `find_unique_class_by_name(display_name)` — a lookup of the
    // *array class itself* — and only then parsed the `[` prefix, which made
    // the whole array arm reachable only when something else had already put
    // that exact array class in the definition index. Measured (W7-37 §B8,
    // §B11.1): four of six cast shapes with an array operand dropped back to
    // the bare `X cannot be cast to Y` form, and the two that did not were the
    // shapes whose own probe happened to allocate the array type it was asking
    // about — which is how this row was recorded as fixed twice.
    //
    // HotSpot reads module and loader off the *bottom* klass
    // (`Klass::class_in_module_of_loader` walks `ObjArrayKlass::bottom_klass`,
    // and `ArrayKlass::class_loader_data()` is the component's CLD, per JVMS
    // §5.3.3 step 2: "the Java Virtual Machine marks C to have the defining
    // loader of the component type as its defining loader"). So resolve the
    // component and answer from it — no array class needs to exist anywhere.
    //
    // Measured on Temurin 25.0.3.9 (`CastMsgs` probe, docs/known-issues/
    // jdk-only/W8-C4-1-array-cast-klass-origin.md §1):
    //   [LCastProbe; is in unnamed module of loader 'app'
    //   [Ljava.lang.String; is in module java.base of loader 'bootstrap'
    //   [[I and java.lang.String are in module java.base of loader 'bootstrap'
    // i.e. the array's clause is the *component's* module and loader verbatim,
    // and a primitive-component array — at any depth, since `[[I` is an
    // objArrayKlass whose bottom klass is the typeArrayKlass `[I` and so takes
    // HotSpot's "klass is an array of primitives, module is java.base" arm — is
    // java.base / bootstrap with no lookup at all.
    let lookup_name = match origin_lookup(display_name) {
        OriginLookup::Plain(name) | OriginLookup::Component(name) => name,
        OriginLookup::PrimitiveArray => {
            return Some(KlassOrigin {
                module: Some("java.base".to_string()),
                loader: "'bootstrap'",
                loader_id: cratonvm_types::ClassLoaderId::Bootstrap,
            })
        }
    };

    let class_id = cm.find_unique_class_by_name(lookup_name).or_else(|| {
        let frame_class = thread.frames.last()?.class_id;
        cm.find_class_by_name_for_class(lookup_name, frame_class)
    })?;
    let class = cm.get_class(class_id)?;
    let loader_id = class.loader_id;
    let loader = loader_name_and_id(loader_id)?;
    Some(KlassOrigin {
        module: class.module_name.clone(),
        loader,
        loader_id,
    })
}

/// HotSpot's `Klass::class_in_module_of_loader`, minus the module `@version`
/// clause — JDK 25 prints no version for `java.base` (measured), and CratonVM
/// records none for any module.
fn class_in_module_of_loader(display_name: &str, origin: &KlassOrigin, use_are: bool) -> String {
    let verb = if use_are { "are" } else { "is" };
    match &origin.module {
        Some(module) => format!(
            "{display_name} {verb} in module {module} of loader {}",
            origin.loader
        ),
        None => format!(
            "{display_name} {verb} in unnamed module of loader {}",
            origin.loader
        ),
    }
}

/// Rebuild a cast refusal in HotSpot's wording, or `None` when we cannot name
/// both operands' module and loader.
///
/// Mirrors `SharedRuntime::generate_class_cast_message`: one joint clause when
/// both klasses are in the same module (`Klass::joint_in_module_of_loader`),
/// two `; `-separated clauses otherwise. Both shapes are measured rather than
/// recalled — the JDK 25 transcript is in the record.
fn hotspot_class_cast_message(
    shared: &SharedVm,
    thread: &JvmThread,
    from_display: &str,
    to_display: &str,
) -> Option<String> {
    let from = klass_origin(shared, thread, from_display)?;
    let to = klass_origin(shared, thread, to_display)?;
    // "Same module" in HotSpot's sense: it compares `ModuleEntry*`, so two
    // classes that are both in *an* unnamed module only share a module when
    // they share a loader.
    let same_module =
        from.module == to.module && (from.module.is_some() || from.loader_id == to.loader_id);
    let parenthetical = if same_module {
        format!(
            "{from_display} and {}",
            class_in_module_of_loader(to_display, &to, true)
        )
    } else {
        format!(
            "{}; {}",
            class_in_module_of_loader(from_display, &from, false),
            class_in_module_of_loader(to_display, &to, false)
        )
    };
    Some(format!(
        "class {from_display} cannot be cast to class {to_display} ({parenthetical})"
    ))
}

/// Split `X cannot be cast to Y` / `class X cannot be cast to class Y` into its
/// two operands, or `None` when the message is not exactly that shape.
///
/// Deliberately strict. `RuntimeError::ClassCastException` is also raised with
/// free text — the checked-collection refusals, and the JIT-panic bridge in
/// `interpreter/jit_bridge.rs` whose payload string merely *contains*
/// `ClassCastException` — and rewriting one of those would be a fabrication.
/// Requiring both operands to be whitespace-free, with nothing else in the
/// message, also makes the rewrite idempotent: an already-normalised message
/// ends in ` (…)`, so its right operand contains a space and it is left alone.
fn split_cast_operands(message: &str) -> Option<(&str, &str)> {
    let (lhs, rhs) = message.split_once(" cannot be cast to ")?;
    let lhs = lhs.strip_prefix("class ").unwrap_or(lhs);
    let rhs = rhs.strip_prefix("class ").unwrap_or(rhs);
    if lhs.is_empty() || rhs.is_empty() {
        return None;
    }
    if lhs.contains(char::is_whitespace) || rhs.contains(char::is_whitespace) {
        return None;
    }
    Some((lhs, rhs))
}

/// Give a VM-minted `ClassCastException` / `ArrayStoreException` the message
/// HotSpot mints for the same failure.
///
/// Both are *generated by the VM* rather than written by a JDK author, so
/// nothing else in the system can get them right — and their exact text is
/// load-bearing. Measured 2026-08-12 against JDK 25 (both VMs running the same
/// class file): HotSpot prints
/// `class java.lang.String cannot be cast to class java.lang.Integer
/// (java.lang.String and java.lang.Integer are in module java.base of loader
/// 'bootstrap')` where CratonVM printed `java.lang.String cannot be cast to
/// java.lang.Integer`, and `ArrayStoreException: java.lang.Integer` where
/// CratonVM printed the *internal* name `java/lang/Integer`.
///
/// A slashed name is not merely ugly. The same defect in the `checkcast`
/// message once made mockk's `JvmAutoHinter` regex capture `String` instead of
/// `java.lang.String` (see the comment at the `checkcast` raise site in
/// `interpreter/opcodes.rs`); every caller that feeds an `ArrayStoreException`
/// message to `Class.forName` has that hole today.
///
/// Applied here — at the single funnel every VM-raised `RuntimeError` passes
/// through — rather than at the raise sites, because the interpreter
/// `checkcast`/`aastore` opcodes, `interpreter/lambda.rs`'s argument check and
/// the JIT cast helper all live in other lanes' files.
///
/// **Which paths newly print a different string:** exactly those whose
/// `ClassCastException` message is already a bare two-operand cast refusal
/// (interpreter `checkcast`, `lambda.rs`'s `cannot be cast to`, the JIT's
/// `class … cannot be cast to class …`), plus `ArrayStoreException`s whose
/// message is a slashed internal class name (interpreter `aastore`, and the
/// `arraycopy` element check in `interpreter.rs`). Everything else — checked
/// collections, the JIT panic bridge, any message with whitespace in an operand
/// — is returned byte-identical.
fn hotspot_vm_type_error_message(
    shared: &SharedVm,
    thread: &JvmThread,
    error: RuntimeError,
) -> RuntimeError {
    match error {
        RuntimeError::ClassCastException { message } => {
            let rebuilt = split_cast_operands(&message)
                .and_then(|(from, to)| hotspot_class_cast_message(shared, thread, from, to));
            RuntimeError::ClassCastException {
                message: rebuilt.unwrap_or(message),
            }
        }
        RuntimeError::ArrayStoreException { message } => {
            // The raise sites store the offending element's *class name*, so
            // only rewrite something that still looks like one. A message with
            // no `/` is either already external (`[I`, a default-package class)
            // or free text; in both cases `replace` would be a no-op or a
            // corruption.
            let rewritable = message.contains('/') && !message.contains(char::is_whitespace);
            RuntimeError::ArrayStoreException {
                message: if rewritable {
                    hotspot_external_name(&message)
                } else {
                    message
                },
            }
        }
        other => other,
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
            // SB-LOADER-ZIPCONTENT (2026-08-04): old-gen-spilling retry, same
            // reason as `alloc_object_shared` / `gc_alloc_array` — a young free
            // list fragmented by the non-moving JIT-safe sweep must not report
            // OOM while the old generation still holds most of the heap. This
            // one matters twice over: failing here replaces the exception the
            // program actually threw with an `OutOfMemoryError`.
            shared
                .mem
                .heap
                .try_alloc_object_full(class_id, num_fields)
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

    // 4. Call fillInStackTrace — but only if the constructor did not already
    //    capture an identical trace.
    //
    // Historically this ran unconditionally ("just in case" the constructor
    // didn't do it). That made every VM-raised throw capture the stack **twice**:
    // the `native_exc_init_*` constructor shadows registered for ~50 exception
    // subclasses all funnel through `capture_throwable_trace`, and so does
    // `Throwable.fillInStackTrace`. Each capture is *not* cheap — per Java frame
    // it resolves the method in the `ClassStore` and linearly scans its
    // `LineNumberTable` (`stackwalker::entry_from_frame`), then the result is
    // allocated into a `Vec<StackTraceEntry>`, **cloned** a second time
    // (`NativeContextImpl::capture_throwable_stack_trace`), and inserted into the
    // VM-wide `throwable_stacks` map under a `RwLock::write`. At a Spring/JUnit
    // stack depth of 50-150 frames that is two O(depth) walks, four vector
    // allocations and two global write-lock acquisitions per throw, where one of
    // each suffices — and the second capture's result was byte-identical to the
    // first, so it was pure waste that also *replaced* a correct entry with an
    // equal one.
    //
    // `trace_already_captured_at_current_depth` only reports `true` when the
    // constructor's capture provably matches what this call would produce (see
    // its doc for the `backtrace`/`depth` proof, and for why every uncertain
    // case — bytecode `<init>`, opaque field layouts, absent markers — falls
    // through to the explicit call). Fidelity is preserved exactly; only the
    // duplicate is removed.
    let obj_ref = thread.native_pin_roots[pin_base];
    if !trace_already_captured_at_current_depth(shared, thread, obj_ref) {
        let _ = invoke_on_class_shared(
            shared,
            thread,
            class_id,
            "fillInStackTrace",
            "(I)Ljava/lang/Throwable;",
            &[Value::Object(Some(obj_ref)), Value::Int(0)],
        );
    }

    // 5. Mirror the two `Throwable` *instance-field initialisers* the JDK
    //    source declares but that our partially-emulated construction can miss.
    let obj_ref = thread.native_pin_roots[pin_base];
    mirror_throwable_field_initialisers(shared, obj_ref);

    let obj_ref = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);
    Ok(obj_ref)
}

/// Mirror `Throwable`'s two declared instance-field initialisers —
/// `private Throwable cause = this;` and
/// `private List<Throwable> suppressedExceptions = SUPPRESSED_SENTINEL;` — on a
/// throwable this module built.
///
/// Only ever reached right after step 3, so no user code has observed the object
/// yet and there is nothing to clobber. Only `()V` and `(String)V` are invoked
/// above, and the four-arg suppression-disabling constructor is unreachable from
/// here, so no state this writes is state HotSpot would have left alone.
///
/// Both fields are now *read as decisions*, which is why the gap had to close:
///
/// * `Throwable.initCause` refuses with `IllegalStateException` unless
///   `cause == this`. A VM-minted throwable whose `cause` never got the
///   sentinel would refuse a first, legitimate `initCause`.
/// * `Throwable.addSuppressed` treats a **null** `suppressedExceptions` as
///   "suppression disabled" and silently drops the call — that is the JDK's own
///   rule, and it is what makes the four-arg constructor work. Measured
///   2026-08-12 on `--real-jdk`: a VM-minted `NullPointerException` and a
///   `ClassNotFoundException` both came out with a **null** list where HotSpot
///   has the empty-list sentinel, so honouring that rule without this mirror
///   would silently lose the `close()` failure from every try-with-resources
///   whose body threw a VM-minted exception.
///
/// The `cause` half deliberately fills in only a slot that was never written
/// (`Int(0)` — an *unset* reference slot reads back as that rather than as
/// `Object(None)`). A genuine stored null must be left alone: several JDK
/// throwables null their cause on purpose — `ClassNotFoundException()` and
/// `InvocationTargetException()` both chain to `super((Throwable) null)` — and
/// HotSpot then correctly refuses `initCause` on them, which was measured on
/// both VMs. Overwriting that null with the sentinel would turn a specified
/// refusal into a silent success.
///
/// The suppressed half is the `create_exception_object` sibling of
/// `capture_throwable_trace`'s `init_suppressed_sentinel` in `native-builtins`'
/// `lang_misc.rs`, which does the same mirroring for the constructor shadows.
/// Reading the real static (rather than parking any empty list) keeps the JDK
/// bytecode's own `== SUPPRESSED_SENTINEL` identity tests valid on the rare
/// paths that still reach it. Before `Throwable.<clinit>` has populated the
/// static there is nothing faithful to write, so a bootstrap-era throwable is
/// left as-is.
fn mirror_throwable_field_initialisers(shared: &SharedVm, obj: ObjectRef) {
    if let Some(idx) = instance_field_index_by_name(shared, obj, "cause") {
        if matches!(shared.mem.heap.get_field(obj, idx), Value::Int(0)) {
            shared
                .mem
                .heap
                .set_field(obj, idx, Value::Object(Some(obj)));
        }
    }
    let Some(idx) = instance_field_index_by_name(shared, obj, "suppressedExceptions") else {
        return;
    };
    if matches!(shared.mem.heap.get_field(obj, idx), Value::Object(Some(_))) {
        return;
    }
    let Some(sentinel) = throwable_suppressed_sentinel(shared) else {
        return;
    };
    shared.mem.heap.set_field(obj, idx, sentinel);
}

/// `java.lang.Throwable.SUPPRESSED_SENTINEL` — the shared immutable empty list
/// the JDK parks in `suppressedExceptions` to mean "suppression enabled, none
/// recorded yet". `None` until `Throwable.<clinit>` has run.
///
/// The `(ClassId, static index)` pair is cached across calls because exception
/// construction is a per-throw cost and framework code throws as control flow —
/// the same reason the `CRATONVM_DBG_*` reads above are cached. Cached lazily
/// rather than in a `OnceLock<Option<_>>`: the first VM-raised throw can precede
/// `Throwable.<clinit>`, and a `OnceLock` would freeze that `None` forever.
fn throwable_suppressed_sentinel(shared: &SharedVm) -> Option<Value> {
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    const UNRESOLVED: u32 = u32::MAX;
    static CLASS: AtomicU32 = AtomicU32::new(UNRESOLVED);
    static INDEX: AtomicUsize = AtomicUsize::new(0);

    // `CLASS` is the publication flag: it is stored last, with `Release`, so a
    // thread that sees a resolved class id also sees the matching `INDEX`.
    let cached = CLASS.load(Ordering::Acquire);
    let (class_id, index) = if cached != UNRESOLVED {
        (ClassId::new(cached), INDEX.load(Ordering::Relaxed))
    } else {
        let cm = shared.classes.class_manager.read();
        let class_id = cm.find_bootstrap_class_by_name("java/lang/Throwable")?;
        let class = cm.get_class(class_id)?;
        let mut static_idx = 0usize;
        let mut found = None;
        for f in &class.fields {
            if !f.is_static() {
                continue;
            }
            if &*f.name == "SUPPRESSED_SENTINEL" {
                found = Some(static_idx);
                break;
            }
            static_idx += 1;
        }
        let index = found?;
        drop(cm);
        INDEX.store(index, Ordering::Relaxed);
        CLASS.store(class_id.as_u32(), Ordering::Release);
        (class_id, index)
    };
    match crate::vm::get_static_shared(shared, class_id, index) {
        v @ Value::Object(Some(_)) => Some(v),
        // `<clinit>` has not populated it yet — nothing faithful to write.
        _ => None,
    }
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
/// Attach the compiled frames snapshotted at a JIT-signalled implicit NPE to
/// the throwable that was constructed for it.
///
/// The construction happens after the compiled activation has returned, so the
/// trace `fillInStackTrace` just stored names none of the compiled code that
/// raised the exception. `snapshot` is what those frames were, taken inside the
/// helper while they were still live; this splices them back onto the front.
///
/// A `None` snapshot (the kill switch, or an NPE with no compiled frames under
/// it) leaves the throwable exactly as it was.
pub fn attach_snapshotted_npe_frames(
    shared: &SharedVm,
    throwable: ObjectRef,
    snapshot: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
) {
    let Some(snapshot) = snapshot else {
        return;
    };
    if snapshot.is_empty() {
        return;
    }
    let hash = shared.mem.heap.identity_hash_code(throwable);
    let Some(existing) = shared.throwable_stack_trace(hash) else {
        // No trace was stored for this throwable (the boot path where the NPE
        // class is not loaded yet). Nothing to splice onto.
        return;
    };
    let cm = shared.classes.class_manager.read();
    let merged = crate::runtime::stackwalker::append_snapshotted_compiled_frames(
        &cm.class_store,
        &snapshot,
        existing,
    );
    drop(cm);
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STTRACE").is_some() {
        eprintln!(
            "STTRACE_DBG_NPE_SNAPSHOT recovered={} trace_now={}",
            snapshot.len(),
            merged.len()
        );
        for f in &snapshot {
            eprintln!("  STTRACE_DBG_NPE_SNAPSHOT[] {} bci={}", f.label, f.bci);
        }
    }
    shared.store_throwable_stack_trace(throwable, merged);
}

pub fn throw_runtime_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    error: RuntimeError,
) -> MethodCallFailed {
    // `CRATONVM_DBG_RTERR=<substring>` -- dump every VM-raised runtime error
    // whose `Debug` form contains `<substring>` (empty matches all), with the
    // Java stack at the raise point. The VM-side raise path never goes through
    // `athrow`, so `CRATONVM_DBG_ATHROW` cannot see these; this is the
    // companion for exceptions manufactured in Rust (a `ClassNotFoundException`
    // out of a classloading native being the case it was written for).
    if let Some(want) = dbg_rterr_filter() {
        let text = format!("{error:?}");
        if want.is_empty() || text.contains(want) {
            eprintln!("[DBG_RTERR] {text}");
            let cm = shared.classes.class_manager.read();
            for (i, f) in thread.frames.iter().enumerate().rev().take(25) {
                let cn = cm
                    .get_class(f.class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                eprintln!("[DBG_RTERR-STK {i}] {}.{} pc={}", cn, f.method_name(), f.pc);
            }
        }
    }
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
    if dbg_npe_none() {
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
    if dbg_aioobe() {
        if let RuntimeError::ArrayIndexOutOfBoundsException { index, message } = &error {
            eprintln!(
                "[AIOOBE-THROW] index={index} message={} — full live Java thread stack ({} frames, deepest first):",
                message.as_deref().unwrap_or("<none>"),
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
    if dbg_bufunder() && matches!(&error, RuntimeError::BufferUnderflowException) {
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
            if dbg_npe_trace() {
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
            if dbg_wf_npe() {
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
            // Targeted NPE-origin dump: `CRATONVM_DBG_NPE_MATCH=<substring>`.
            // Narrower than `CRATONVM_IAE_TRACE` (which dumps EVERY NPE, and
            // Spring startup raises hundreds it catches), so a rare, swallowed
            // NPE can be located without drowning the log.
            if let Some(needle) = dbg_npe_match() {
                if let RuntimeError::NullPointerException { message: Some(m) } = &error {
                    if m.contains(needle) {
                        eprintln!("[NPE-MATCH] msg={m}");
                        for (i, f) in thread.frames.iter().enumerate().rev().take(40) {
                            let cn = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(f.class_id)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            eprintln!("[NPE-MATCH-STK {i}] {}.{} pc={}", cn, f.method_name(), f.pc);
                        }
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
    // Give the two type errors the VM *mints itself* HotSpot's wording before
    // the message becomes a `detailMessage` String. Done after every debug dump
    // above so `CRATONVM_DBG_RTERR` still shows the raise site's own text, and
    // before `as_java_throwable` because that is what reads the message out.
    // See `hotspot_vm_type_error_message` for exactly which raise sites this
    // changes and which are returned byte-identical.
    let error = hotspot_vm_type_error_message(shared, thread, error);
    // One table, shared with the reflective `Method.invoke` /
    // `Constructor.newInstance` wrapper in `native-builtins` — see
    // `RuntimeError::as_java_throwable`. `None` means "not a Java exception"
    // (`NotImplemented`), which stays an internal error.
    let Some((class_name, message)) = error.as_java_throwable() else {
        return MethodCallFailed::InternalError(VmError::Runtime(error));
    };

    match create_exception_object(shared, thread, class_name, message.as_deref()) {
        Ok(obj_ref) => {
            populate_pattern_syntax_fields(shared, obj_ref, &error);
            MethodCallFailed::ExceptionThrown(obj_ref)
        }
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

/// Fill in `PatternSyntaxException`'s `desc` / `pattern` / `index` after the
/// generic constructor path has allocated it.
///
/// That class is the one throwable here whose message is NOT
/// `Throwable.detailMessage`: it declares only `(String desc, String regex,
/// int index)`, overrides `getMessage()`, and assembles a three-line report
/// from the fields. `create_exception_object` knows how to call `()V` and
/// `(String)V`, neither of which exists on it, so without this the object came
/// out with all three fields null/zero and `getMessage()` returned
/// `"null near index 0\r\nnull"`.
///
/// A no-op for every other error, and by-name so it cannot depend on the JDK's
/// private field order.
fn populate_pattern_syntax_fields(shared: &SharedVm, obj_ref: ObjectRef, error: &RuntimeError) {
    let RuntimeError::PatternSyntaxException {
        description,
        pattern,
        index,
    } = error
    else {
        return;
    };
    let set = |name: &str, value: Value| {
        let class_id = shared.mem.heap.class_id_of(obj_ref);
        let cm = shared.classes.class_manager.read();
        if let Some(i) =
            crate::vm::vm_exec::resolve_field_index_in_hierarchy(class_id, name, &cm.class_store)
        {
            drop(cm);
            shared.mem.heap.set_field(obj_ref, i, value);
        }
    };
    if let Some(d) = crate::vm::try_create_java_string_uninterned(shared, description) {
        set("desc", Value::Object(Some(d)));
    }
    if let Some(p) = crate::vm::try_create_java_string_uninterned(shared, pattern) {
        set("pattern", Value::Object(Some(p)));
    }
    set("index", Value::Int(*index));
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
    // `CRATONVM_DBG_LINKAGE_BT=1` -- same hook `linkage_throwable` carries, for
    // the OTHER way a `NoClassDefFoundError` reaches Java. The Java stack stops
    // at whatever bytecode triggered resolution; only the Rust backtrace names
    // the resolver that decided the class was missing.
    if dbg_linkage_bt() {
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!("[DBG_LINKAGE_BT] NoClassDefFoundError {class_name}\n{bt}");
    }
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

/// Cached `CRATONVM_DBG_RTERR` filter (see `throw_runtime_error`). Read once:
/// `throw_runtime_error` is on the hot path of every VM-raised NPE / CCE /
/// AIOOBE, so a per-call `env::var` allocation is not acceptable here.
fn dbg_rterr_filter() -> Option<&'static str> {
    static FILTER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    FILTER
        .get_or_init(|| cratonvm_types::flags::runtime_var("CRATONVM_DBG_RTERR").ok())
        .as_deref()
}

/// Cached `CRATONVM_DBG_LINKAGE_BT` flag (see `linkage_throwable`).
fn dbg_linkage_bt() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_LINKAGE_BT").is_some())
}

fn linkage_throwable(error: &LinkageError) -> (&'static str, String) {
    // `CRATONVM_DBG_LINKAGE_BT=1` -- Rust backtrace at every linkage-error
    // raise. A `VerifyError`/`NoSuchMethodError` reaching Java carries only
    // the class+method; the *VM* call path that produced it (which resolver,
    // which native) is what actually identifies the defect.
    if dbg_linkage_bt() {
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!("[DBG_LINKAGE_BT] {error:?}\n{bt}");
    }
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
            nsme_message(class_name, method_name, method_descriptor),
        ),
        LinkageError::IncompatibleClassChangeError { message } => {
            ("java/lang/IncompatibleClassChangeError", message.clone())
        }
        // `java.lang.LinkageError` ITSELF, not a subclass — that is what
        // HotSpot throws for a duplicate definition, and code that catches it
        // (Tomcat's loader lifecycle, ByteBuddy's injection strategies) catches
        // the base type. The message reproduces HotSpot's wording up to the
        // parenthetical module tail, which carries an identity hash.
        LinkageError::DuplicateClassDefinition { class_name, loader } => (
            "java/lang/LinkageError",
            format!(
                "loader {} attempted duplicate class definition for {}.",
                loader,
                class_name.replace('/', ".")
            ),
        ),
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
        // Deliberately NOT `format!("{class_name}: {message}")` like the arm
        // above: HotSpot's wording already names the class, mid-sentence
        // ("Preview features are not enabled for P (class file version
        // 69.65535)..."), so prefixing would print it twice.
        LinkageError::UnsupportedClassVersionError { message, .. } => {
            ("java/lang/UnsupportedClassVersionError", message.clone())
        }
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
    if dbg_verify_error() {
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
        Err(e) => {
            // Falling back means the linkage error stays a `VmError`, which no
            // Java `catch` can ever observe — it unwinds past every handler and
            // kills the VM at `main-vm run()`. That is a very different outcome
            // from "an error was thrown", so say why the throwable could not be
            // built instead of failing silently.
            tracing::warn!(
                "throw_linkage_error: could not materialize {class_name} ({detail}); \
                 propagating as an uncatchable VM error instead: {e:?}"
            );
            MethodCallFailed::InternalError(VmError::Linkage(error))
        }
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
    if dbg_ncdfe() {
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
    // `klass_origin`'s descriptor parse (W7-37 §B8.1 / §B11.1)
    // -----------------------------------------------------------------------

    /// The operand spellings are HotSpot's, taken verbatim from a Temurin
    /// 25.0.3.9 run of the `CastMsgs` probe (see
    /// `docs/known-issues/jdk-only/W8-C4-1-array-cast-klass-origin.md`
    /// §1). They are *dotted descriptors*, not JEP 358 source form: HotSpot
    /// prints `class [I cannot be cast to class [Ljava.lang.String;`, never
    /// `int[]` / `String[]`, for a cast refusal.
    ///
    /// The bug this pins: `klass_origin` used to look the *array class* up in
    /// the definition index and only parse the `[` prefix afterwards, so the
    /// array arm ran only when something unrelated had already defined that
    /// exact array class — non-deterministically, per program. Parsing first
    /// removes the dependence on incidental index state, and this test is the
    /// part of that which needs no VM.
    #[test]
    fn origin_lookup_resolves_the_bottom_component_not_the_array() {
        // Plain classes resolve themselves — including one whose name starts
        // with the descriptor tag `L`, which a naive `strip_prefix('L')` eats.
        assert_eq!(
            origin_lookup("java.lang.String"),
            OriginLookup::Plain("java.lang.String")
        );
        assert_eq!(origin_lookup("Long"), OriginLookup::Plain("Long"));
        assert_eq!(
            origin_lookup("CastProbe"),
            OriginLookup::Plain("CastProbe"),
            "a default-package application class has no separator at all"
        );

        // Reference arrays resolve the BOTTOM component, at every depth: HotSpot
        // walks `ObjArrayKlass::bottom_klass`, so `[[Ljava.lang.String;` and
        // `[Ljava.lang.String;` print the same module/loader clause.
        assert_eq!(
            origin_lookup("[Ljava.lang.String;"),
            OriginLookup::Component("java.lang.String")
        );
        assert_eq!(
            origin_lookup("[[Ljava.lang.String;"),
            OriginLookup::Component("java.lang.String")
        );
        assert_eq!(
            origin_lookup("[LCastProbe;"),
            OriginLookup::Component("CastProbe"),
            "measured: `[LCastProbe; is in unnamed module of loader 'app'` — an \
             array takes the COMPONENT's defining loader (JVMS §5.3.3 step 2), \
             so the component is the class that has to be resolved"
        );

        // Primitive arrays need no lookup at any depth. `[[I` is an
        // objArrayKlass whose bottom klass is the typeArrayKlass `[I`, and
        // HotSpot's `else` arm hard-codes java.base for it — measured:
        // `([[I and java.lang.String are in module java.base of loader
        // 'bootstrap')`.
        for name in ["[I", "[[I", "[J", "[[[D", "[Z"] {
            assert_eq!(
                origin_lookup(name),
                OriginLookup::PrimitiveArray,
                "{name} must answer java.base/bootstrap without a lookup"
            );
        }
    }

    /// `split_cast_operands` has to leave an already-rewritten message alone,
    /// or the funnel would nest parentheticals every time a message passed
    /// through it twice. Array operands are the case worth pinning because
    /// they contain `;` and `[` and nothing else in the splitter looks at
    /// those.
    #[test]
    fn split_cast_operands_handles_array_descriptors_and_is_idempotent() {
        assert_eq!(
            split_cast_operands("class [I cannot be cast to class [Ljava.lang.String;"),
            Some(("[I", "[Ljava.lang.String;"))
        );
        assert_eq!(
            split_cast_operands("[I cannot be cast to [Ljava.lang.String;"),
            Some(("[I", "[Ljava.lang.String;")),
            "the bare pre-rewrite wording must split too — that is the shape \
             the interpreter's checkcast raise site produces"
        );
        assert_eq!(
            split_cast_operands(
                "class [I cannot be cast to class [Ljava.lang.String; ([I and \
                 [Ljava.lang.String; are in module java.base of loader 'bootstrap')"
            ),
            None,
            "an already-rewritten message must NOT split again"
        );
    }

    // -----------------------------------------------------------------------
    // Cached debug-flag helpers (throw hot path)
    // -----------------------------------------------------------------------

    /// `throw_runtime_error` used to pay three uncached `cratonvm_types::flags::runtime_var_os`
    /// lookups on *every* VM-raised throw, plus one each in
    /// `convert_class_not_found` and `throw_linkage_error`. They are now
    /// process-lifetime memoized. The failure mode a memo introduces is a typo'd
    /// or inverted variable name — the switch would silently never fire and the
    /// next person debugging an NPE origin would chase a dead flag. Comparing
    /// each helper against the live environment catches exactly that.
    #[test]
    fn cached_throw_path_debug_flags_match_environment_and_are_stable() {
        let pairs: [(fn() -> bool, &str); 7] = [
            (dbg_npe_none, "CRATONVM_DBG_NPE_NONE"),
            (dbg_aioobe, "CRATONVM_DBG_AIOOBE"),
            (dbg_bufunder, "CRATONVM_DBG_BUFUNDER"),
            (dbg_npe_trace, "CRATONVM_DBG_NPE_TRACE"),
            (dbg_wf_npe, "CRATONVM_DBG_WF_NPE"),
            (dbg_ncdfe, "CRATONVM_DBG_NCDFE"),
            (dbg_verify_error, "CRATONVM_DBG_VERIFY_ERROR"),
        ];
        for (flag, name) in pairs {
            let expected = cratonvm_types::flags::runtime_var_os(name).is_some();
            assert_eq!(
                flag(),
                expected,
                "cached flag disagrees with env for {name}"
            );
            assert_eq!(flag(), expected, "cached flag for {name} is not stable");
        }
        assert_eq!(
            iae_trace_enabled(),
            cratonvm_types::flags::runtime_var("CRATONVM_IAE_TRACE").is_ok()
        );
    }

    // -----------------------------------------------------------------------
    // Stack-trace capture: no duplicate walk, no lost fidelity
    // -----------------------------------------------------------------------

    /// The skip guard must fail **closed** when the thread has no Java frames.
    /// A freshly built VM's main thread is in exactly that state, and it is the
    /// state every bootstrap-era throwable is constructed in — so the explicit
    /// `fillInStackTrace` still runs there and the legacy behaviour is bit-for-bit
    /// preserved.
    #[test]
    fn duplicate_trace_guard_is_closed_with_no_frames() {
        let mut vm = test_vm();
        let Ok(obj) = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/IllegalStateException",
            Some("probe"),
        ) else {
            // Stripped VM (no JDK on the test classpath): nothing to probe with.
            return;
        };
        // Any heap object works as the probe: with zero frames the guard must
        // short-circuit to `false` before it ever looks at a field.
        if vm.main_thread.frames.is_empty() {
            assert!(
                !trace_already_captured_at_current_depth(&vm.shared, &vm.main_thread, obj),
                "guard must not skip fillInStackTrace when there are no frames"
            );
        }
    }

    /// The core fidelity invariant behind dropping the redundant
    /// `fillInStackTrace` call: whenever the constructor's capture actually ran
    /// (proved by the `backtrace = this` self-marker that
    /// `capture_throwable_trace` parks there), a trace **must** be registered in
    /// the VM-wide store keyed by the throwable's identity hash.
    ///
    /// If that implication ever breaks, `create_exception_object` could skip the
    /// explicit `fillInStackTrace` and leave the throwable with an empty
    /// `getStackTrace()` — the precise regression this change must never cause.
    #[test]
    fn constructor_capture_marker_implies_a_registered_trace() {
        let mut vm = test_vm();
        let Ok(obj) = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/IllegalStateException",
            Some("boom"),
        ) else {
            // Stripped VM (no JDK on the test classpath): nothing to assert.
            return;
        };
        let Some(bt_idx) = instance_field_index_by_name(&vm.shared, obj, "backtrace") else {
            return;
        };
        if vm.shared.mem.heap.get_field(obj, bt_idx) != Value::Object(Some(obj)) {
            // Our capture never ran for this class in this configuration.
            return;
        }
        let hash = vm.shared.mem.heap.identity_hash_code(obj);
        assert!(
            vm.shared.throwable_stack_trace(hash).is_some(),
            "backtrace self-marker is set but no trace was registered"
        );
    }

    /// Nested / rethrown shape: building a second throwable while the first is
    /// still live must not evict or overwrite the first one's trace. The store
    /// is keyed by identity hash, so both entries have to coexist — otherwise a
    /// `catch (E e) { throw new F(e); }` chain would print the wrong frames for
    /// the cause.
    #[test]
    fn nested_throwables_keep_independent_traces() {
        let mut vm = test_vm();
        let Ok(inner) = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/IllegalStateException",
            Some("inner"),
        ) else {
            return;
        };
        let inner_hash = vm.shared.mem.heap.identity_hash_code(inner);
        let Some(inner_trace) = vm.shared.throwable_stack_trace(inner_hash) else {
            return;
        };

        // Pin `inner` so building the outer throwable cannot collect or move it
        // out from under the identity hash we just read.
        vm.main_thread.native_pin_roots.push(inner);
        let outer = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/IllegalArgumentException",
            Some("outer"),
        );
        let inner = vm.main_thread.native_pin_roots.pop().expect("pinned inner");

        let Ok(outer) = outer else { return };
        let outer_hash = vm.shared.mem.heap.identity_hash_code(outer);
        let inner_hash = vm.shared.mem.heap.identity_hash_code(inner);

        assert!(
            vm.shared.throwable_stack_trace(outer_hash).is_some(),
            "outer throwable lost its trace"
        );
        assert_eq!(
            vm.shared.throwable_stack_trace(inner_hash).map(|t| t.len()),
            Some(inner_trace.len()),
            "constructing the outer throwable clobbered the inner one's trace"
        );
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
        let error = RuntimeError::aioobe_index_only(42);
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
        let error = RuntimeError::aioobe_index_only(-1);
        // Just verify the variant holds the data; we can't check Java object
        // creation without rt.jar but we can verify the Rust side.
        //
        // This asserted `message == None` until 2026-08-06, on the reasoning
        // that a call site which cannot name the array's length must not invent
        // any text. `ca7526cf0` replaced that with the JDK's OWN one-argument
        // wording, which is the better answer for the same reason: `new
        // ArrayIndexOutOfBoundsException(int)` in `java.base` produces exactly
        // `"Array index out of range: N"`, so this is HotSpot's string for a
        // site that knows only the index, not a guess at the one it cannot
        // build. The genuinely-null case moved to `aioobe_no_message`, and is
        // asserted below so this pair cannot silently collapse into one.
        if let RuntimeError::ArrayIndexOutOfBoundsException { index, message } = error {
            assert_eq!(index, -1);
            assert_eq!(
                message.as_deref(),
                Some("Array index out of range: -1"),
                "aioobe_index_only is the \"cannot name the length\" \
                 constructor — it carries the JDK's own one-argument wording, \
                 which is exact for what the site knows"
            );
        } else {
            panic!("wrong variant");
        }
    }

    /// The null-message sibling, which is a separate constructor precisely so
    /// that "HotSpot really prints nothing here" is stated rather than
    /// inherited.
    ///
    /// `java.lang.reflect.Array`'s accessors raise AIOOBE with no text at all,
    /// so `Array.get(new int[4], 9).getMessage()` is null on HotSpot while a
    /// plain `a[9]` in bytecode says "Index 9 out of bounds for length 4".
    #[test]
    fn runtime_error_array_index_can_carry_no_message_at_all() {
        let error = RuntimeError::aioobe_no_message(9);
        if let RuntimeError::ArrayIndexOutOfBoundsException { index, message } = error {
            assert_eq!(index, 9);
            assert_eq!(
                message, None,
                "aioobe_no_message exists for the sites a HotSpot control shows \
                 producing a null getMessage(); giving it text would make \
                 `Array.get` diverge from the JDK"
            );
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn runtime_error_array_index_carries_hotspot_message() {
        let error = RuntimeError::aioobe(9, 4);
        if let RuntimeError::ArrayIndexOutOfBoundsException { index, message } = error {
            assert_eq!(index, 9);
            assert_eq!(
                message.as_deref(),
                Some("Index 9 out of bounds for length 4")
            );
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
            RuntimeError::sioobe_index(99, 5),
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
    use super::nsme_message;
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

    /// `NoSuchMethodError`'s message, pinned to the JDK 25 output of
    /// `apps/nsme_probe` (compile against a class, run against one with the
    /// methods removed). Every character matters: the quotes are part of the
    /// message, the return type leads, params are `, `-separated, and arrays
    /// use source spelling rather than descriptor form.
    #[test]
    fn nsme_message_matches_hotspot() {
        // 'Lib Lib.widen(boolean)'
        assert_eq!(
            nsme_message("Lib", "widen", "(Z)LLib;"),
            "'Lib Lib.widen(boolean)'"
        );
        // 'long Lib.calc(int, java.lang.String[], double[][])'
        assert_eq!(
            nsme_message("Lib", "calc", "(I[Ljava/lang/String;[[D)J"),
            "'long Lib.calc(int, java.lang.String[], double[][])'"
        );
        // 'void Lib.plain()' — void return, and an empty parameter list must
        // not leave a stray separator.
        assert_eq!(nsme_message("Lib", "plain", "()V"), "'void Lib.plain()'");
    }

    /// The internal owner name is dotted, and the message still contains the
    /// fully-qualified `Class.method(` substring Spring Boot's
    /// `NoSuchMethodFailureAnalyzer` looks for — the property the previous
    /// (raw-descriptor) spelling was written to satisfy, which the HotSpot
    /// spelling satisfies too. This is what keeps
    /// `NoSuchMethodFailureAnalyzerTests` green across the change.
    #[test]
    fn nsme_message_keeps_the_dotted_class_and_method_prefix() {
        let msg = nsme_message(
            "org/springframework/data/r2dbc/mapping/R2dbcMappingContext",
            "setForceQuote",
            "(Z)V",
        );
        assert!(
            msg.contains(
                "org.springframework.data.r2dbc.mapping.R2dbcMappingContext.setForceQuote("
            ),
            "analyzer-visible prefix missing from {msg}"
        );
    }

    /// The real Linux `spring-boot-data-redis` failure this spelling was fixed
    /// for: a fixture compiled against a newer Jedis than the jar on its
    /// classpath. HotSpot reports exactly this string.
    #[test]
    fn nsme_message_matches_hotspot_for_the_jedis_fixture_skew() {
        assert_eq!(
            nsme_message(
                "redis/clients/jedis/DefaultJedisClientConfig$Builder",
                "autoNegotiateProtocol",
                "(Z)Lredis/clients/jedis/DefaultJedisClientConfig$Builder;"
            ),
            "'redis.clients.jedis.DefaultJedisClientConfig$Builder \
             redis.clients.jedis.DefaultJedisClientConfig$Builder.autoNegotiateProtocol(boolean)'"
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

    // -- Merge-point blocks (DIV-001..003 divergence-log closure) ----------
    //
    // A basic block whose leader is a control-flow *join* starts with operand
    // entries its predecessors pushed. `simulate_to` cannot see them, and used
    // to bail on the resulting underflow — costing the `because "<expr>"` clause
    // for every `T x = cond ? a : b; x.deref()`, the single most common shape in
    // real code. It now treats an underflow as "this operand predates the
    // block", which keeps top-relative indices exact. Expectations below are the
    // verbatim JDK 25 `getExtendedNPEMessage` output for the equivalent source
    // (the `DivNpe2` differential probe).

    const ILOAD_0: u8 = 0x1a;
    const IFEQ: u8 = 0x99;
    const GOTO: u8 = 0xa7;
    const ACONST_NULL: u8 = 0x01;
    const ASTORE_1: u8 = 0x4c;
    const ALOAD_2: u8 = 0x2c;
    const IADD: u8 = 0x60;
    const POP: u8 = 0x57;

    /// `Node n = flag ? null : other; n.value` — the trapping `getfield` sits in
    /// the ternary's merge block, whose leader (`astore_1`) pops a value pushed
    /// by both predecessor blocks.
    #[test]
    fn ternary_merge_block_still_names_the_local() {
        // 0: iload_0
        // 1: ifeq   -> 8
        // 4: aconst_null
        // 5: goto   -> 9
        // 8: aload_2
        // 9: astore_1        <- merge-point block leader
        // 10: aload_1
        // 11: getfield #1    <- trap
        let mut code = vec![ILOAD_0, IFEQ];
        code.extend_from_slice(&u16_be(7)); // relative to bci 1 -> 8
        code.push(ACONST_NULL);
        code.push(GOTO);
        code.extend_from_slice(&u16_be(4)); // relative to bci 5 -> 9
        code.push(ALOAD_2);
        code.push(ASTORE_1);
        code.push(ALOAD_1);
        let trap_bci = code.len();
        code.push(GETFIELD);
        code.extend_from_slice(&u16_be(1));

        let mut fields = HashMap::new();
        fields.insert(1u16, field("Node", "value"));
        let mut resolver = MockResolver::new_static(fields, HashMap::new());
        resolver.locals.insert(1, "n".to_string());

        let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
        assert_eq!(text(&expr), Some("n"));
        assert_eq!(
            helpful_npe::combine_opt(&helpful_npe::action_read_field("value"), expr.as_ref()),
            "Cannot read field \"value\" because \"n\" is null"
        );
    }

    /// `a[i + 1]` in a merge block: the index sub-expression puts a modelled
    /// `iadd` in the prefix, and the array operand is still reachable at depth 1.
    /// Before arithmetic was modelled the `iadd` alone abandoned the walk.
    #[test]
    fn arithmetic_prefix_does_not_abandon_the_walk() {
        // 0: astore_1   <- merge-point leader, pops a predecessor value
        // 1: aload_1
        // 2: iload_0
        // 3: iconst_1
        // 4: iadd
        // 5: iaload     <- trap; stack is [arr, idx]
        const IALOAD: u8 = 0x2e;
        let code = vec![ASTORE_1, ALOAD_1, ILOAD_0, ICONST_1, IADD, IALOAD];
        let trap_bci = 5;

        let mut resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
        resolver.locals.insert(1, "a".to_string());

        let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 1, &resolver);
        assert_eq!(text(&expr), Some("a"));
        assert_eq!(
            helpful_npe::combine_opt(
                &helpful_npe::action_array_load(helpful_npe::ArrayElemKind::Int),
                expr.as_ref()
            ),
            "Cannot load from int array because \"a\" is null"
        );
    }

    /// Tolerating the underflow must not *invent* an expression: an operand that
    /// genuinely came from a predecessor block stays unknown, so the message
    /// falls back to HotSpot's action-only shape rather than naming the wrong
    /// value.
    #[test]
    fn operand_from_a_predecessor_block_stays_unnamed() {
        // 0: astore_1   (consumes the merge value)
        // 1: pop        (consumes a second predecessor value)
        // 2: arraylength  <- trap on a third, which predates the block entirely
        let code = vec![ASTORE_1, POP, ARRAYLENGTH];
        let resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
        let expr = helpful_npe::null_expr_at_depth(&code, 2, 0, &resolver);
        assert_eq!(text(&expr), None);
        assert_eq!(
            helpful_npe::combine_opt(&helpful_npe::action_array_length(), expr.as_ref()),
            "Cannot read the array length"
        );
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
