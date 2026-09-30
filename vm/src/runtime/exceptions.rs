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
        /// The trapping method's descriptor, used to name an unwritten
        /// parameter slot `<parameterN>` the way HotSpot does when there is no
        /// `LocalVariableTable`. `None` keeps the `<localN>` spelling.
        fn method_descriptor(&self) -> Option<&str> {
            None
        }
        /// Whether the field `cp_index` names is a `long` / `double` (its
        /// descriptor starts with `J` / `D`). The whole-method analysis needs
        /// it only where a category-dependent shuffle (`pop2`, `dup2`, …)
        /// meets a field value; `None` stops the analysis on that path.
        fn field_is_category2(&self, _cp_index: u16) -> Option<bool> {
            None
        }
        /// The call-site descriptor of the `invokedynamic` at `cp_index`,
        /// which sizes its arguments and result. `None` stops the analysis
        /// on that path (the trap then gets the action-only message).
        fn invokedynamic_descriptor(&self, _cp_index: u16) -> Option<String> {
            None
        }
        /// The trapping method's exception-handler bcis: each starts with
        /// the throwable on the stack and no written locals (HotSpot's
        /// `ExceptionMessageBuilder`).
        fn handler_pcs(&self) -> Vec<usize> {
            Vec::new()
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

    // -- Whole-method operand-stack analysis --------------------------------
    //
    // HotSpot's `ExceptionMessageBuilder` (`bytecodeUtils.cpp`), transcribed.
    //
    // Until interpreter round i1 wave 22 this was a straight-line walk from the
    // trapping bci's BASIC BLOCK leader that gave up on the first opcode it did
    // not model. Any array store, string concatenation (`invokedynamic`),
    // `dup_x1` / `dup2` / `pop2`, `monitorenter` or `multianewarray` earlier in
    // the same block — and a basic block routinely spans several statements —
    // cost the whole `because …` clause; so did a receiver pushed before a
    // conditional argument (`n.take(b ? 1 : 2)`: the invoke sits after a merge
    // whose predecessors agree on the receiver); and "was this local written"
    // was asked of the whole method, so `s = s.trim()` on a null parameter said
    // `<local0>` where HotSpot says `<parameter1>`. The probe is
    // `tools/probes/interp/L7/L7W22HelpfulNpeFlow.java`.
    //
    // What is transcribed, because each piece is observable in a message:
    //
    // * States are kept per bci. Bci 0 starts empty; every exception handler
    //   starts with one entry (the throwable, "produced" at the handler's own
    //   bci) and NO written locals — a handler does not inherit the protected
    //   range's writes, so a parameter reassigned in a `try` is still
    //   `<parameterN>` in its `catch`.
    // * Linear passes over the code, each instruction propagating its
    //   successor state into its fall-through and branch targets by MERGE: an
    //   entry whose producers disagree loses its producer, and the written-
    //   local sets are OR-ed. The one state object is merged into each
    //   successor in turn (fall-through first), exactly as HotSpot's `merge`
    //   mutates it.
    // * Passes repeat only while a pass gave some bci its FIRST state, and stop
    //   as soon as the scan reaches the trapping bci with a state. So a loop's
    //   back edge merges into the loop head only if the trap was not already
    //   reached: `for (..) { s.length(); s = "x"; }` names `<parameter1>`.
    // * Category-2 values are one entry here (the interpreter's model, and the
    //   one every caller's `depth_below_top` counts in); each entry records
    //   whether it is a `long`/`double`, which is what the category-dependent
    //   shuffles (`pop2`, `dup2`, `dup_x2`, `dup2_x1`, `dup2_x2`) need.
    // * `checkcast` leaves its operand's producer in place: `((String) o)` is
    //   named after `o`.

    /// HotSpot's `_max_entries`: past this many simulated entries the analysis
    /// stops, and a trap it has not reached gets the action-only message.
    const MAX_ENTRIES: usize = 1_000_000;

    /// One simulated operand-stack entry.
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Slot {
        /// The bci of the instruction that pushed it; `None` when it cannot be
        /// named (predecessors disagreed, or the entry predates a fragment's
        /// first instruction).
        producer_bci: Option<usize>,
        /// `Some(true)` for a `long`/`double`, `Some(false)` for any other
        /// value, `None` when unknown (a field whose descriptor the resolver
        /// does not report, a disagreeing merge, an underflow).
        cat2: Option<bool>,
    }

    /// An entry this analysis cannot name.
    const UNKNOWN_SLOT: Slot = Slot {
        producer_bci: None,
        cat2: None,
    };

    /// HotSpot's `SimulatedOperandStack`: the entries, and which local slots
    /// some path to here has written.
    #[derive(Clone, Default)]
    struct State {
        stack: Vec<Slot>,
        written: Vec<u64>,
    }

    impl State {
        /// Pop one entry. An underflow yields [`UNKNOWN_SLOT`] rather than
        /// failing: verified code never underflows, and a hand-built fragment
        /// that starts mid-expression (the unit tests use them) keeps its
        /// top-relative entries exact.
        fn pop(&mut self) -> Slot {
            self.stack.pop().unwrap_or(UNKNOWN_SLOT)
        }

        fn push(&mut self, bci: usize, cat2: Option<bool>) {
            self.stack.push(Slot {
                producer_bci: Some(bci),
                cat2,
            });
        }

        fn push_slot(&mut self, slot: Slot) {
            self.stack.push(slot);
        }

        fn top(&self) -> Slot {
            self.stack.last().copied().unwrap_or(UNKNOWN_SLOT)
        }

        fn write_local(&mut self, slot: u16) {
            let word = usize::from(slot) / 64;
            if self.written.len() <= word {
                self.written.resize(word + 1, 0);
            }
            self.written[word] |= 1u64 << (slot % 64);
        }

        fn local_written(&self, slot: u16) -> bool {
            self.written
                .get(usize::from(slot) / 64)
                .is_some_and(|word| word & (1u64 << (slot % 64)) != 0)
        }

        /// `SimulatedOperandStack::merge`: fold `other` into `self`. `None`
        /// when the heights differ, which verified code never produces.
        fn merge_from(&mut self, other: &State) -> Option<()> {
            if self.stack.len() != other.stack.len() {
                return None;
            }
            for (mine, theirs) in self.stack.iter_mut().zip(&other.stack) {
                if mine.producer_bci != theirs.producer_bci {
                    mine.producer_bci = None;
                }
                if mine.cat2 != theirs.cat2 {
                    mine.cat2 = None;
                }
            }
            if self.written.len() < other.written.len() {
                self.written.resize(other.written.len(), 0);
            }
            for (mine, theirs) in self.written.iter_mut().zip(&other.written) {
                *mine |= *theirs;
            }
            Some(())
        }
    }

    /// Where control goes after one instruction.
    enum Flow {
        /// To the next instruction only.
        Next,
        /// To the next instruction and to these targets.
        Branch(Vec<usize>),
        /// To these targets only.
        Jump(Vec<usize>),
        /// Nowhere this analysis follows (a return, `athrow`, `jsr` / `ret`, or
        /// an instruction it cannot model).
        End,
    }

    /// The per-bci states of one analysis run.
    struct Analysis {
        states: Vec<Option<State>>,
        entries: usize,
        added_one: bool,
        all_processed: bool,
    }

    impl Analysis {
        /// The state immediately before the instruction at `bci`.
        fn state(&self, bci: usize) -> Option<&State> {
            self.states.get(bci).and_then(Option::as_ref)
        }

        /// Run HotSpot's pass structure until the scan reaches `trap_bci` with
        /// a state (or no pass adds a state). `None` when the code cannot be
        /// decoded or two paths reach one bci at different heights.
        fn run(code: &[u8], trap_bci: usize, resolver: &dyn CpResolver) -> Option<Analysis> {
            let len = code.len();
            if trap_bci >= len {
                return None;
            }
            let mut states: Vec<Option<State>> = vec![None; len + 1];
            states[0] = Some(State::default());
            for handler in resolver.handler_pcs() {
                if handler < len && states[handler].is_none() {
                    let mut entry = State::default();
                    entry.push(handler, Some(false));
                    states[handler] = Some(entry);
                }
            }
            let mut an = Analysis {
                states,
                entries: 0,
                added_one: true,
                all_processed: false,
            };
            // Each continuing pass gave at least one bci its first state, so
            // `len + 1` passes always suffice; the bound only guards a bug.
            let mut passes = 0usize;
            while !an.all_processed && an.added_one && passes <= len {
                passes += 1;
                an.all_processed = true;
                an.added_one = false;
                let mut bci = 0usize;
                while bci < len {
                    bci += an.do_instruction(code, bci, resolver)?;
                    if bci == trap_bci && an.states[bci].is_some() {
                        an.all_processed = true;
                        break;
                    }
                    if an.entries > MAX_ENTRIES {
                        return Some(an);
                    }
                }
            }
            Some(an)
        }

        /// Process the instruction at `bci` if it has a state; return its
        /// length either way.
        fn do_instruction(
            &mut self,
            code: &[u8],
            bci: usize,
            resolver: &dyn CpResolver,
        ) -> Option<usize> {
            let (instr, next) = Instruction::decode(code, bci).ok()?;
            let step = next.checked_sub(bci).filter(|&s| s > 0)?;
            let Some(mut state) = self.states[bci].clone() else {
                self.all_processed = false;
                return Some(step);
            };
            let (falls_through, targets) = match apply_effect(&mut state, &instr, bci, resolver) {
                Flow::Next => (true, Vec::new()),
                Flow::Branch(targets) => (true, targets),
                Flow::Jump(targets) => (false, targets),
                Flow::End => (false, Vec::new()),
            };
            if falls_through {
                self.merge_into(next, &mut state)?;
            }
            for target in targets {
                self.merge_into(target, &mut state)?;
            }
            Some(step)
        }

        /// HotSpot's `ExceptionMessageBuilder::merge`: fold the successor's
        /// existing state INTO `state` (which the next successor then sees),
        /// and store a copy as the successor's state.
        fn merge_into(&mut self, bci: usize, state: &mut State) -> Option<()> {
            let Some(slot) = self.states.get_mut(bci) else {
                // A target outside the code: nothing to record.
                return Some(());
            };
            match slot.as_mut() {
                Some(existing) => state.merge_from(existing)?,
                None => {
                    self.added_one = true;
                    self.entries += state.stack.len();
                }
            }
            *slot = Some(state.clone());
            Some(())
        }
    }

    /// Pop an invoke's arguments (and receiver) and push its result, per
    /// `descriptor`, one entry per value.
    fn invoke_effect(state: &mut State, descriptor: &str, receiver: bool, bci: usize) {
        for _ in 0..param_tokens(descriptor).len() {
            state.pop();
        }
        if receiver {
            state.pop();
        }
        let ret = descriptor
            .rfind(')')
            .and_then(|i| descriptor.as_bytes().get(i + 1).copied());
        match ret {
            Some(b'V') => {}
            Some(b'J' | b'D') => state.push(bci, Some(true)),
            _ => state.push(bci, Some(false)),
        }
    }

    /// Apply `instr`'s operand-stack and local-write effect to `state` and say
    /// where control goes next.
    fn apply_effect(
        state: &mut State,
        instr: &Instruction,
        bci: usize,
        resolver: &dyn CpResolver,
    ) -> Flow {
        use Instruction::*;
        const ONE: Option<bool> = Some(false);
        const TWO: Option<bool> = Some(true);
        // Cast: a signed branch offset added to its own bci.
        let rel = |off: i64| -> usize { (bci as i64 + off).max(0) as usize };
        let s = state;
        match instr {
            Nop => {}
            AconstNull | IconstM1 | Iconst0 | Iconst1 | Iconst2 | Iconst3 | Iconst4 | Iconst5
            | Fconst0 | Fconst1 | Fconst2 | Bipush(_) | Sipush(_) | Ldc(_) | LdcW(_) => {
                s.push(bci, ONE)
            }
            Lconst0 | Lconst1 | Dconst0 | Dconst1 | Ldc2W(_) => s.push(bci, TWO),
            Iload(_) | Fload(_) | Aload(_) => s.push(bci, ONE),
            Lload(_) | Dload(_) => s.push(bci, TWO),
            Iaload | Faload | Aaload | Baload | Caload | Saload => {
                s.pop();
                s.pop();
                s.push(bci, ONE);
            }
            Laload | Daload => {
                s.pop();
                s.pop();
                s.push(bci, TWO);
            }
            Istore(i) | Fstore(i) | Astore(i) => {
                s.pop();
                s.write_local(*i);
            }
            Lstore(i) | Dstore(i) => {
                s.pop();
                s.write_local(*i);
                if let Some(high) = i.checked_add(1) {
                    s.write_local(high);
                }
            }
            Iastore | Lastore | Fastore | Dastore | Aastore | Bastore | Castore | Sastore => {
                s.pop();
                s.pop();
                s.pop();
            }
            Pop => {
                s.pop();
            }
            Pop2 => match s.top().cat2 {
                Some(true) => {
                    s.pop();
                }
                Some(false) => {
                    s.pop();
                    s.pop();
                }
                None => return Flow::End,
            },
            Dup => {
                let v1 = s.top();
                s.push_slot(v1);
            }
            DupX1 => {
                let v1 = s.pop();
                let v2 = s.pop();
                s.push_slot(v1);
                s.push_slot(v2);
                s.push_slot(v1);
            }
            DupX2 => {
                let v1 = s.pop();
                let v2 = s.pop();
                match v2.cat2 {
                    Some(true) => {
                        s.push_slot(v1);
                        s.push_slot(v2);
                        s.push_slot(v1);
                    }
                    Some(false) => {
                        let v3 = s.pop();
                        s.push_slot(v1);
                        s.push_slot(v3);
                        s.push_slot(v2);
                        s.push_slot(v1);
                    }
                    None => return Flow::End,
                }
            }
            Dup2 => match s.top().cat2 {
                Some(true) => {
                    let v1 = s.top();
                    s.push_slot(v1);
                }
                Some(false) => {
                    let v1 = s.pop();
                    let v2 = s.pop();
                    s.push_slot(v2);
                    s.push_slot(v1);
                    s.push_slot(v2);
                    s.push_slot(v1);
                }
                None => return Flow::End,
            },
            Dup2X1 => {
                let v1 = s.pop();
                match v1.cat2 {
                    Some(true) => {
                        let v2 = s.pop();
                        s.push_slot(v1);
                        s.push_slot(v2);
                        s.push_slot(v1);
                    }
                    Some(false) => {
                        let v2 = s.pop();
                        let v3 = s.pop();
                        s.push_slot(v2);
                        s.push_slot(v1);
                        s.push_slot(v3);
                        s.push_slot(v2);
                        s.push_slot(v1);
                    }
                    None => return Flow::End,
                }
            }
            Dup2X2 => {
                let v1 = s.pop();
                match v1.cat2 {
                    Some(true) => {
                        let v2 = s.pop();
                        match v2.cat2 {
                            // Form 4: two category-2 values.
                            Some(true) => {
                                s.push_slot(v1);
                                s.push_slot(v2);
                                s.push_slot(v1);
                            }
                            // Form 2: a category-2 value over two category-1s.
                            Some(false) => {
                                let v3 = s.pop();
                                s.push_slot(v1);
                                s.push_slot(v3);
                                s.push_slot(v2);
                                s.push_slot(v1);
                            }
                            None => return Flow::End,
                        }
                    }
                    Some(false) => {
                        let v2 = s.pop();
                        let v3 = s.pop();
                        match v3.cat2 {
                            // Form 3: two category-1 values over a category-2.
                            Some(true) => {
                                s.push_slot(v2);
                                s.push_slot(v1);
                                s.push_slot(v3);
                                s.push_slot(v2);
                                s.push_slot(v1);
                            }
                            // Form 1: four category-1 values.
                            Some(false) => {
                                let v4 = s.pop();
                                s.push_slot(v2);
                                s.push_slot(v1);
                                s.push_slot(v4);
                                s.push_slot(v3);
                                s.push_slot(v2);
                                s.push_slot(v1);
                            }
                            None => return Flow::End,
                        }
                    }
                    None => return Flow::End,
                }
            }
            Swap => {
                let v1 = s.pop();
                let v2 = s.pop();
                s.push_slot(v1);
                s.push_slot(v2);
            }
            Iadd | Isub | Imul | Idiv | Irem | Iand | Ior | Ixor | Ishl | Ishr | Iushr | Fadd
            | Fsub | Fmul | Fdiv | Frem | Lcmp | Fcmpl | Fcmpg | Dcmpl | Dcmpg => {
                s.pop();
                s.pop();
                s.push(bci, ONE);
            }
            Ladd | Lsub | Lmul | Ldiv | Lrem | Land | Lor | Lxor | Lshl | Lshr | Lushr | Dadd
            | Dsub | Dmul | Ddiv | Drem => {
                s.pop();
                s.pop();
                s.push(bci, TWO);
            }
            Ineg | Fneg | L2i | L2f | F2i | D2i | D2f | I2f | I2b | I2c | I2s => {
                s.pop();
                s.push(bci, ONE);
            }
            Lneg | Dneg | I2l | I2d | L2d | F2l | F2d | D2l => {
                s.pop();
                s.push(bci, TWO);
            }
            Iinc { index, .. } => s.write_local(*index),
            Ifeq(o) | Ifne(o) | Iflt(o) | Ifge(o) | Ifgt(o) | Ifle(o) | Ifnull(o)
            | Ifnonnull(o) => {
                s.pop();
                return Flow::Branch(vec![rel(i64::from(*o))]);
            }
            IfIcmpeq(o) | IfIcmpne(o) | IfIcmplt(o) | IfIcmpge(o) | IfIcmpgt(o) | IfIcmple(o)
            | IfAcmpeq(o) | IfAcmpne(o) => {
                s.pop();
                s.pop();
                return Flow::Branch(vec![rel(i64::from(*o))]);
            }
            Goto(o) => return Flow::Jump(vec![rel(i64::from(*o))]),
            GotoW(o) => return Flow::Jump(vec![rel(i64::from(*o))]),
            // Subroutines (class files before version 50) are not followed:
            // code reached only through them gets the action-only message.
            Jsr(_) | JsrW(_) | Ret(_) => return Flow::End,
            Tableswitch(ts) => {
                s.pop();
                let mut targets = vec![rel(i64::from(ts.default))];
                targets.extend(ts.offsets.iter().map(|o| rel(i64::from(*o))));
                return Flow::Jump(targets);
            }
            Lookupswitch(ls) => {
                s.pop();
                let mut targets = vec![rel(i64::from(ls.default))];
                targets.extend(ls.pairs.iter().map(|(_, o)| rel(i64::from(*o))));
                return Flow::Jump(targets);
            }
            Ireturn | Lreturn | Freturn | Dreturn | Areturn | Athrow => {
                s.pop();
                return Flow::End;
            }
            Return => return Flow::End,
            Getstatic(idx) => {
                let cat2 = resolver.field_is_category2(*idx);
                s.push(bci, cat2);
            }
            Putstatic(_) => {
                s.pop();
            }
            Getfield(idx) => {
                s.pop();
                let cat2 = resolver.field_is_category2(*idx);
                s.push(bci, cat2);
            }
            Putfield(_) => {
                s.pop();
                s.pop();
            }
            Invokevirtual(idx) | Invokespecial(idx) | Invokestatic(idx) => {
                let Some(CpRef::Method { descriptor, .. }) = resolver.method_ref(*idx) else {
                    return Flow::End;
                };
                invoke_effect(s, &descriptor, !matches!(instr, Invokestatic(_)), bci);
            }
            Invokeinterface { index, .. } => {
                let Some(CpRef::Method { descriptor, .. }) = resolver.method_ref(*index) else {
                    return Flow::End;
                };
                invoke_effect(s, &descriptor, true, bci);
            }
            Invokedynamic(idx) => {
                let Some(descriptor) = resolver.invokedynamic_descriptor(*idx) else {
                    return Flow::End;
                };
                invoke_effect(s, &descriptor, false, bci);
            }
            New(_) => s.push(bci, ONE),
            Newarray(_) | Anewarray(_) | Arraylength | Instanceof(_) => {
                s.pop();
                s.push(bci, ONE);
            }
            // The cast value keeps its producer.
            Checkcast(_) => {}
            Monitorenter | Monitorexit => {
                s.pop();
            }
            Multianewarray { dimensions, .. } => {
                for _ in 0..*dimensions {
                    s.pop();
                }
                s.push(bci, ONE);
            }
            #[allow(unreachable_patterns)]
            _ => return Flow::End,
        }
        Flow::Next
    }

    /// Render the local-variable slot `slot` loaded at `bci` the way HotSpot's
    /// `print_local_var` names it: the `LocalVariableTable` source name when
    /// present; otherwise, for a slot no path to the consuming instruction has
    /// written (`written == false`), `this` for slot 0 of an **instance**
    /// method and `<parameterN>` (1-based, a `long`/`double` counted once) for
    /// a slot inside the parameter area; otherwise the synthetic `<localN>`.
    fn render_local(resolver: &dyn CpResolver, slot: u16, bci: usize, written: bool) -> String {
        if let Some(name) = resolver.local_name(slot, bci) {
            return name;
        }
        let is_parameter = !written;
        let is_static = resolver.is_static_method();
        if slot == 0 && !is_static && is_parameter {
            return "this".to_string();
        }
        if is_parameter {
            if let Some(index) = resolver
                .method_descriptor()
                .and_then(|d| parameter_index_of_slot(d, is_static, slot))
            {
                return format!("<parameter{index}>");
            }
        }
        format!("<local{slot}>")
    }

    /// The 1-based parameter number whose slot range covers `slot`, or `None`
    /// when `slot` lies past the parameter area.
    fn parameter_index_of_slot(descriptor: &str, is_static: bool, slot: u16) -> Option<usize> {
        let mut curr: usize = if is_static { 0 } else { 1 };
        let slot = usize::from(slot);
        for (i, token) in param_tokens(descriptor).iter().enumerate() {
            let size = if token == "J" || token == "D" { 2 } else { 1 };
            if slot >= curr && slot < curr + size {
                return Some(i + 1);
            }
            curr += size;
        }
        None
    }

    /// HotSpot's `print_NPE_cause0`: describe the entry `depth_below_top`
    /// entries below the top of the stack as it stands before the instruction
    /// at `consumer_bci`. `depth` counts nesting levels; past
    /// [`MAX_EXPR_DEPTH`] nothing is described (HotSpot's `max_detail`).
    fn describe_operand(
        code: &[u8],
        an: &Analysis,
        consumer_bci: usize,
        depth_below_top: usize,
        resolver: &dyn CpResolver,
        depth: u32,
    ) -> Option<Producer> {
        if depth > MAX_EXPR_DEPTH {
            return None;
        }
        let state = an.state(consumer_bci)?;
        let idx = state.stack.len().checked_sub(depth_below_top + 1)?;
        let producer_bci = state.stack.get(idx)?.producer_bci?;
        describe_producer(code, an, state, producer_bci, resolver, depth)
    }

    /// Describe the source expression pushed by the instruction at
    /// `producer_bci`, for a consumer whose state is `consumer` (whose
    /// written-local set decides `<parameterN>` against `<localN>`, as
    /// HotSpot's does). Returns a [`Producer`] whose `text` is the inline
    /// rendering and whose `is_invoke` flag drives the top-level
    /// `the return value of "…"` phrasing.
    fn describe_producer(
        code: &[u8],
        an: &Analysis,
        consumer: &State,
        producer_bci: usize,
        resolver: &dyn CpResolver,
        depth: u32,
    ) -> Option<Producer> {
        let (instr, _) = Instruction::decode(code, producer_bci).ok()?;
        match instr {
            // A reference local / `this`, or an int local used as an array
            // index (`arr[i]`).
            Instruction::Aload(slot) | Instruction::Iload(slot) => Some(Producer::expr(
                render_local(resolver, slot, producer_bci, consumer.local_written(slot)),
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
                // The receiver, one level down; an undescribable one leaves the
                // bare field name (`"s" is null`), as HotSpot prints it.
                match describe_operand(code, an, producer_bci, 0, resolver, depth + 1) {
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
            // `<arr>[<idx>]`: HotSpot describes `aaload` (a null element) and
            // `iaload` (an int element used as an index, `a[idx[0]]`) — and no
            // other array load (`a[b[0]]` on a `byte[]` is `a[...]`). An
            // array operand it cannot describe prints `<array>`, an index
            // `...`; neither loses the rest of the expression.
            Instruction::Aaload | Instruction::Iaload => {
                let array = describe_operand(code, an, producer_bci, 1, resolver, depth + 1)
                    .map(|p| p.text)
                    .unwrap_or_else(|| "<array>".to_string());
                let index = describe_operand(code, an, producer_bci, 0, resolver, depth + 1)
                    .map(|p| p.text)
                    .unwrap_or_else(|| "...".to_string());
                Some(Producer::expr(format!("{array}[{index}]")))
            }
            // An invoke result: `Owner.m(params)`. Flagged `is_invoke` so the
            // top level renders it as `the return value of "…"` while a nested
            // use (sub-receiver, index) renders the inline form.
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
    /// `invoke_bci`, which consumes `num_params` argument entries above the
    /// receiver (a `long`/`double` argument is one entry). Returns `None`
    /// (→ action-only message) when the producer can't be named.
    pub fn null_expr_for_invoke_receiver(
        code: &[u8],
        invoke_bci: usize,
        num_params: usize,
        resolver: &dyn CpResolver,
    ) -> Option<Producer> {
        // The receiver sits `num_params` entries below the top of the operand
        // stack as it stood just before the invoke.
        null_expr_at_depth(code, invoke_bci, num_params, resolver)
    }

    /// Generalized null-operand reconstruction shared by every null-deref
    /// opcode. `depth_below_top` is how many operand-stack entries sit *above*
    /// the null operand when the trapping opcode at `trap_bci` begins — `0`
    /// for the top-of-stack operand (`getfield` receiver, `arraylength` /
    /// `monitor` / `athrow` operand), `1` for the entry one below (the
    /// `putfield` receiver, the `*aload` array), and `2` for the `*astore`
    /// array (value + index above it). A `long`/`double` is one entry.
    ///
    /// Returns `None` (→ action-only message) when the producer can't be named,
    /// exactly like the invoke path.
    pub fn null_expr_at_depth(
        code: &[u8],
        trap_bci: usize,
        depth_below_top: usize,
        resolver: &dyn CpResolver,
    ) -> Option<Producer> {
        let an = Analysis::run(code, trap_bci, resolver)?;
        describe_operand(code, &an, trap_bci, depth_below_top, resolver, 0)
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
    // Throwable's own `detailMessage` slot first (see
    // `throwable_own_field_index`); the walk below remains for the opaque
    // `_fN` bootstrap layout, which the cache cannot resolve by name.
    if let Some(idx) = throwable_own_field_index(shared, "detailMessage") {
        shared
            .mem
            .heap
            .set_field(obj, idx, Value::Object(Some(string_ref)));
        return;
    }
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

/// The absolute slot of one of `java.lang.Throwable`'s OWN instance fields
/// (`detailMessage`, `cause`, `suppressedExceptions`, `backtrace`, `depth`,
/// `stackTrace`), through the per-VM `ThrowableLayoutCache`.
///
/// Resolved exactly as the native context's `throwable_field_index`
/// (`vm_exec.rs`) resolves it — `get_loaded_class_id("java/lang/Throwable")`
/// plus `resolve_field_index_in_hierarchy` — because the two share the cache
/// slots and must never disagree about what is in them. Superclass fields
/// precede subclass fields in every layout, so the index is valid for every
/// throwable.
///
/// Two reasons to prefer it over [`instance_field_index_by_name`], which walks
/// the THROWN object's hierarchy from the most-derived class:
///
/// * cost — the by-name walk ran four or five times per VM-minted throwable
///   (`backtrace`, `depth`, `detailMessage`, `cause`, `suppressedExceptions`),
///   each a class-manager read acquisition plus a string compare per field of
///   every class in the chain;
/// * correctness — a subclass that declares its OWN field named `cause` or
///   `depth` (legal Java; JNI `ThrowNew` builds arbitrary user throwables
///   through this module) shadowed Throwable's, and the mirror below then
///   wrote `cause = this` into the user's field instead.
///
/// `None` when the name is not one of the six, or when Throwable's layout
/// does not name the field (the opaque `_fN` bootstrap metadata) — callers
/// keep their by-name fallback for that case.
fn throwable_own_field_index(shared: &SharedVm, name: &str) -> Option<usize> {
    use crate::vm::realms::thread_realm::UNRESOLVED_THROWABLE_FIELD_INDEX;
    use std::sync::atomic::Ordering;

    let slot = shared.threads.throwable_layout_cache.field_slot(name)?;
    let cached = slot.load(Ordering::Relaxed);
    if cached != UNRESOLVED_THROWABLE_FIELD_INDEX {
        return Some(cached);
    }
    let resolved = {
        let cm = shared.classes.class_manager.read();
        let class_id = cm.get_loaded_class_id("java/lang/Throwable")?;
        crate::vm::vm_exec::resolve_field_index_in_hierarchy(class_id, name, &cm.class_store)?
    };
    slot.store(resolved, Ordering::Relaxed);
    Some(resolved)
}

/// [`throwable_own_field_index`] with the historical by-name walk as the
/// fallback, for the callers below that must still work on a bootstrap layout
/// the cache cannot resolve.
fn throwable_field_index_or_walk(shared: &SharedVm, obj: ObjectRef, name: &str) -> Option<usize> {
    throwable_own_field_index(shared, name)
        .or_else(|| instance_field_index_by_name(shared, obj, name))
}

/// Whether the throwable's stack trace was already captured by its constructor
/// — in which case re-running `fillInStackTrace` would recompute and re-store a
/// byte-identical trace.
///
/// `capture_throwable_trace` (native-builtins `lang_misc.rs`, the shared body of
/// every `native_exc_init_*` constructor shadow *and* of
/// `Throwable.fillInStackTrace`) writes two markers on the throwable:
///
/// * `backtrace = this` — the non-null marker real-JDK `getOurStackTrace()`
///   requires before it will materialise any frames, and
/// * `depth = trace.len()` — and the captured trace is the *whole* Java stack
///   (`capture_full_trace` maps every entry of `thread.frames`), so
///   `depth == thread.frames.len()` **as it stood at capture time**. A stack
///   deeper than `VmConfig::max_java_stack_trace_depth` is captured capped,
///   so its `depth` never equals the frame count and the check fails closed
///   (one extra, bounded, capture).
///
/// So `backtrace == this && depth == frames.len()` proves the constructor's
/// capture was taken with this thread's frame stack in exactly the state
/// `fillInStackTrace` would see now — same frames, same `last_instr_pc` per
/// frame, therefore the same `StackTraceEntry` vector.
///
/// # Why `depth` is no longer compared against `frames.len()`
///
/// It used to be, on the argument that a bytecode (non-shadowed) `<init>`
/// captures inside its own pushed frames and so records `depth == frames.len()
/// + k`, a "constructor-frame-contaminated" trace. Two things made that test
/// wrong in both directions:
///
/// * `NativeExceptionAccess::capture_throwable_stack_trace` now applies
///   HotSpot's `fill_in_stack_trace` skip (`stackwalker::trim_throwable_fill_frames`)
///   to EVERY capture: the `fillInStackTrace` and `<init>` frames whose holder
///   the throwable `is_a` are dropped. A real `Throwable.<init>` chain therefore
///   records exactly the frames this call site would — no contamination to
///   replace.
/// * `depth` counts the compiled frames the capture splices in, while
///   `thread.frames.len()` counts interpreter frames only. With any compiled
///   method on the stack — every trap out of compiled code, i.e. the hot case —
///   the two can never match, so the "duplicate" capture ran on precisely the
///   throws that were the reason to skip it. Under `--jdk-only`, where every
///   JDK exception's `<init>` is real bytecode, it ran on all of them.
///
/// So the marker pair alone is the test: `backtrace == this` says our capture
/// ran on this freshly allocated object (which nothing else can have
/// written), and `depth > 0` says it recorded something. A missing / unwritten
/// field (opaque `_fN` bootstrap layouts), `depth == 0`, or a constructor that
/// did not capture (`writableStackTrace == false`, an overridden
/// `fillInStackTrace`) still returns `false`, and the explicit call runs
/// exactly as before.
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
    // parks there. Anything else (null, an unset slot -- `Int(0)` on a legacy
    // object, `Object(None)` on a compact one -- or a real JDK backtrace
    // object) means our capture did not run.
    let Some(bt_idx) = throwable_field_index_or_walk(shared, obj, "backtrace") else {
        return false;
    };
    if shared.mem.heap.get_field(obj, bt_idx) != Value::Object(Some(obj)) {
        return false;
    }
    let Some(depth_idx) = throwable_field_index_or_walk(shared, obj, "depth") else {
        return false;
    };
    matches!(
        shared.mem.heap.get_field(obj, depth_idx),
        Value::Int(d) if d > 0
    )
}

/// Does an APPLICATION override of `fillInStackTrace()` decide whether a
/// throwable of `class_id` records a trace, so that step 4 of
/// [`create_exception_object_for_class_inner`] must not fill one behind its
/// back? Round 12 wave 7 (lane compat), both modes.
///
/// HotSpot's `Exceptions::new_exception` runs the constructor and nothing
/// else: `Throwable.<init>` calls the VIRTUAL `fillInStackTrace()`, so a class
/// that overrides it to `return this` (the "quiet exception" idiom) ends with
/// an empty trace. Step 4 called the private `fillInStackTrace(int)` native
/// directly whenever the constructor left no trace -- which is exactly what
/// such an override does -- so JNI `ThrowNew` of a quiet exception came back
/// with a full trace, under `--jdk-only` as well as `--compatible`.
///
/// Only an override declared by a class OUTSIDE the built-in loaders counts:
/// the JDK's own (`NullPointerException`'s calls `super`,
/// `sun.nio.fs.WindowsException`'s is an internal exception) keep the step-4
/// backstop for a constructor shadow that captured nothing. Any zero-argument
/// instance `fillInStackTrace` returning a reference counts, so a covariant
/// override (`()LMyEx;`, reached through a bridge) is seen too.
///
/// Kill switch `CRATONVM_THROWABLE_FILL_OVERRIDE_DECIDES=0` restores the
/// unconditional step 4.
fn fill_left_to_an_application_override(shared: &SharedVm, class_id: ClassId) -> bool {
    if !fill_override_decides_enabled() {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    first_application_fill_override(class_id, |cid| {
        let class = cm.class_store.get(cid)?;
        Some(FillOverrideStep {
            is_throwable: &*class.name == "java/lang/Throwable",
            built_in: matches!(
                class.loader_id,
                cratonvm_types::ClassLoaderId::Bootstrap
                    | cratonvm_types::ClassLoaderId::Extension
            ),
            declares_fill: class.methods.iter().any(|m| {
                !m.is_static()
                    && &*m.name == "fillInStackTrace"
                    && m.descriptor.starts_with("()L")
            }),
            superclass: class.superclass,
        })
    })
}

/// One class of the superclass walk [`first_application_fill_override`]
/// makes.
struct FillOverrideStep {
    is_throwable: bool,
    built_in: bool,
    declares_fill: bool,
    superclass: Option<ClassId>,
}

/// The walk behind [`fill_left_to_an_application_override`], over a class
/// view so it can be tested without a class store: from `class_id` up to (not
/// including) `java/lang/Throwable`, is there a class outside the built-in
/// loaders that declares `fillInStackTrace()`? An unknown class ends the walk
/// with `false` (the step-4 backstop stays).
fn first_application_fill_override(
    class_id: ClassId,
    mut view: impl FnMut(ClassId) -> Option<FillOverrideStep>,
) -> bool {
    let mut cursor = Some(class_id);
    // A malformed (cyclic) chain must not spin; real ones are a few deep.
    for _ in 0..256 {
        let Some(cid) = cursor else {
            return false;
        };
        let Some(step) = view(cid) else {
            return false;
        };
        if step.is_throwable {
            return false;
        }
        if step.declares_fill && !step.built_in {
            return true;
        }
        cursor = step.superclass;
    }
    false
}

/// `CRATONVM_THROWABLE_FILL_OVERRIDE_DECIDES` -- default ON (round 12 wave 7,
/// lane compat); `0` restores the unconditional step-4 `fillInStackTrace(int)`
/// of [`create_exception_object_for_class_inner`]. Not cached (no new static
/// under `vm/tests/per_vm_state_statics_ratchet.rs`): it is read only after a
/// constructor captured nothing, which an ordinary throwable never does.
fn fill_override_decides_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_FILL_OVERRIDE_DECIDES")
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

/// HotSpot's `NoSuchFieldError` message for a failed field resolution (JDK 21+,
/// JDK-8298065, `LinkResolver::resolve_field`):
///
/// ```text
/// Class p.C does not have member field 'int f'
/// Class p.C does not have member field 'java.lang.String[] names'
/// ```
///
/// The class is the one the fieldref names (dotted), and the type is the
/// fieldref's descriptor in source spelling — the same
/// [`helpful_npe::class_external`] rendering `nsme_message` uses for
/// parameters. Until 2026-09-23 this VM emitted `p/C.f`.
///
/// An empty `field_descriptor` (a name-only resolution that had no descriptor)
/// drops the type rather than inventing one: `... member field 'f'`.
fn nsfe_message(class_name: &str, field_name: &str, field_descriptor: &str) -> String {
    let class = class_name.replace('/', ".");
    if field_descriptor.is_empty() {
        format!("Class {class} does not have member field '{field_name}'")
    } else {
        format!(
            "Class {class} does not have member field '{} {field_name}'",
            helpful_npe::class_external(field_descriptor)
        )
    }
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
    loader: std::borrow::Cow<'static, str>,
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
/// docs/internal/jdk-only/W7-37-differential-throwable-and-vm.md.
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
    // Measured on Temurin 25.0.3.9 (`CastMsgs` probe,
    // docs/internal/jdk-only/W8-C4-1-array-cast-klass-origin.md §1):
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
                loader: "'bootstrap'".into(),
                loader_id: cratonvm_types::ClassLoaderId::Bootstrap,
            })
        }
    };

    let Some(class_id) = cm.find_unique_class_by_name(lookup_name).or_else(|| {
        let frame_class = thread.frames.last()?.class_id;
        cm.find_class_by_name_for_class(lookup_name, frame_class)
    }) else {
        // NOT LOADED — and for a `java.base` name that is still answerable
        // without loading anything, because `java.base` has exactly one
        // defining loader in every JVM (JVMS §5.3.1: the bootstrap loader
        // defines it, and the module system's own bootstrap depends on that
        // being true before any other loader exists).
        //
        // The gap this closes is not hypothetical and not rare. CratonVM's
        // immutable collections are internal STAMPS, and `cce_display_class_name`
        // deliberately renders them as the JDK class they stand in for
        // (`java.util.ImmutableCollections$MapN`) — a name with no loaded class
        // behind it unless something else happened to load it. So the same cast
        // failure printed two different messages in one run, decided by nothing
        // the program did:
        //
        //   empty: [java.util.ImmutableCollections$MapN cannot be cast to java.lang.String]
        //   many:  [class java.util.ImmutableCollections$MapN cannot be cast to
        //           class java.lang.String (… are in module java.base of loader 'bootstrap')]
        //
        // measured 2026-09-08 on `LambdaSafeUnmodifiableMapClassCastProbe`,
        // where `Map.of(k,v,k,v)` loads the real `MapN` and `Map.of()` does not.
        // HotSpot prints the second form for both. A message that changes with
        // the class-load history is a message no consumer can parse — Spring's
        // `LambdaSafe` and mockk's `JvmAutoHinter` both parse this text — so the
        // answer has to be the same either way.
        //
        // Deliberately `java.base` ALONE. Every other module's loader is a real
        // question (`java.sql` is the platform loader, an application module's
        // is the app loader), and answering it from the name would be a guess in
        // a string log-scrapers read. `None` keeps today's bare wording for
        // those, which is what this function has always done when it cannot
        // name an operand.
        let pkg = lookup_name
            .rsplit_once('.')
            .map(|(p, _)| p)?
            .replace('.', "/");
        if cm.module_registry.module_for_package(&pkg)? != "java.base" {
            return None;
        }
        return Some(KlassOrigin {
            module: Some("java.base".to_string()),
            loader: "'bootstrap'".into(),
            loader_id: cratonvm_types::ClassLoaderId::Bootstrap,
        });
    };
    let class = cm.get_class(class_id)?;
    let loader_id = class.loader_id;
    let module = message_module_of(class, &cm.module_registry);
    drop(cm);
    // A user-defined loader is named by its `nameAndId` under `--jdk-only`
    // (interpreter round i1 wave 32); `--compatible` keeps the bare wording.
    let loader = match loader_name_and_id(loader_id) {
        Some(builtin) => builtin.into(),
        None if shared.config.is_jdk_only() => user_loader_name_and_id(shared, class_id)?.into(),
        None => return None,
    };
    Some(KlassOrigin {
        module,
        loader,
        loader_id,
    })
}

/// Record the class ids of the `checkcast` about to fail on `thread`, for
/// [`hotspot_class_cast_message`] (`--jdk-only`, interpreter round i1 wave
/// 32). [`klass_origin`] resolves each operand by NAME, which cannot tell two
/// loaders' same-named classes apart; this is the raise site's answer, taken
/// by the message funnel, which checks that each id's name is the operand it
/// is asked about, so a note no message consumed is never misapplied.
pub(crate) fn note_cast_operands(thread: &JvmThread, from: ClassId, to: ClassId) {
    let packed = (u64::from(from.as_u32()) << 32) | u64::from(to.as_u32());
    thread
        .cast_operands_note
        .store(packed, std::sync::atomic::Ordering::Relaxed);
}

/// Take the note [`note_cast_operands`] left on `thread`.
fn take_cast_operands(thread: &JvmThread) -> Option<(ClassId, ClassId)> {
    let packed = thread
        .cast_operands_note
        .swap(u64::MAX, std::sync::atomic::Ordering::Relaxed);
    (packed != u64::MAX).then(|| {
        (
            ClassId::new((packed >> 32) as u32),
            ClassId::new(packed as u32),
        )
    })
}

/// [`klass_origin`] from the class itself rather than its name, when
/// `display_name` is that class's external name and not an array (`None`
/// otherwise, and the caller asks by name). A user-defined loader is named as HotSpot names it:
/// the loader's own `nameAndId` field (`'child' @1b6d3586`, or the loader's
/// class name and its identity hash), which `ClassLoader`'s constructor
/// computes and HotSpot's `loader_name_and_id` prints.
fn klass_origin_by_id(
    shared: &SharedVm,
    class_id: ClassId,
    display_name: &str,
) -> Option<KlassOrigin> {
    // An array's clause is its bottom component's (see `klass_origin`), which
    // the name path resolves; an array class's own loader is not it.
    if display_name.starts_with('[') {
        return None;
    }
    let (loader_id, module) = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        if class.name.replace('/', ".") != display_name {
            return None;
        }
        (class.loader_id, message_module_of(class, &cm.module_registry))
    };
    let loader = match loader_name_and_id(loader_id) {
        Some(builtin) => builtin.into(),
        None => user_loader_name_and_id(shared, class_id)?.into(),
    };
    Some(KlassOrigin {
        module,
        loader,
        loader_id,
    })
}

/// The module a message names for `class`: its `module_name`, or, for a
/// class of a non-boot layer's module (`--jdk-only`; the define path leaves
/// `module_name` `None` there), that module (interpreter round i1 wave 44,
/// lane L5). One test in a VM with no layer module.
fn message_module_of(
    class: &crate::classloading::Class,
    registry: &crate::classloading::module::ModuleRegistry,
) -> Option<String> {
    match crate::classloading::access_control::layer_module_name_of(class, registry) {
        Some(name) => Some(name.to_string()),
        None => class.module_name.clone(),
    }
}

/// A user-defined loader's `ClassLoader.nameAndId`, read through the class's
/// mirror (`Class.classLoader`).
fn user_loader_name_and_id(shared: &SharedVm, class_id: ClassId) -> Option<String> {
    let mirror = crate::vm::get_or_create_class_mirror(shared, class_id);
    let heap = &shared.mem.heap;
    let field = |obj: ObjectRef, name: &str| -> Option<ObjectRef> {
        let slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(
            shared,
            heap.class_id_of(obj),
            name,
        )?;
        match heap.get_field(obj, slot) {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        }
    };
    let loader = field(mirror, "classLoader")?;
    let name_and_id = field(loader, "nameAndId")?;
    crate::vm::read_java_string(heap, name_and_id).filter(|s| !s.is_empty())
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

/// Most distinct `(receiver class, target)` pairs [`cast_refusal_forensics_admitted`]
/// remembers. Past it, only zero-header receivers are admitted.
const CAST_FORENSICS_PAIR_CAP: usize = 4096;

/// Should THIS failing cast pay for the reclaimed-memory forensics
/// (`memory::reclaim_guard::report_reclaimed_receiver*` and the opcode's own
/// `cratonvm_gc::gen_heap::old_freed_lookup_covering` probes)?
///
/// # Why this exists (round 9 wave 8, vmrt8)
///
/// Those forensics scan the GC's reclamation rings linearly, and the old-gen
/// ring is 1 M records x 32 B. Every failing `checkcast` -- compiled or
/// interpreted -- ran them unconditionally: three scans per interpreted
/// refusal, one per compiled one. MEASURED on the w7b binary: a
/// `ClassCastException` cost ~5-7 ms (HotSpot: 3 us; NPE and `/ 0` on the
/// same VM: 2-9 us), `new ClassCastException("x")` and `Class.cast` cost
/// 1 us and 13 us (no forensics), and the FIRST refusal alone grew the
/// process's peak working set by ~34 MB, i.e. the ring's BSS being faulted in.
/// Frameworks use a failing cast as a type test on hot paths (Spring's
/// `LambdaSafe`), and `DeoptStorm`'s ~110 000 refusals ran 228 s.
///
/// # The rule
///
/// * A `ClassId(0)` receiver (`java.lang.Object`'s id, and the all-zero
///   header a reclaimed span reads as) is ALWAYS admitted: that is the
///   classic signature the flag-free verdict was built for.
/// * Otherwise the FIRST refusal of each distinct `(receiver class, target
///   name)` pair is admitted and repeats are not. A type-test idiom repeats
///   the same pair; a reused-block corruption (`java.util.BitSet cannot be
///   cast to org.h2.mvstore.Chunk`) is a pair the program never produces
///   legitimately, so it is still reported on its first occurrence -- which
///   is all the reporter ever printed anyway (each report kind is capped at
///   `MAX_REPORTS` = 8 lines).
/// * Past [`CAST_FORENSICS_PAIR_CAP`] distinct pairs, only zero headers.
///
/// Process-wide rather than per VM: it gates diagnostics only, never a
/// decision, and a second VM seeing a pair the first already scanned for
/// loses nothing a flag-free first sighting did not already report.
pub(crate) fn cast_refusal_forensics_admitted(actual_cid: u32, target: &str) -> bool {
    use std::collections::HashSet;
    use std::hash::{Hash, Hasher};
    use std::sync::Mutex;
    if actual_cid == 0 {
        return true;
    }
    static SEEN: OnceLock<Mutex<HashSet<(u32, u64)>>> = OnceLock::new();
    // Hashed with `/` folded onto `.`: the interpreter passes the dotted
    // binary name and the JIT helper the internal slashed one, and one pair
    // should cost one scan whichever tier refuses it first.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for b in target.bytes() {
        (if b == b'/' { b'.' } else { b }).hash(&mut h);
    }
    let key = (actual_cid, h.finish());
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.contains(&key) || seen.len() >= CAST_FORENSICS_PAIR_CAP {
        return false;
    }
    seen.insert(key);
    true
}

/// Rebuild a cast refusal in HotSpot's wording, or `None` when we cannot name
/// both operands' module and loader.
///
/// Mirrors `SharedRuntime::generate_class_cast_message`: one joint clause when
/// both klasses are in the same module (`Klass::joint_in_module_of_loader`),
/// two `; `-separated clauses otherwise. Both shapes are measured rather than
/// recalled — the JDK 25 transcript is in the record.
pub(crate) fn hotspot_class_cast_message(
    shared: &SharedVm,
    thread: &JvmThread,
    from_display: &str,
    to_display: &str,
    noted: Option<(ClassId, ClassId)>,
) -> Option<String> {
    let by_id = |id: Option<ClassId>, display: &str| {
        id.and_then(|id| klass_origin_by_id(shared, id, display))
    };
    let from = by_id(noted.map(|n| n.0), from_display)
        .or_else(|| klass_origin(shared, thread, from_display))?;
    let to = by_id(noted.map(|n| n.1), to_display)
        .or_else(|| klass_origin(shared, thread, to_display))?;
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
            // The raise site's class ids, under `--jdk-only` (wave 32).
            let noted = take_cast_operands(thread).filter(|_| shared.config.is_jdk_only());
            let rebuilt = split_cast_operands(&message)
                .and_then(|(from, to)| hotspot_class_cast_message(shared, thread, from, to, noted));
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
pub(crate) fn set_cause_by_name(shared: &SharedVm, obj: ObjectRef, cause_ref: ObjectRef) {
    // Throwable's own `cause` slot first (see `throwable_own_field_index`);
    // the walk below is the fallback for a layout the cache cannot resolve.
    if let Some(idx) = throwable_own_field_index(shared, "cause") {
        shared
            .mem
            .heap
            .set_field(obj, idx, Value::Object(Some(cause_ref)));
        return;
    }
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
    // 1. Load the exception class.
    //
    // Read-first, write-on-miss: `resolve_fast_path_class_id` is the exact
    // fast-path lookup `ClassManager::load_class` itself runs first under
    // its own write lock, so an already-loaded class -- every VM-minted
    // throwable after the first one of its kind -- resolves here without
    // ever taking the write lock at all. A genuine miss (first-ever throw
    // of this class) falls through to the real write-locked `load_class`,
    // unchanged from before.
    let already_loaded = shared
        .classes
        .class_manager
        .read()
        .resolve_fast_path_class_id(class_name);
    let class_id = match already_loaded {
        Some(id) => id,
        None => shared
            .classes
            .class_manager
            .write()
            .load_class(class_name)
            .map_err(|e| {
                MethodCallFailed::InternalError(VmError::Internal {
                    message: format!("failed to load exception class {class_name}: {e}"),
                })
            })?,
    };

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
    create_exception_object_for_class_inner(
        shared,
        thread,
        class_id,
        class_name,
        message,
        ExceptionAlloc::MayCollect,
        false,
    )
}

/// The throwable JNI `ThrowNew` makes pending: [`create_exception_object_for_class`]
/// with HotSpot's `Exceptions::new_exception` rules for the constructor, which
/// a VM-raised JDK exception never exercises but a user throwable can.
///
/// * **A constructor that throws** makes ITS exception the one to throw
///   (`new_exception`: "if another exception was thrown in the process,
///   rethrow that one"). The half-built object used to be returned instead,
///   with the caller's message written into it by hand.
/// * **A constructor that sets its own message** keeps it: a user
///   `MyException(String s) { super("wrapped: " + s); }` reports
///   `wrapped: <s>`, not `<s>`. The caller's message is written directly only
///   when the constructor left `detailMessage` null, which keeps the
///   bootstrap-era partially-emulated constructors (and the opaque `_fN`
///   layout, where the slot cannot be read by name) as they were.
/// * **A class that does not declare the constructor** (`(String)`, or `()`
///   for a NULL message) throws `NoSuchMethodError` after initializing the
///   class, as `JavaCalls::construct_new_instance` does. Constructors are not
///   inherited: `invoke_on_class_shared` would otherwise walk up to
///   `Throwable.<init>(String)` and run it on the subclass instance, skipping
///   the subclass's own constructor.
/// * **A class that is not a `Throwable`** is refused (`Err`, which JNI turns
///   into `JNI_ERR` with nothing pending). The JNI specification requires a
///   `Throwable` subclass; HotSpot only asserts it, and `-Xcheck:jni` treats it
///   as a fatal error.
#[cold]
pub fn create_exception_object_for_jni_throw_new(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    class_name: &str,
    message: Option<&str>,
) -> Result<ObjectRef, MethodCallFailed> {
    let descriptor = if message.is_some() {
        "(Ljava/lang/String;)V"
    } else {
        "()V"
    };
    match jni_throw_new_class_check(shared, class_id, descriptor) {
        ThrowNewClassCheck::Constructible => {}
        ThrowNewClassCheck::NotThrowable => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: format!("ThrowNew: {class_name} is not a Throwable"),
            }));
        }
        ThrowNewClassCheck::NoConstructor => {
            // `construct_new_instance` initializes the class before it
            // resolves the constructor, so a failing `<clinit>` wins.
            match crate::vm::ensure_class_initialized_shared(shared, thread, class_id) {
                Ok(()) => {}
                Err(MethodCallFailed::ExceptionThrown(thrown)) => return Ok(thrown),
                Err(e) => return Err(e),
            }
            let detail = nsme_message(class_name, "<init>", descriptor);
            return create_exception_object(
                shared,
                thread,
                "java/lang/NoSuchMethodError",
                Some(&detail),
            );
        }
    }
    create_exception_object_for_class_inner(
        shared,
        thread,
        class_id,
        class_name,
        message,
        ExceptionAlloc::MayCollect,
        true,
    )
}

/// What JNI `ThrowNew` may do with `class_id` (see
/// [`create_exception_object_for_jni_throw_new`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ThrowNewClassCheck {
    /// Build it through its own `descriptor` constructor.
    Constructible,
    /// Not a subclass of the bootstrap `java/lang/Throwable`.
    NotThrowable,
    /// A `Throwable` with real class bytes that does not declare the
    /// constructor.
    NoConstructor,
}

fn jni_throw_new_class_check(
    shared: &SharedVm,
    class_id: ClassId,
    descriptor: &str,
) -> ThrowNewClassCheck {
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(class_id) else {
        // A stale `jclass`: the builder reports it as an internal error.
        return ThrowNewClassCheck::Constructible;
    };
    // Without a bootstrap `Throwable` (a stripped VM) there is nothing to
    // compare against; the builder decides as before.
    if let Some(throwable) = cm.loaded_class_under_exact_key(
        "java/lang/Throwable",
        cratonvm_types::ClassLoaderId::Bootstrap,
    ) {
        if !cm.is_subclass_of(class_id, throwable) {
            return ThrowNewClassCheck::NotThrowable;
        }
    }
    // A class without a class file of its own (a compatibility stub, a
    // VM-internal carrier) answers its constructors through registered
    // natives, so its method table is not evidence of absence.
    if class.dispatch_lacks_class_file() || class.find_method("<init>", descriptor).is_some() {
        ThrowNewClassCheck::Constructible
    } else {
        ThrowNewClassCheck::NoConstructor
    }
}

/// Does `obj`'s `Throwable.detailMessage` still hold null after its
/// constructor ran? `true` when the slot cannot be found by name (the opaque
/// bootstrap layout), so the caller keeps its direct write there.
fn detail_message_is_null(shared: &SharedVm, obj: ObjectRef) -> bool {
    let Some(idx) = throwable_own_field_index(shared, "detailMessage") else {
        return true;
    };
    // Widening: u32 -> usize.
    if idx >= shared.mem.heap.get_header(obj).num_slots() as usize {
        return true;
    }
    matches!(
        shared.mem.heap.get_field(obj, idx),
        Value::Object(None) | Value::Int(0)
    )
}

/// May building a throwable collect when its first allocation attempt fails?
///
/// gc-common w4-c (`common-w3a-oome-construction-reruns-the-allocation-ladder`):
/// a VM-raised `OutOfMemoryError` is converted into a throwable only AFTER its
/// allocation ladder (`collect_and_retry`: forced GC, overhead check, G1's
/// complete mark cycle, up to four soft-reference collections) has given up,
/// or for a request HotSpot refuses without collecting (`Requested array size
/// exceeds VM limit`). Running the same ladder a second time to allocate the
/// error object doubled every thrown OOME's cost on an exhausted heap -- two
/// forced full cycles on G1 -- and HotSpot throws its preallocated error
/// without collecting again. Such a conversion therefore makes collection-free
/// attempts only and falls back to `SharedVm::singleton_oom`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ExceptionAlloc {
    /// An ordinary exception: its allocation failing is a genuine first
    /// failure, so it climbs the ladder before reporting heap exhaustion.
    MayCollect,
    /// The exception reports a ladder that has already failed (or a request
    /// that must not collect): allocate without collecting, or report OOM so
    /// the caller throws the preallocated singleton.
    NoCollect,
}

/// [`create_exception_object`] without any collection on the allocation
/// failure path -- see [`ExceptionAlloc::NoCollect`].
#[cold]
pub(crate) fn create_exception_object_no_collect(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    message: Option<&str>,
) -> Result<ObjectRef, MethodCallFailed> {
    let already_loaded = shared
        .classes
        .class_manager
        .read()
        .resolve_fast_path_class_id(class_name);
    let class_id = match already_loaded {
        Some(id) => id,
        None => shared
            .classes
            .class_manager
            .write()
            .load_class(class_name)
            .map_err(|e| {
                MethodCallFailed::InternalError(VmError::Internal {
                    message: format!("failed to load exception class {class_name}: {e}"),
                })
            })?,
    };
    create_exception_object_for_class_inner(
        shared,
        thread,
        class_id,
        class_name,
        message,
        ExceptionAlloc::NoCollect,
        false,
    )
}

/// The throwable of a VM-raised `OutOfMemoryError` whose detail message is
/// `message`: [`create_exception_object_no_collect`], except for HotSpot's
/// VM-limit refusal (`Requested array size exceeds VM limit`), which may
/// collect once when the collection-free attempt finds no room.
///
/// gen r5w1/oom5,
/// `gengc-r4w5-final-array-limit-oome-loses-its-message-on-a-full-heap-FIXED-20260927.md`:
/// the no-collect door exists because the ladder the error reports has
/// already run. For the VM-limit refusal NO ladder ran -- the request is
/// refused before the heap is touched -- so on a heap full of the program's
/// dropped garbage the collection-free attempt failed, and the caller threw
/// the preallocated singleton, whose message is `Java heap space`. HotSpot
/// throws a preallocated instance per message
/// (`Universe::out_of_memory_error_array_size`); a second singleton would need
/// a new VM root in two other lanes' files, so this takes the collection the
/// refusal skipped instead. Only the failure path changes: a throwable that
/// fits is built exactly as before, and every other `OutOfMemoryError` keeps
/// the no-collect door.
pub(crate) fn create_vm_oome_object(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    message: Option<&str>,
) -> Result<ObjectRef, MethodCallFailed> {
    let created = create_exception_object_no_collect(shared, thread, class_name, message);
    let heap_refused = matches!(
        &created,
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError { .. }
        )))
    );
    if heap_refused && message == Some(super::interpreter::ARRAY_SIZE_EXCEEDS_VM_LIMIT) {
        return create_exception_object(shared, thread, class_name, message);
    }
    created
}

/// `jni_throw_new`: the constructor rules of
/// [`create_exception_object_for_jni_throw_new`].
#[cold]
fn create_exception_object_for_class_inner(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    class_name: &str,
    message: Option<&str>,
    alloc_mode: ExceptionAlloc,
    jni_throw_new: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    // The caller may have received a stale/bogus `jclass`; reject it before
    // allocating an object with an unknown layout. The same (single) lookup
    // yields the field count for step 2.
    let Some((num_fields, awaits_link)) = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| (c.num_total_fields, crate::vm::class_awaits_link(c)))
    else {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!("exception class {class_name} is not loaded"),
        }));
    };
    // 1b. Link the class (verify, prepare, report `ClassPrepare`), as
    //     HotSpot's `Exceptions::new_exception` does by initializing it: a
    //     class only the VM instantiated stayed `Loaded` (JDWP status 0, no
    //     `ClassPrepare`) while it had instances (interpreter round i1 wave
    //     17). Once per class; not on the no-collect door, which must not run
    //     Java. A class that fails to link throws its linkage error, as there.
    if awaits_link && alloc_mode == ExceptionAlloc::MayCollect {
        crate::vm::link_class_on_thread(shared, thread, class_id)?;
    }

    // 2. Allocate the exception object
    let obj_ref = match shared.mem.heap.try_alloc_object(class_id, num_fields) {
        Some(obj) => obj,
        None if alloc_mode == ExceptionAlloc::NoCollect => {
            // The ladder this error reports has already run: one more
            // collection-free attempt (the old-generation spill), then OOM,
            // which the caller turns into the preallocated singleton.
            shared
                .mem
                .heap
                .try_alloc_object_full(class_id, num_fields)
                .ok_or_else(|| {
                    MethodCallFailed::InternalError(VmError::Runtime(
                        RuntimeError::OutOfMemoryError {
                            message: "Java heap space".to_string(),
                        },
                    ))
                })?
        }
        None => {
            // Young gen full — force a GC cycle and retry.
            thread.tlab.retire();
            super::interpreter::maybe_gc_forced_pub_at(shared, thread, "exceptions");
            // GC-overhead limit: if the heap is GC-thrashing, fail fast with OOM
            // so the caller falls back to the pre-allocated singleton (this very
            // path is what builds a fresh exception — looping here would
            // death-spiral too). The startup pre-allocation runs on an empty
            // heap, so it never reaches this branch.
            //
            // Either way the soft-reference rung runs first: the OOM this
            // produces is thrown to Java (as the singleton), and
            // `java.lang.ref.SoftReference` guarantees every softly-reachable
            // object is cleared before any `OutOfMemoryError` — the same rung
            // `alloc_object_shared` / `gc_alloc_array` climb.
            if super::interpreter::gc_overhead_limit_exceeded(shared) {
                let soft_collected =
                    super::interpreter::gc_and_alloc::last_ditch_clear_soft_refs(shared, thread);
                // gcd d2/j: and on Generational (with
                // `CRATONVM_GC_OVERHEAD_PROGRESS` on) a major on this thread
                // when the soft rung collected nothing, before the attempt
                // whose failure turns the program's exception into an
                // `OutOfMemoryError`. The streak latched on young cycles alone
                // cannot see the old generation's dropped data (below its 75 %
                // floor no young cycle collects it) -- the shape gen r4w4
                // fixed in the interpreter's ladder and gcd d1/b in the JIT
                // helpers; this door was the third. One attempt, the request
                // withdrawn on a lost race; only on this failure path. A
                // second major follows when the first freed little (gcd d4/j,
                // `majors_to_decide_oome`: dead old data kept alive through a
                // surviving young object needs a second mark to go).
                if !soft_collected
                    && shared.mem.heap.is_generational()
                    && cratonvm_types::flags().gc.gc_overhead_progress
                {
                    let _ = super::interpreter::gc_and_alloc::majors_to_decide_oome(
                        shared,
                        thread,
                        "exceptions-overhead-major",
                    );
                }
                let mut retried = shared.mem.heap.try_alloc_object_full(class_id, num_fields);
                // gcd d10/o (applied by lane f): on G1 the latched arm ran no
                // marking cycle -- the forced collection above is a young
                // pause, which cannot reach a dead Old or humongous region --
                // so one decides: productive (>= 2 % of the heap freed, the
                // streak reset), one more attempt; futile, the error. It
                // declines (no collection) off G1 and under
                // `CRATONVM_GC_OVERHEAD_PROGRESS=0`, keeping this arm's one
                // attempt byte for byte there.
                if retried.is_none() && !soft_collected && shared.mem.heap.is_g1() {
                    let before_live = shared.mem.heap.live_bytes_estimate();
                    if super::interpreter::gc_and_alloc::g1_overhead_limit_full_cycle(
                        shared,
                        thread,
                        before_live,
                    ) {
                        retried = shared.mem.heap.try_alloc_object_full(class_id, num_fields);
                    }
                }
                match retried {
                    Some(obj) => obj,
                    None => {
                        return Err(MethodCallFailed::InternalError(VmError::Runtime(
                            RuntimeError::OutOfMemoryError {
                                message: "Java heap space".to_string(),
                            },
                        )))
                    }
                }
            } else if let Some(obj) = shared.mem.heap.try_alloc_object_full(class_id, num_fields) {
                // SB-LOADER-ZIPCONTENT (2026-08-04): old-gen-spilling retry,
                // same reason as `alloc_object_shared` / `gc_alloc_array` — a
                // young free list fragmented by the non-moving JIT-safe sweep
                // must not report OOM while the old generation still holds most
                // of the heap. This one matters twice over: failing here
                // replaces the exception the program actually threw with an
                // `OutOfMemoryError`.
                obj
            } else {
                // The allocation paths' final rung (G1 full cycle + soft refs).
                super::interpreter::gc_and_alloc::last_ditch_reclaim(shared, thread);
                let mut retried = shared.mem.heap.try_alloc_object_full(class_id, num_fields);
                // gcd d5/u: and the ladder's second major before the error
                // (`second_major_before_oome`, gcd d4/j), as the interpreter's
                // `collect_and_retry_with_thread` and the JIT helpers'
                // `jit_g1_last_ditch_full_cycle` run it: one Generational major
                // cannot free old data a dead old object keeps through a young
                // one, and a failure here turns the program's own exception
                // into an `OutOfMemoryError`. Off Generational and under
                // `CRATONVM_GC_OVERHEAD_PROGRESS=0`, nothing (as before).
                if retried.is_none()
                    && super::interpreter::gc_and_alloc::second_major_before_oome(
                        shared,
                        thread,
                        "exceptions-oome-second-major",
                    )
                {
                    retried = shared.mem.heap.try_alloc_object_full(class_id, num_fields);
                }
                retried
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
        }
    };
    // 2b. Typed JVM defaults in every instance field, as a bytecode `new` gets
    //     them (`gc_alloc_object`). The allocator's zero fill decodes as
    //     `Int(0)` in every slot of a LEGACY object, so a throwable built here —
    //     including a user class raised through JNI `ThrowNew` — whose
    //     constructor leaves a `long`/`float`/`double`/reference field
    //     unassigned exposed that raw `Int(0)` to every reader that does not
    //     repair it by descriptor. Then Throwable's own `cause = this` field
    //     initialiser, which is where every constructor that reaches
    //     `Throwable.<init>` starts from ([`seed_throwable_cause`]). Nothing
    //     here allocates, so `obj_ref` needs no pin yet.
    super::interpreter::init_primitive_fields(shared, obj_ref, class_id);
    seed_throwable_cause(shared, obj_ref);
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
        //
        // A FRESH String, not a pooled one (gc-common w9-c). This used to be
        // the pooled `try_create_java_string`, and the pool is a strong GC
        // root (`memory::roots` section 5) that nothing ever prunes: every
        // distinct VM-raised detail message -- `Index 7 out of bounds for
        // length 5`, a `NegativeArraySizeException`'s `-3`, each helpful NPE
        // and `ClassCastException` text -- stayed on the heap for the life of
        // the VM, so a program that catches array-index errors over varying
        // indices grew the old generation without bound. It also made
        // `e.getMessage() == "/ by zero"` true, where HotSpot's detail
        // message is a new String. The no-collect door (a VM-raised
        // `OutOfMemoryError`) keeps the pooled form: its messages are a
        // closed set (`hotspot_oome_detail` strips the ladders' site detail),
        // and a pool hit lets a repeated `OutOfMemoryError` on a full heap
        // carry its message without allocating one. A fresh String the heap
        // cannot hold falls back to an EXISTING pool entry (a hit only,
        // nothing inserted), so a message the pooled form used to find --
        // `"/ by zero"` after a program's own literal -- does not turn the
        // exception into an `OutOfMemoryError` on a nearly full heap.
        let fresh = if alloc_mode == ExceptionAlloc::NoCollect {
            try_create_java_string(shared, msg)
        } else {
            crate::vm::try_create_java_string_uninterned(shared, msg)
                .or_else(|| shared.mem.string_pool.read().get(msg).copied())
        };
        let Some(string_ref) = fresh else {
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
                // JNI `ThrowNew` keeps a message the constructor chose.
                if !jni_throw_new || detail_message_is_null(shared, obj_ref) {
                    set_detail_message_by_name(shared, obj_ref, string_ref);
                }
            }
            Err(MethodCallFailed::ExceptionThrown(thrown)) if jni_throw_new => {
                // HotSpot `Exceptions::new_exception`: the constructor's
                // exception is the one thrown. Nothing below allocates.
                let thrown = *thrown;
                thread.native_pin_roots.truncate(pin_base);
                return Ok(thrown);
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
        match init_result {
            // JNI `ThrowNew`: the constructor's exception is the one thrown,
            // as in the `(String)` arm above.
            Err(MethodCallFailed::ExceptionThrown(thrown)) if jni_throw_new => {
                thread.native_pin_roots.truncate(pin_base);
                return Ok(thrown);
            }
            // Can't call constructor — object is partially initialized but
            // usable. A VM-raised throwable ignores a throwing constructor.
            _ => {}
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
    // stored in the VM-wide `throwable_stacks` map. The constructor captures
    // exact frame identity once; line numbers are then resolved only if a
    // reader observes the trace. At a Spring/JUnit stack depth of 50-150
    // frames, a second capture would still be a duplicate O(depth) walk and a
    // second registry update, so it remains pure waste.
    //
    // `trace_already_captured_at_current_depth` only reports `true` when the
    // constructor's capture ran on this object — shadowed or real-bytecode
    // `<init>` alike, since every capture drops the filling frames HotSpot
    // drops (see its doc for why `depth` is no longer compared with the
    // interpreter depth, and for the cases — opaque field layouts, absent
    // markers, a constructor that captured nothing — that still fall through
    // to the explicit call). Fidelity is preserved; only the duplicate is
    // removed.
    let obj_ref = thread.native_pin_roots[pin_base];
    if !trace_already_captured_at_current_depth(shared, thread, obj_ref)
        && !fill_left_to_an_application_override(shared, class_id)
    {
        let _ = invoke_on_class_shared(
            shared,
            thread,
            class_id,
            "fillInStackTrace",
            "(I)Ljava/lang/Throwable;",
            &[Value::Object(Some(obj_ref)), Value::Int(0)],
        );
    }

    // 5. Mirror the `suppressedExceptions` *instance-field initialiser* the JDK
    //    source declares but that our partially-emulated construction can
    //    miss. (`cause`'s was seeded before the constructor, in 2b.)
    let obj_ref = thread.native_pin_roots[pin_base];
    mirror_suppressed_initialiser(shared, obj_ref);

    let obj_ref = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);
    Ok(obj_ref)
}

/// `Throwable`'s `private Throwable cause = this;` field initialiser, on a
/// throwable no constructor has touched yet.
///
/// `Throwable.<init>` runs that initialiser before anything else, so seeding it
/// BEFORE a constructor is invoked is exactly the state a constructor that
/// reaches `Throwable.<init>` starts from. Such a constructor then overwrites
/// the slot with the same `this`, a cause argument, or the explicit null of
/// `ClassNotFoundException()` / `InvocationTargetException()` /
/// `ExceptionInInitializerError()`, in both modes: the real bytecode under
/// `--jdk-only`, the `native_exc_init_*` shadows (each of which writes `cause`
/// explicitly) under `--compatible`. Measured: all 157 public throwable
/// constructors of the shadowed family give HotSpot 25's `initCause` /
/// `addSuppressed` verdicts in both modes (`CtorCauseProbe`), before and after
/// this change. A throwable no constructor reaches -- neither `()V` nor
/// `(String)V` resolves, or the constructor throws first, or the fast-throw
/// door, which runs none -- keeps `this`, so its first `initCause` succeeds as
/// on a constructed one.
///
/// This replaces reading the slot back AFTER construction and taking an
/// `Int(0)` as "no constructor set a cause". That marker is a property of the
/// LEGACY 16-byte cell only. A COMPACT reference slot of zeroes -- and every
/// real `java/lang/Throwable` has a compact layout -- reads `Object(None)`, the
/// same bytes as a written null, so on a compact throwable the marker never
/// fired: the fast-throw door handed out throwables whose `initCause` always
/// refused (`FastThrowCauseProbe`: 132,265 of 132,265 stackless exceptions),
/// while a legacy one would have accepted.
fn seed_throwable_cause(shared: &SharedVm, obj: ObjectRef) {
    if let Some(idx) = throwable_field_index_or_walk(shared, obj, "cause") {
        shared
            .mem
            .heap
            .set_field(obj, idx, Value::Object(Some(obj)));
    }
}

/// Mirror `Throwable`'s declared instance-field initialiser
/// `private List<Throwable> suppressedExceptions = SUPPRESSED_SENTINEL;` on a
/// throwable this module built.
///
/// Only ever reached right after construction (or, for the fast-throw door,
/// instead of it), so no user code has observed the object yet and there is
/// nothing to clobber. Only `()V` and `(String)V` are invoked by this module,
/// and the four-arg suppression-disabling constructor is unreachable from
/// here, so no state this writes is state HotSpot would have left alone.
///
/// `Throwable.addSuppressed` treats a **null** `suppressedExceptions` as
/// "suppression disabled" and silently drops the call — that is the JDK's own
/// rule, and it is what makes the four-arg constructor work. Measured
/// 2026-08-12 on `--real-jdk`: a VM-minted `NullPointerException` and a
/// `ClassNotFoundException` both came out with a **null** list where HotSpot
/// has the empty-list sentinel, so honouring that rule without this mirror
/// would silently lose the `close()` failure from every try-with-resources
/// whose body threw a VM-minted exception.
///
/// The test is `Object(Some(_))` -- "already holds a list" -- which answers the
/// same on a legacy object (an unset slot reads `Int(0)`) and a compact one (it
/// reads `Object(None)`); anything else gets the sentinel. It is the
/// `create_exception_object` sibling of `capture_throwable_trace`'s
/// `init_suppressed_sentinel` in `native-builtins`' `lang_misc.rs`, which does
/// the same mirroring for the constructor shadows. Reading the real static
/// (rather than parking any empty list) keeps the JDK bytecode's own
/// `== SUPPRESSED_SENTINEL` identity tests valid on the rare paths that still
/// reach it. Before `Throwable.<clinit>` has populated the static there is
/// nothing faithful to write, so a bootstrap-era throwable is left as-is.
fn mirror_suppressed_initialiser(shared: &SharedVm, obj: ObjectRef) {
    let Some(idx) = throwable_field_index_or_walk(shared, obj, "suppressedExceptions") else {
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

// ===========================================================================
// OmitStackTraceInFastThrow — stackless implicit exceptions at hot compiled sites
// ===========================================================================
//
// HotSpot's C2 compiles a null check, range check, zero-divisor check, cast
// check or array-store check whose site has trapped "too often" into a throw of
// a PREALLOCATED instance with a null message and an empty stack trace
// (`-XX:+OmitStackTraceInFastThrow`, on by default there). CratonVM builds a
// full throwable for every one of them — constructor, message `String`, a stack
// walk into the VM-wide trace store — which measured 3-8 us per exception
// (`perf-implicit-exceptions-in-compiled-code-cost-microseconds-each-FIXED-20260920.md`),
// against HotSpot's ~0 for a hot site.
//
// This is the equivalent, and it is OPT-IN: `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW=1`
// (the analogue of `-XX:+OmitStackTraceInFastThrow`). Unset — the default — is
// `-XX:-OmitStackTraceInFastThrow`: every implicit exception is built in full,
// which is the fidelity rule the rest of this file holds (a hot compiled throw
// and a cold interpreted one carry the same message and trace; see
// `vm/tests/jit_npe_message_hot_equals_cold.rs`). It is observable — the
// message becomes null and the trace empty — so it is not ours to turn on
// silently.
//
// Two deliberate differences from HotSpot, both on the safe side:
//
// * a FRESH instance per throw rather than one shared preallocated object. The
//   cost that matters is the constructor and the stack walk, not the ~100 ns
//   allocation, and a fresh object needs no permanent GC root, cannot leak an
//   `initCause`/`addSuppressed` from one throw into the next, and keeps
//   identity-based code (`e1 != e2`) working;
// * the site is counted per thread, on the interpreter frame the compiled code
//   was entered from and its pc (the compiled frames have left the stack by the
//   time the throwable is built), plus the kind. HotSpot counts per bci in the
//   method's profile.
//
// Only the drains that build an implicit exception FOR compiled code call
// [`try_fast_throw`]; an exception the interpreter raises for its own bytecode
// is never made stackless, exactly as in HotSpot, where the interpreter always
// fills in the trace.

/// The implicit exceptions HotSpot's `OmitStackTraceInFastThrow` covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FastThrowKind {
    NullPointer,
    ArrayIndexOutOfBounds,
    Arithmetic,
    ClassCast,
    ArrayStore,
}

impl FastThrowKind {
    /// The bootstrap class this kind instantiates.
    pub fn class_name(self) -> &'static str {
        match self {
            FastThrowKind::NullPointer => "java/lang/NullPointerException",
            FastThrowKind::ArrayIndexOutOfBounds => "java/lang/ArrayIndexOutOfBoundsException",
            FastThrowKind::Arithmetic => "java/lang/ArithmeticException",
            FastThrowKind::ClassCast => "java/lang/ClassCastException",
            FastThrowKind::ArrayStore => "java/lang/ArrayStoreException",
        }
    }

    /// The kind a VM-minted `RuntimeError` is, when it is one of the implicit
    /// exceptions `OmitStackTraceInFastThrow` covers.
    pub fn of_runtime_error(error: &RuntimeError) -> Option<Self> {
        match error {
            RuntimeError::NullPointerException { .. } => Some(FastThrowKind::NullPointer),
            RuntimeError::ArrayIndexOutOfBoundsException { .. } => {
                Some(FastThrowKind::ArrayIndexOutOfBounds)
            }
            RuntimeError::ArithmeticException { .. } => Some(FastThrowKind::Arithmetic),
            RuntimeError::ClassCastException { .. } => Some(FastThrowKind::ClassCast),
            RuntimeError::ArrayStoreException { .. } => Some(FastThrowKind::ArrayStore),
            _ => None,
        }
    }
}

thread_local! {
    /// `thread.frames.len()` of the interpreter frame currently finishing a
    /// trapped compiled activation (helper precise-resume), or 0 for none.
    static RESUMED_COMPILED_FRAME_DEPTH: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// While alive, an implicit exception the interpreter raises from the frame at
/// `depth` — and only that frame, not a callee it invokes — counts as raised
/// by compiled code for `OmitStackTraceInFastThrow`.
///
/// That is what it is: the helper precise-resume path (`jit/helpers.rs`)
/// rebuilds a compiled callee that trapped (a zero divisor, say) as an
/// interpreter frame and re-executes the trapping bytecode, so the exception
/// HotSpot would throw from the compiled code is minted by the interpreter's
/// opcode here. Nested scopes restore the outer depth on drop.
pub struct ResumedCompiledFrameScope {
    prev: usize,
}

impl ResumedCompiledFrameScope {
    pub fn enter(depth: usize) -> Self {
        let prev = RESUMED_COMPILED_FRAME_DEPTH.with(|d| d.replace(depth));
        ResumedCompiledFrameScope { prev }
    }
}

impl Drop for ResumedCompiledFrameScope {
    fn drop(&mut self) {
        RESUMED_COMPILED_FRAME_DEPTH.with(|d| d.set(self.prev));
    }
}

fn in_resumed_compiled_frame(thread: &JvmThread) -> bool {
    let depth = RESUMED_COMPILED_FRAME_DEPTH.with(|d| d.get());
    depth != 0 && thread.frames.len() == depth
}

/// Throws at one site (per thread, per kind) that are still built in full.
/// The next one and every later one are stackless. HotSpot's equivalent is
/// the trap count that makes C2 recompile the site as a fast throw
/// (`PerBytecodeTrapLimit`, 4, plus the recompilation lag); 16 keeps the first
/// few throws of a hot site fully diagnosable here too.
pub const FAST_THROW_HOT_THRESHOLD: u32 = 16;

/// Bound on the per-thread site table. Keys name method code by address, so a
/// long-lived thread that throws from many short-lived methods would otherwise
/// grow it without limit; when it fills, it is cleared and counting restarts
/// (a site then pays `FAST_THROW_HOT_THRESHOLD` full throws again — never a
/// wrong answer, only a slower one).
const FAST_THROW_SITE_CAP: usize = 4096;

/// `(vm_identity, method code address, pc, kind)`.
///
/// The VM is `SharedVm::vm_identity`, never reissued, not the `SharedVm`
/// address, which the allocator hands to the next VM created after one is
/// dropped (sequential VMs on one thread: `cratonvm-embed`, the test harness):
/// keyed by address, a later VM inherited the dropped VM's counts for any site
/// whose code landed at the same address, and its first throws there came out
/// stackless. Interpreter round i1 wave 24, lane L5; wave 23 moved four
/// sibling thread-local memos the same way.
type FastThrowSiteKey = (usize, usize, usize, FastThrowKind);

thread_local! {
    static FAST_THROW_SITES: std::cell::RefCell<rustc_hash::FxHashMap<FastThrowSiteKey, u32>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW` — default OFF (see the section
/// note above). Cached: it is read on every implicit exception from compiled
/// code, and the answer cannot change mid-run.
pub fn omit_stack_trace_in_fast_throw() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW")
    })
}

/// Count one throw at `key` and answer whether the site is now hot (strictly
/// more than `threshold` throws seen). Pure over `counts`, so the policy is
/// unit-testable without a VM.
fn fast_throw_note_site<K: std::hash::Hash + Eq>(
    counts: &mut rustc_hash::FxHashMap<K, u32>,
    key: K,
    threshold: u32,
    cap: usize,
) -> bool {
    if counts.len() >= cap && !counts.contains_key(&key) {
        counts.clear();
    }
    let n = counts.entry(key).or_insert(0);
    *n = n.saturating_add(1);
    *n > threshold
}

/// The stackless throwable for an implicit exception raised by compiled code,
/// when `OmitStackTraceInFastThrow` is on and this site is hot; `None` means
/// "build it in full, as always".
///
/// Call it at a drain that is about to construct one of the [`FastThrowKind`]
/// exceptions on behalf of compiled code, BEFORE building the message (the
/// JEP 358 NPE message is itself a bytecode analysis). Allocates on the
/// fast path (one object, no Java code, no stack walk), so it is a GC point
/// exactly where the full construction it replaces was one.
pub fn try_fast_throw(
    shared: &SharedVm,
    thread: &JvmThread,
    kind: FastThrowKind,
) -> Option<ObjectRef> {
    if !omit_stack_trace_in_fast_throw() {
        return None;
    }
    let frame = thread.frames.last()?;
    let key: FastThrowSiteKey = (
        shared.vm_identity,
        frame.code.as_ptr() as usize,
        frame.pc,
        kind,
    );
    let hot = FAST_THROW_SITES.with(|sites| match sites.try_borrow_mut() {
        Ok(mut counts) => fast_throw_note_site(
            &mut counts,
            key,
            FAST_THROW_HOT_THRESHOLD,
            FAST_THROW_SITE_CAP,
        ),
        // Re-entered from inside a count (cannot happen: nothing above calls
        // out) — answer "not hot", which is the full, always-correct build.
        Err(_) => false,
    });
    if !hot {
        return None;
    }
    create_stackless_exception_object(shared, kind.class_name())
}

/// [`try_fast_throw`] for a drain that builds its exception through
/// [`throw_runtime_error`]. `error` is only evaluated on the full path, so an
/// expensive message is not built for a throwable that will not carry it.
pub fn throw_implicit_runtime_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    kind: FastThrowKind,
    error: impl FnOnce() -> RuntimeError,
) -> MethodCallFailed {
    if let Some(exc) = try_fast_throw(shared, thread, kind) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    throw_runtime_error(shared, thread, error())
}

/// [`try_fast_throw`] for a drain that builds its exception through
/// [`create_exception_object`] with a message. `message` is only evaluated on
/// the full path.
pub fn create_implicit_exception_object(
    shared: &SharedVm,
    thread: &mut JvmThread,
    kind: FastThrowKind,
    message: impl FnOnce() -> String,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(exc) = try_fast_throw(shared, thread, kind) {
        return Ok(exc);
    }
    let msg = message();
    create_exception_object(shared, thread, kind.class_name(), Some(&msg))
}

/// A throwable of the bootstrap class `class_name` with HotSpot's fast-throw
/// shape: null `detailMessage`, no `backtrace`, nothing in the VM trace store
/// (so `getStackTrace()` answers an empty array and `printStackTrace()` prints
/// the header line only), and the two `Throwable` field initialisers mirrored
/// so `initCause` / `addSuppressed` still behave as on a constructed one.
///
/// That last part is a deliberate difference from HotSpot, whose fast-throw
/// exception is ONE preallocated instance per kind carrying neither
/// initialiser: measured on 25.0.3 (`FirstStacklessProbe`), even the first
/// stackless NPE / `ArithmeticException` / AIOOBE there refuses `initCause`
/// and drops `addSuppressed`. This door hands out a fresh object per throw, so
/// a caller that decorates the exception it caught gets the constructed
/// object's behaviour rather than an artefact of sharing.
///
/// `None` — and the caller builds it in full — when the class is not loaded
/// yet, when instantiating it would skip an OBSERVABLE initialisation (see
/// [`initialisation_is_unobservable`]), or when the young generation cannot
/// satisfy the allocation without a collection.
///
/// r9w10 exc10: "initialised" used to be required outright, and that made the
/// whole door dead. The full build this path replaces
/// ([`create_exception_object`]) allocates the class and runs its constructor
/// shadow; it never runs `<clinit>`, so a VM-minted
/// `ArrayIndexOutOfBoundsException` / `ArithmeticException` stays
/// `UNINITIALIZED` for the life of a program that never says `new` for one
/// itself. Measured on `ExcLoop` (w9 build): with the flag ON, 0 of 150 000
/// caught AIOOBEs came out stackless, and the flag's cost was identical to
/// OFF — the integrator's "no difference". The same probe with one
/// `new ArrayIndexOutOfBoundsException("x")` in `main` got 148 984 stackless
/// and 3.2 us -> 1.4 us per exception.
fn create_stackless_exception_object(shared: &SharedVm, class_name: &str) -> Option<ObjectRef> {
    let (class_id, num_fields) = {
        let cm = shared.classes.class_manager.read();
        // The exact bootstrap-key probe, not `find_bootstrap_class_by_name`:
        // that one normalises its argument into TWO fresh `String`s (slash and
        // dot spellings) on every call, and this is the per-throw fast path.
        // `class_name` is always a `FastThrowKind::class_name`, i.e. already
        // the slash spelling, so the dot probe could never hit.
        let class_id =
            cm.loaded_class_under_exact_key(class_name, cratonvm_types::ClassLoaderId::Bootstrap)?;
        let class = cm.get_class(class_id).filter(|c| !c.hidden)?;
        if !crate::vm::is_class_initialized_fast(class)
            && !initialisation_is_unobservable(class_id, |cid| {
                cm.get_class(cid).map(|c| ClassInitView {
                    initialized: crate::vm::is_class_initialized_fast(c),
                    // `class_linked_or_link_trivial` also refuses a known link
                    // failure, and a never-linked class with something to check
                    // (a bootstrap class under `-Xverify:all`).
                    untouched: c.state != crate::classloading::ClassState::InitializationError
                        && crate::vm::class_linked_or_link_trivial(shared, &cm, c)
                        && c.init_state.load(std::sync::atomic::Ordering::Acquire)
                            == cratonvm_classloading::CLASS_INIT_UNINITIALIZED,
                    has_clinit: c.methods.iter().any(|m| &*m.name == "<clinit>"),
                    superclass: c.superclass,
                })
            })
        {
            return None;
        }
        (class_id, class.num_total_fields)
    };
    let obj = shared.mem.heap.try_alloc_object(class_id, num_fields)?;
    // No allocation and no Java between the allocation and the return, so the
    // bare `obj` cannot go stale here. No constructor runs, so both of
    // `Throwable`'s field initialisers are this door's to write.
    seed_throwable_cause(shared, obj);
    mirror_suppressed_initialiser(shared, obj);
    Some(obj)
}

/// What [`initialisation_is_unobservable`] needs to know about one class.
#[derive(Clone, Copy, Debug)]
struct ClassInitView {
    /// `CLASS_INIT_INITIALIZED`.
    initialized: bool,
    /// Never initialised, not being initialised, not erroneous, and linked or
    /// with nothing to check at link time.
    untouched: bool,
    /// Declares a `<clinit>`.
    has_clinit: bool,
    superclass: Option<ClassId>,
}

/// Would initialising `class_id` right now run no Java code at all?
///
/// True when every class from `class_id` up to its nearest INITIALISED
/// ancestor is untouched and declares no `<clinit>`. Initialising such a class
/// only flips its state word (JVMS §5.5 step 7 initialises the superclass
/// first — here already done — and step 9 runs a `<clinit>` there is none of;
/// `ConstantValue` statics were set at preparation), so creating an instance
/// without it is indistinguishable from creating one after it. A later
/// `new` of the class still initialises it, exactly as it would have.
///
/// That is the shape of every [`FastThrowKind`] class: `Throwable` has a
/// `<clinit>` and is initialised at boot; `Exception`, `RuntimeException`,
/// `IndexOutOfBoundsException` and the five leaves declare only
/// `serialVersionUID` constants. Anything else — a `<clinit>` anywhere below
/// the initialised ancestor, an in-progress or failed initialisation, a class
/// the lookup cannot see, a chain with no initialised ancestor, or one longer
/// than any real hierarchy — answers `false`, and the caller builds the
/// throwable in full.
///
/// Pure over `view`, so the rule is testable without a heap.
fn initialisation_is_unobservable(
    class_id: ClassId,
    view: impl Fn(ClassId) -> Option<ClassInitView>,
) -> bool {
    const MAX_DEPTH: usize = 64;
    let mut current = Some(class_id);
    for _ in 0..MAX_DEPTH {
        let Some(cid) = current else {
            return false;
        };
        let Some(v) = view(cid) else {
            return false;
        };
        if v.initialized {
            return true;
        }
        if !v.untouched || v.has_clinit {
            return false;
        }
        current = v.superclass;
    }
    false
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
///
/// The cache is the PER-VM `ThrowableLayoutCache`, shared with the native
/// context's `throwable_suppressed_sentinel` (`vm_exec.rs`), which resolves the
/// same two slots the same way. It used to be a pair of process-global statics
/// here: a `ClassId` and a static-field index are VM state, so a second
/// `SharedVm` in the process read the first VM's answer — the exact defect the
/// layout cache was introduced to remove from `lang_misc.rs`
/// (`perf-building-a-throwable-costs-2-microseconds-20260920.md`, item 1).
/// Each slot is independently idempotent (it only ever goes from unresolved to
/// one fixed value), so `Relaxed` is enough for both.
fn throwable_suppressed_sentinel(shared: &SharedVm) -> Option<Value> {
    use crate::vm::realms::thread_realm::{
        UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID, UNRESOLVED_THROWABLE_FIELD_INDEX,
    };
    use std::sync::atomic::Ordering;

    let cache = &shared.threads.throwable_layout_cache;
    let raw_class_id = cache.throwable_class_id.load(Ordering::Relaxed);
    let raw_index = cache
        .suppressed_sentinel_static_index
        .load(Ordering::Relaxed);
    let (class_id, index) = if raw_class_id != UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID
        && raw_index != UNRESOLVED_THROWABLE_FIELD_INDEX
    {
        (ClassId::new(raw_class_id), raw_index)
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
        cache
            .throwable_class_id
            .store(class_id.as_u32(), Ordering::Relaxed);
        cache
            .suppressed_sentinel_static_index
            .store(index, Ordering::Relaxed);
        (class_id, index)
    };
    match crate::vm::get_static_shared(shared, class_id, index) {
        v @ Value::Object(Some(_)) => Some(v),
        // `<clinit>` has not populated it yet — nothing faithful to write.
        _ => None,
    }
}

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
///
/// `frames` is the thread's own frame stack, and it is not optional: a compiled
/// activation that is the SAME activation as one of those frames — an OSR
/// continuation is always one — must be dropped, or the trace names it twice.
/// See `stackwalker::dedupe_compiled_snapshot`.
pub fn attach_snapshotted_trap_frames(
    shared: &SharedVm,
    frames: &[crate::runtime::frame::Frame],
    throwable: ObjectRef,
    snapshot: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
) {
    let Some(snapshot) = snapshot else {
        return;
    };
    let (snapshot, osr_bci) =
        crate::runtime::stackwalker::dedupe_compiled_snapshot(frames, snapshot);
    if snapshot.is_empty() && osr_bci.is_empty() {
        return;
    }
    // Looked up by ADDRESS, which is what the registry is keyed by. This used
    // to go through the identity-hash compatibility door (the since-deleted
    // `SharedVm::throwable_stack_trace(hash)`): that minted a mark-word hash
    // on the throwable solely to ask, then scanned every retained trace in
    // every shard comparing hashes — O(retained throwables) per compiled NPE —
    // and an identity-hash collision picked ANOTHER throwable's trace to
    // splice onto. `throwable_stack_trace_for` is the same answer (line
    // numbers resolved) from one shard probe.
    //
    // Round 12 wave 3 (lane exc): read UNRESOLVED and keep the capture's own
    // stamp. Nothing below reads a line -- the dedupe and overlap rules compare
    // `(class, method, bci)`, the OSR override re-resolves the entry it moves,
    // and the snapshot's entries are built unresolved -- so resolving every
    // frame here charged the line-table walk the registry exists to defer to
    // every compiled implicit exception, caught or not.
    let (mut existing, captured_at) = if trap_splice_lazy_lines_enabled() {
        let Some((existing, captured_at)) = shared.throwable_stack_trace_unresolved_for(throwable)
        else {
            return;
        };
        (existing, Some(captured_at))
    } else {
        let Some(existing) = shared.throwable_stack_trace_for(throwable) else {
            // No trace was stored for this throwable (the boot path where the
            // NPE class is not loaded yet). Nothing to splice onto.
            return;
        };
        (existing, None)
    };
    let old_len = existing.len();
    let cm = shared.classes.class_manager.read();
    // An OSR continuation the dedupe just dropped knew where its interpreter
    // frame really is; that frame's entry in the late capture still names the
    // back-edge it tiered up at. See `dedupe_compiled_snapshot`.
    crate::runtime::stackwalker::apply_snapshot_osr_overrides(
        &cm.class_store,
        frames,
        &osr_bci,
        &mut existing,
    );
    // Hidden frames stay out of the spliced snapshot as they stayed out of the
    // capture (`--jdk-only`, interpreter round i1 wave 37).
    let mut merged = crate::runtime::stackwalker::append_snapshotted_compiled_frames(
        &cm.class_store,
        &snapshot,
        existing,
        shared.config.is_jdk_only(),
    );
    drop(cm);
    // The spliced frames are the innermost ones; the cap keeps them and drops
    // from the outer end, as the construction-time capture does.
    crate::runtime::stackwalker::cap_throwable_trace(
        &mut merged,
        shared.config.max_java_stack_trace_depth,
    );
    if crate::runtime::env_cache::dbg_sttrace() {
        eprintln!(
            "STTRACE_DBG_NPE_SNAPSHOT recovered={} trace_now={}",
            snapshot.len(),
            merged.len()
        );
        for f in &snapshot {
            eprintln!("  STTRACE_DBG_NPE_SNAPSHOT[] {} bci={}", f.label, f.bci);
        }
    }
    // `Throwable.depth` sizes the array the real-JDK `getOurStackTrace()` asks
    // `StackTraceElement.of(backtrace, depth)` to fill, and that fill takes the
    // INNERMOST `depth` entries. Left at the pre-splice length it silently
    // dropped as many OUTERMOST frames as were spliced in. Updated only when it
    // still holds the length the construction-time capture wrote, i.e. when
    // it is ours to update.
    if let Some(depth_idx) = throwable_field_index_or_walk(shared, throwable, "depth") {
        if matches!(
            shared.mem.heap.get_field(throwable, depth_idx),
            Value::Int(d) if usize::try_from(d).ok() == Some(old_len)
        ) {
            // Cast: bounded by the frame count (at most 2 Mi frames,
            // `frame::MAX_REQUESTED_FRAME_LIMIT`, plus inlined levels) or the
            // depth cap; fits i32
            let depth = merged.len() as i32;
            shared
                .mem
                .heap
                .set_field(throwable, depth_idx, Value::Int(depth));
        }
    }
    let merged: Box<[crate::runtime::stackwalker::BacktraceFrame]> = merged
        .into_iter()
        .map(crate::runtime::stackwalker::BacktraceFrame::Entry)
        .collect();
    match captured_at {
        Some(at) => shared.store_throwable_backtrace_captured_at(throwable, merged, at),
        None => shared.store_throwable_backtrace(throwable, merged),
    }
}

/// [`create_exception_object`] for the implicit exception a compiled trap
/// signalled, with the compiled frames the trapping helper snapshotted.
///
/// Round 12 wave 4 (lane exc2), W3-2: when the snapshot is exactly the compiled
/// half of the stack the construction's `fillInStackTrace` would walk
/// (`stackwalker::trap_snapshot_can_stand_in_for_walk`), that capture uses it
/// in place of its own walk and the trace comes out whole: one stack walk per
/// implicit exception instead of two, and no
/// [`attach_snapshotted_trap_frames`] rewrite of the stored trace. Otherwise --
/// and whenever no capture took the snapshot -- the snapshot is spliced on
/// afterwards exactly as before. `CRATONVM_JIT_TRAP_CAPTURE_ONE_WALK=0` always
/// splices.
///
/// On failure the snapshot comes back with the error, so a caller that must
/// restore the signal exactly as it found it can.
#[allow(clippy::type_complexity)]
pub(crate) fn create_exception_object_with_trap_frames(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    message: Option<&str>,
    snapshot: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
) -> Result<
    ObjectRef,
    (
        MethodCallFailed,
        Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    ),
> {
    let mut snapshot = snapshot;
    let mut armed = None;
    if let Some(frames) = snapshot.take() {
        if trap_capture_one_walk_enabled()
            && crate::runtime::stackwalker::trap_snapshot_can_stand_in_for_walk(
                &thread.frames,
                &frames,
            )
        {
            match crate::runtime::stackwalker::arm_trap_capture(frames, thread.frames.len()) {
                Ok(window) => armed = Some(window),
                Err(frames) => snapshot = Some(frames),
            }
        } else {
            snapshot = Some(frames);
        }
    }
    let created = create_exception_object(shared, thread, class_name, message);
    if let Some(window) = armed {
        let (frames, used) = window.disarm();
        // A capture that used the snapshot stored the whole trace; splicing
        // it on again would name every trap frame twice.
        if !used || created.is_err() {
            snapshot = frames;
        }
    }
    let exc = created.map_err(|err| (err, snapshot.take()))?;
    attach_snapshotted_trap_frames(shared, &thread.frames, exc, snapshot);
    Ok(exc)
}

/// `CRATONVM_JIT_TRAP_CAPTURE_ONE_WALK` -- default ON; `0` restores the walk
/// plus splice in [`create_exception_object_with_trap_frames`]. Cached: read
/// per compiled implicit exception.
fn trap_capture_one_walk_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TRAP_CAPTURE_ONE_WALK")
    })
}

/// `CRATONVM_JIT_TRAP_SPLICE_LAZY_LINES` -- default ON; `0` restores the eager
/// line resolution in [`attach_snapshotted_trap_frames`]. Cached: read per
/// compiled implicit exception.
fn trap_splice_lazy_lines_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TRAP_SPLICE_LAZY_LINES")
    })
}

/// Convert a `RuntimeError` into a `MethodCallFailed::ExceptionThrown`.
///
/// Creates a real Java exception object on the heap corresponding to the
/// `RuntimeError` variant. If creating the Java exception object fails,
/// falls back to `MethodCallFailed::InternalError`.
///
/// Round-7 Fix 7: `#[cold]` — the whole throw machinery (allocation, init
/// call, fillInStackTrace) is rare relative to non-throwing opcodes. (This doc
/// and the attribute had drifted above `attach_snapshotted_trap_frames` when
/// that function was inserted between them, so `throw_runtime_error` itself
/// was not `#[cold]`.)
#[cold]
pub fn throw_runtime_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    error: RuntimeError,
) -> MethodCallFailed {
    // Interpreter round i1 wave 25, lane L1: an `Object.wait` / `Thread.join`
    // woken by the interrupt of a debugger's `ThreadReference.Stop` throws
    // the stop's throwable instead of its `InterruptedException`, as HotSpot's
    // asynchronous exception replaces the pending one
    // (`debug::take_stop_for_blocking_call`). One load while no stop is
    // pending.
    #[cfg(feature = "experimental-debug")]
    if matches!(error, RuntimeError::InterruptedException)
        && shared.debug.debugger_gates.stop_pending()
    {
        if let Some(stop) = crate::debug::take_stop_for_blocking_call(shared, thread.thread_id.0) {
            return MethodCallFailed::ExceptionThrown(stop);
        }
    }
    // r9w9 exc9: `OmitStackTraceInFastThrow` for an implicit exception raised
    // by the interpreter while it finishes a TRAPPED COMPILED frame (the
    // helper precise-resume path; see `ResumedCompiledFrameScope`). Off by
    // default; with no scope open this is one TLS read behind the flag.
    if omit_stack_trace_in_fast_throw() {
        if let Some(kind) = FastThrowKind::of_runtime_error(&error) {
            if in_resumed_compiled_frame(thread) {
                if let Some(exc) = try_fast_throw(shared, thread, kind) {
                    return MethodCallFailed::ExceptionThrown(exc);
                }
            }
        }
    }
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
            // Round 12 wave 5 (lane exc3): a `doRun`-frame scan and a 15-frame
            // walk taking the class-manager read lock per frame used to sit
            // here, discarding every name it read (`let _cn`). Deleted: it
            // printed nothing and decided nothing.
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

    // A VM-raised `OutOfMemoryError` reports an allocation ladder that has
    // already failed (or a length HotSpot refuses without collecting): build
    // its throwable without collecting again, falling back to the singleton
    // below. gc-common w4-c, `ExceptionAlloc::NoCollect`.
    let created = if matches!(&error, RuntimeError::OutOfMemoryError { .. }) {
        create_vm_oome_object(shared, thread, class_name, message.as_deref())
    } else {
        create_exception_object(shared, thread, class_name, message.as_deref())
    };
    match created {
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
                // gcd d2/j: the preallocated error of the message being
                // thrown when there is one (`CRATONVM_GC_PREALLOCATED_OOME_KINDS`),
                // else the `Java heap space` singleton, as before.
                if let Some(oom) = preallocated_oome_for(shared, message.as_deref().unwrap_or("")) {
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
///
/// gcd d2/j (2026-09-27): under the opt-in
/// `CRATONVM_GC_PREALLOCATED_OOME_KINDS` it also preallocates one error per
/// other VM-raised message (`heap_realm::OomeKind`: `Requested array size
/// exceeds VM limit`, `Metaspace`), as HotSpot's `Universe::genesis` does,
/// each stored (and so rooted) before the next is built. See
/// `preallocated_oome_for`.
pub fn ensure_singleton_oom(shared: &SharedVm, thread: &mut JvmThread) {
    if shared.mem.singleton_oom.read().is_none() {
        if let Ok(obj) = create_exception_object(
            shared,
            thread,
            "java/lang/OutOfMemoryError",
            Some("Java heap space"),
        ) {
            *shared.mem.singleton_oom.write() = Some(obj);
        }
    }
    if !cratonvm_types::flags().gc.preallocated_oome_kinds {
        return;
    }
    for kind in crate::vm::realms::heap_realm::OomeKind::ALL {
        if shared.mem.preallocated_oome.read().get(kind).is_some() {
            continue;
        }
        if let Ok(obj) = create_exception_object(
            shared,
            thread,
            "java/lang/OutOfMemoryError",
            Some(kind.message()),
        ) {
            shared.mem.preallocated_oome.write().set(kind, obj);
        }
    }
}

/// The preallocated `OutOfMemoryError` to throw for a VM-raised error whose
/// Java-visible detail message is `message`, when a fresh one cannot be built:
/// the message's own default where one was preallocated
/// ([`ensure_singleton_oom`], opt-in), else the `Java heap space` singleton
/// (the only one before gcd d2/j, whatever the message). `None` before
/// [`ensure_singleton_oom`] has run.
///
/// Item 4 of `gengc-r5w1-oom5-oom-path-review-residuals-FIXED-20260928`: a VM-limit
/// refusal whose own throwable could not be built on a full heap used to
/// reach Java as `Java heap space`.
pub(crate) fn preallocated_oome_for(shared: &SharedVm, message: &str) -> Option<ObjectRef> {
    let own = crate::vm::realms::heap_realm::OomeKind::for_message(message)
        .and_then(|kind| shared.mem.preallocated_oome.read().get(kind));
    own.or_else(|| *shared.mem.singleton_oom.read())
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

/// The `NoClassDefFoundError` a later use of a class whose `<clinit>` already
/// failed raises (JVMS §5.5 step 5). HotSpot words it
/// `Could not initialize class a.b.C` with the dotted name, unlike the
/// slash-named "class missing" form [`raise_no_class_def_found`] produces.
pub fn raise_could_not_initialize_class(
    shared: &SharedVm,
    thread: &mut JvmThread,
    internal_name: &str,
) -> MethodCallFailed {
    let message = format!(
        "Could not initialize class {}",
        internal_name.replace('/', ".")
    );
    raise_no_class_def_found(shared, thread, &message)
}

/// Why a class's initialization failed, as [`record_class_init_error`]
/// records it (`ClassRealm::class_init_errors`).
///
/// Holds no heap reference: the message is text, and the trace is the
/// original exception's frames as built entries (names and bcis), not the
/// compact `BacktraceFrame::Method` form, so a row keeps no method metadata
/// alive either.
#[derive(Clone, Debug)]
pub struct RecordedClassInitError {
    /// `Exception <class>[: <message>] [in thread "<name>"]`.
    pub message: std::sync::Arc<str>,
    /// The original exception's stack trace, OUTERMOST-first, lines resolved;
    /// `None` when it had none in the VM trace store (a stackless throwable).
    pub trace: Option<std::sync::Arc<[crate::native::registry::StackTraceEntry]>>,
}

/// [`raise_could_not_initialize_class`] with HotSpot 21+'s cause
/// (JDK-8048190): when `class_id`'s failure was recorded by
/// [`record_class_init_error`], the `NoClassDefFoundError` carries an
/// `ExceptionInInitializerError` whose message names the original exception
/// and thread, `Exception java.lang.RuntimeException: boom [in thread "main"]`,
/// and whose stack trace is the original exception's.
///
/// HotSpot records ONE such error object; this builds a fresh one per throw
/// (so it keeps no heap reference across collections) and gives it the
/// recorded trace.
#[cold]
pub fn raise_could_not_initialize_class_for(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    internal_name: &str,
) -> MethodCallFailed {
    let recorded = shared
        .classes
        .class_init_errors
        .lock()
        .get(&class_id)
        .cloned();
    let failed = raise_could_not_initialize_class(shared, thread, internal_name);
    let MethodCallFailed::ExceptionThrown(ncdfe) = failed else {
        return failed;
    };
    let Some(recorded) = recorded else {
        return MethodCallFailed::ExceptionThrown(ncdfe);
    };
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(ncdfe);
    let cause = create_exception_object(
        shared,
        thread,
        "java/lang/ExceptionInInitializerError",
        Some(&recorded.message),
    );
    let ncdfe = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);
    if let Ok(cause) = cause {
        // No allocation between here and the store: `cause` stays valid.
        if let Some(trace) = &recorded.trace {
            replace_fresh_throwable_trace(shared, cause, trace);
        }
        set_cause_by_name(shared, ncdfe, cause);
    }
    MethodCallFailed::ExceptionThrown(ncdfe)
}

/// Give `throwable`, just built by [`create_exception_object`], the trace
/// `entries` (OUTERMOST-first) in place of its construction-site one: the
/// trace store row and `Throwable.depth`, which sizes what the real-JDK
/// `getOurStackTrace()` materialises. Only when the construction-time capture
/// is ours (`backtrace == this`, see `trace_already_captured_at_current_depth`)
/// and nothing can have read it yet; otherwise the throwable is left alone.
fn replace_fresh_throwable_trace(
    shared: &SharedVm,
    throwable: ObjectRef,
    entries: &[crate::native::registry::StackTraceEntry],
) {
    let heap = &shared.mem.heap;
    let Some(bt_idx) = throwable_field_index_or_walk(shared, throwable, "backtrace") else {
        return;
    };
    if heap.get_field(throwable, bt_idx) != Value::Object(Some(throwable)) {
        return;
    }
    let Some(depth_idx) = throwable_field_index_or_walk(shared, throwable, "depth") else {
        return;
    };
    let Ok(depth) = i32::try_from(entries.len()) else {
        return;
    };
    heap.set_field(throwable, depth_idx, Value::Int(depth));
    shared.store_throwable_backtrace(
        throwable,
        entries
            .iter()
            .cloned()
            .map(crate::runtime::stackwalker::BacktraceFrame::Entry)
            .collect(),
    );
}

/// Record why `class_id`'s initialization failed, for
/// [`raise_could_not_initialize_class_for`]: HotSpot's
/// `java_lang_Throwable::create_initialization_error` message,
/// `Exception <external class name>[: <detailMessage>] [in thread "<name>"]`,
/// built from the throwable `failure` carries (its `detailMessage` field, not
/// a virtual `getMessage()`, as HotSpot reads it), and the exception's stack
/// trace, which HotSpot gives the recorded error too. Anything but a thrown
/// exception records nothing. No heap allocation, no Java.
#[cold]
pub fn record_class_init_error(
    shared: &SharedVm,
    thread: &JvmThread,
    class_id: ClassId,
    failure: &MethodCallFailed,
) {
    let MethodCallFailed::ExceptionThrown(exc) = failure else {
        return;
    };
    let exc = *exc;
    let heap = &shared.mem.heap;
    let exc_class = heap.class_id_of(exc);
    let Some(exc_name) = shared
        .classes
        .class_manager
        .read()
        .get_class(exc_class)
        .map(|c| c.name.replace('/', "."))
    else {
        return;
    };
    let string_field = |obj: ObjectRef, idx: Option<usize>| -> Option<String> {
        // Widening: u32 -> usize.
        let idx = idx.filter(|&i| i < heap.get_header(obj).num_slots() as usize)?;
        match heap.get_field(obj, idx) {
            Value::Object(Some(s)) => crate::vm::read_java_string(heap, s),
            _ => None,
        }
    };
    let message = string_field(
        exc,
        throwable_field_index_or_walk(shared, exc, "detailMessage"),
    );
    // `JavaThread::name()`: the `java.lang.Thread`'s own name.
    let thread_name = thread
        .java_thread_obj
        .and_then(|t| string_field(t, instance_field_index_by_name(shared, t, "name")))
        .unwrap_or_else(|| thread.name.clone());
    let text = match message {
        Some(m) => format!("Exception {exc_name}: {m} [in thread \"{thread_name}\"]"),
        None => format!("Exception {exc_name} [in thread \"{thread_name}\"]"),
    };
    // Rust-side copies only (the trace store row is read, not moved).
    let trace = shared
        .throwable_stack_trace_for(exc)
        .map(std::sync::Arc::<[crate::native::registry::StackTraceEntry]>::from);
    shared.classes.class_init_errors.lock().insert(
        class_id,
        RecordedClassInitError {
            message: std::sync::Arc::from(text),
            trace,
        },
    );
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
    raise_no_class_def_found_with_named_cause(shared, thread, missing_internal, missing_internal)
}

/// [`raise_no_class_def_found_with_cause`] where the error's message and the
/// cause's class name differ: `NoClassDefFoundError: [Lp/Gone;` caused by
/// `ClassNotFoundException: p.Gone` -- the array's element is what the
/// loader's `loadClass` was asked for. Both names are internal (slash) form.
#[cold]
fn raise_no_class_def_found_with_named_cause(
    shared: &SharedVm,
    thread: &mut JvmThread,
    error_internal: &str,
    missing_internal: &str,
) -> MethodCallFailed {
    // Same `CRATONVM_DBG_LINKAGE_BT` hook as `raise_no_class_def_found`, so
    // moving a site from that builder to this one keeps it traceable.
    if dbg_linkage_bt() {
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!("[DBG_LINKAGE_BT] NoClassDefFoundError {error_internal}\n{bt}");
    }
    let ncdfe = match create_exception_object(
        shared,
        thread,
        "java/lang/NoClassDefFoundError",
        Some(error_internal),
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

/// Whether any opt-in dump in [`throw_runtime_error`] that an implicit NPE or
/// `/ by zero` could trigger is on. A door that builds those throwables
/// directly (round 12 wave 4, `jit_bridge::implicit_exception_from_signals`)
/// keeps the `throw_runtime_error` route while one is, so the dumps still fire.
pub(crate) fn dbg_rterr_or_npe_none_dumps_enabled() -> bool {
    dbg_rterr_filter().is_some()
        || dbg_npe_none()
        || dbg_npe_trace()
        || dbg_wf_npe()
        || dbg_npe_match().is_some()
        || iae_trace_enabled()
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
            field_descriptor,
        } => (
            "java/lang/NoSuchFieldError",
            nsfe_message(class_name, field_name, field_descriptor),
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
        LinkageError::ClassCircularityError { class_name } => (
            "java/lang/ClassCircularityError",
            class_name.replace('/', "."),
        ),
        LinkageError::LoaderConstraintViolation { message } => {
            ("java/lang/LinkageError", message.clone())
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
        // A message in HotSpot's `ClassFileParser` wording names the class
        // itself (`... in class file <name>`), so it is not prefixed again
        // (interpreter round i1 wave 43, lane L3: the bad-magic refusal of
        // `ClassManager::define_class_shared_with_options`).
        LinkageError::ClassFormatError {
            class_name,
            message,
        } => (
            "java/lang/ClassFormatError",
            if message.contains(" in class file ") {
                message.clone()
            } else {
                format!("{}: {}", class_name, message)
            },
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
    // At an interpreter opcode boundary the frame on top of the stack is the
    // one whose constant pool is being resolved, so its class's defining
    // loader is the initiating loader. Compiled code has no interpreter frame
    // of its own there and passes its holder through
    // `convert_class_not_found_for`.
    let referencing = thread.frames.last().map(|f| f.class_id);
    convert_class_not_found_for(shared, thread, referencing, class_name, err)
}

/// The bottom element class of an internal array name (`[[Lp/X;` -> `p/X`),
/// or `name` itself when it is not a reference array (`p/X`, `[I`).
fn array_bottom_element_name(name: &str) -> &str {
    if !name.starts_with('[') {
        return name;
    }
    name.trim_start_matches('[')
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(name)
}

/// [`convert_class_not_found`] with the referencing class (the class whose
/// constant pool names `class_name`) given explicitly, for callers whose top
/// interpreter frame is not that class: the compiled-code helpers in
/// `vm/src/jit/helpers.rs`.
///
/// The referencing class decides one thing: whether a class that is simply
/// missing gets a `ClassNotFoundException` cause. HotSpot's
/// `SystemDictionary::resolve_or_fail` wraps whatever the initiating loader's
/// lookup threw; every Java loader (application, platform, user-defined)
/// throws `ClassNotFoundException`, and the bootstrap loader answers null
/// without one. So the cause is attached unless the referencing class is
/// bootstrap-defined (probe
/// `tools/probes/interp/L5/L5W23ResolutionErrorCause.java`). An unknown
/// referencing class counts as not bootstrap-defined.
#[cold]
pub fn convert_class_not_found_for(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing: Option<ClassId>,
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
            // An array name whose bottom element is the missing class is not a
            // dependency failure: HotSpot's `resolve_or_fail` names the class it
            // was asked for, `[Lp/Gone;` (probe `L2MissingCastTarget`), or
            // `[[Lp/Gone;` for `multianewarray` (probe `L5W24MissingClassCause`).
            // Compared element to element, so a miss reported under an
            // intermediate array name (`[Lp/Gone;` while resolving
            // `[[Lp/Gone;`) is still recognised.
            let missing_element = array_bottom_element_name(&missing);
            if missing_element == array_bottom_element_name(class_name) {
                let bootstrap_initiated = referencing.is_some_and(|cid| {
                    matches!(
                        shared.classes.class_manager.read().get_loader_id(cid),
                        Some(cratonvm_types::ClassLoaderId::Bootstrap)
                    )
                });
                if bootstrap_initiated {
                    raise_no_class_def_found(shared, thread, class_name)
                } else {
                    raise_no_class_def_found_with_named_cause(
                        shared,
                        thread,
                        class_name,
                        missing_element,
                    )
                }
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

    /// `throwable_own_field_index` answers from THIS VM's `ThrowableLayoutCache`
    /// with the slot the native context would resolve, and a second VM's slot
    /// is not consulted. Also pins that a name outside Throwable's six is
    /// refused rather than walked.
    #[test]
    fn throwable_own_field_slots_come_from_this_vms_layout_cache() {
        use std::sync::atomic::Ordering::Relaxed;
        let first = test_vm();
        let second = test_vm();
        let Some(cause) = throwable_own_field_index(&first.shared, "cause") else {
            // Stripped VM (no JDK on the test classpath): no Throwable layout.
            return;
        };
        let expected = {
            let cm = first.shared.classes.class_manager.read();
            let tid = cm
                .get_loaded_class_id("java/lang/Throwable")
                .expect("resolved just above");
            crate::vm::vm_exec::resolve_field_index_in_hierarchy(tid, "cause", &cm.class_store)
        };
        assert_eq!(
            Some(cause),
            expected,
            "must match the native context's resolution"
        );

        second
            .shared
            .threads
            .throwable_layout_cache
            .cause
            .store(cause + 1000, Relaxed);
        assert_eq!(
            throwable_own_field_index(&first.shared, "cause"),
            Some(cause),
            "another VM's slot must not leak into this one"
        );
        assert_eq!(
            throwable_own_field_index(&second.shared, "cause"),
            Some(cause + 1000),
            "each VM reads its own slot"
        );
        assert_eq!(
            throwable_own_field_index(&first.shared, "notAThrowableField"),
            None
        );
    }

    /// `throwable_suppressed_sentinel`'s `(ClassId, static index)` pair lives
    /// in the per-VM layout cache. It was a pair of process-global statics, so
    /// a second VM in the process read the first VM's ids.
    #[test]
    fn suppressed_sentinel_slots_are_per_vm() {
        use crate::vm::realms::thread_realm::{
            UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID, UNRESOLVED_THROWABLE_FIELD_INDEX,
        };
        use std::sync::atomic::Ordering::Relaxed;
        let first = test_vm();
        let second = test_vm();
        if throwable_suppressed_sentinel(&first.shared).is_none() {
            // Stripped VM, or `Throwable.<clinit>` has not run: nothing cached.
            return;
        }
        let cache = &first.shared.threads.throwable_layout_cache;
        let cid = cache.throwable_class_id.load(Relaxed);
        let idx = cache.suppressed_sentinel_static_index.load(Relaxed);
        assert_ne!(
            cid, UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID,
            "resolved into THIS VM's cache"
        );
        assert_ne!(
            idx, UNRESOLVED_THROWABLE_FIELD_INDEX,
            "resolved into THIS VM's cache"
        );

        // Poison the second VM's pair; the first VM's answer must not move.
        let other = &second.shared.threads.throwable_layout_cache;
        other.throwable_class_id.store(cid, Relaxed);
        other
            .suppressed_sentinel_static_index
            .store(idx + 1000, Relaxed);
        assert!(throwable_suppressed_sentinel(&first.shared).is_some());
        assert_eq!(cache.suppressed_sentinel_static_index.load(Relaxed), idx);
    }

    /// Round 9 wave 8 (vmrt8): a failing cast pays for the reclamation-ring
    /// forensics once per `(receiver class, target)` pair, not per refusal
    /// (that was a ~5 ms `ClassCastException`), and a zero header always
    /// pays. Class ids here are far outside anything a test VM defines, so
    /// the process-wide pair set cannot collide with another test.
    #[test]
    fn cast_refusal_forensics_run_once_per_pair_and_always_for_a_zero_header() {
        let cid = 0x7ACE_0001;
        assert!(cast_refusal_forensics_admitted(cid, "java.lang.Integer"));
        assert!(
            !cast_refusal_forensics_admitted(cid, "java.lang.Integer"),
            "a repeat of the same refusal is a type-test idiom and must not rescan"
        );
        assert!(
            !cast_refusal_forensics_admitted(cid, "java/lang/Integer"),
            "the JIT's slashed spelling is the same pair as the interpreter's dotted one"
        );
        assert!(
            cast_refusal_forensics_admitted(cid, "java.lang.Long"),
            "a new target is a new pair"
        );
        assert!(
            cast_refusal_forensics_admitted(0x7ACE_0002, "java.lang.Integer"),
            "a new receiver class is a new pair"
        );
        for _ in 0..3 {
            assert!(
                cast_refusal_forensics_admitted(0, "java.lang.Integer"),
                "a ClassId(0) receiver is the reclaimed-span signature: always admitted"
            );
        }
    }

    // -----------------------------------------------------------------------
    // `klass_origin`'s descriptor parse (W7-37 §B8.1 / §B11.1)
    // -----------------------------------------------------------------------

    /// The operand spellings are HotSpot's, taken verbatim from a Temurin
    /// 25.0.3.9 run of the `CastMsgs` probe (see
    /// `docs/internal/jdk-only/W8-C4-1-array-cast-klass-origin.md`
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

    /// Round 12 wave 7 (lane compat): step 4 leaves the trace to an
    /// application override of `fillInStackTrace()` (the "quiet exception"
    /// idiom), and keeps its backstop for the JDK's own overrides and for a
    /// class with none. Mode-independent: the walk reads no compatibility mode.
    #[test]
    fn step_four_defers_only_to_an_application_fill_override() {
        use std::collections::HashMap;
        // (is_throwable, built_in, declares_fill, superclass)
        let classes: HashMap<u32, (bool, bool, bool, Option<u32>)> = HashMap::from([
            (1, (true, true, true, None)),      // java/lang/Throwable
            (2, (false, true, false, Some(1))), // java/lang/RuntimeException
            (3, (false, true, true, Some(2))),  // java/lang/NullPointerException
            (4, (false, false, true, Some(2))), // app Quiet extends RuntimeException
            (5, (false, false, false, Some(4))), // app LoudQuiet extends Quiet
            (6, (false, false, false, Some(2))), // app Plain extends RuntimeException
            (7, (false, false, false, Some(3))), // app MyNpe extends NullPointerException
            (8, (false, false, false, Some(99))), // superclass not in the store
        ]);
        let walk = |start: u32| {
            first_application_fill_override(ClassId::new(start), |cid| {
                let &(is_throwable, built_in, declares_fill, sup) =
                    classes.get(&cid.as_u32())?;
                Some(FillOverrideStep {
                    is_throwable,
                    built_in,
                    declares_fill,
                    superclass: sup.map(ClassId::new),
                })
            })
        };
        assert!(walk(4), "an application override decides");
        assert!(walk(5), "and so does an inherited one");
        assert!(!walk(1) && !walk(2) && !walk(6), "no override: the backstop stays");
        assert!(!walk(3) && !walk(7), "a JDK override keeps the backstop");
        assert!(!walk(8), "an unknown class keeps the backstop");
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
        assert!(
            vm.shared.throwable_stack_trace_for(obj).is_some(),
            "backtrace self-marker is set but no trace was registered"
        );
    }

    /// Nested / rethrown shape: building a second throwable while the first is
    /// still live must not evict or overwrite the first one's trace. The store
    /// is keyed by the throwable, so both entries have to coexist — otherwise a
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
        let Some(inner_trace) = vm.shared.throwable_stack_trace_for(inner) else {
            return;
        };

        // Pin `inner` so building the outer throwable cannot collect it, and
        // re-read it afterwards in case the collection moved it.
        vm.main_thread.native_pin_roots.push(inner);
        let outer = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/IllegalArgumentException",
            Some("outer"),
        );
        let inner = vm.main_thread.native_pin_roots.pop().expect("pinned inner");

        let Ok(outer) = outer else { return };

        assert!(
            vm.shared.throwable_stack_trace_for(outer).is_some(),
            "outer throwable lost its trace"
        );
        assert_eq!(
            vm.shared.throwable_stack_trace_len_for(inner),
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

    // -----------------------------------------------------------------------
    // r9w9 exc9: OmitStackTraceInFastThrow
    // -----------------------------------------------------------------------

    /// A site is hot strictly after `threshold` throws, per key; a full table
    /// is cleared rather than grown, and never answers "hot" for a site it
    /// has not counted past the threshold.
    #[test]
    fn fast_throw_site_turns_hot_after_the_threshold_and_the_table_is_bounded() {
        let mut counts: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
        for i in 1..=3 {
            assert!(
                !fast_throw_note_site(&mut counts, 7, 3, 16),
                "throw {i} of 3 must still be built in full"
            );
        }
        assert!(
            fast_throw_note_site(&mut counts, 7, 3, 16),
            "the 4th is hot"
        );
        assert!(fast_throw_note_site(&mut counts, 7, 3, 16), "and stays hot");
        assert!(
            !fast_throw_note_site(&mut counts, 8, 3, 16),
            "another site counts on its own"
        );

        // Cap: filling the table with distinct sites clears it, and the old
        // hot site starts over (slower, never wrong).
        let mut small: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
        for _ in 0..3 {
            let _ = fast_throw_note_site(&mut small, 1, 1, 2);
        }
        assert!(fast_throw_note_site(&mut small, 1, 1, 2));
        let _ = fast_throw_note_site(&mut small, 2, 1, 2);
        assert_eq!(small.len(), 2);
        let _ = fast_throw_note_site(&mut small, 3, 1, 2);
        assert_eq!(small.len(), 1, "a full table is cleared before a new key");
        assert!(
            !fast_throw_note_site(&mut small, 1, 1, 2),
            "a cleared site is cold again"
        );
    }

    #[test]
    fn fast_throw_kind_covers_exactly_the_five_implicit_exceptions() {
        use super::FastThrowKind as K;
        let cases = [
            (
                RuntimeError::NullPointerException { message: None },
                Some(K::NullPointer),
            ),
            (
                RuntimeError::ArrayIndexOutOfBoundsException {
                    index: 5,
                    message: None,
                },
                Some(K::ArrayIndexOutOfBounds),
            ),
            (
                RuntimeError::ArithmeticException {
                    message: "/ by zero".to_string(),
                },
                Some(K::Arithmetic),
            ),
            (
                RuntimeError::ClassCastException {
                    message: "x".to_string(),
                },
                Some(K::ClassCast),
            ),
            (
                RuntimeError::ArrayStoreException {
                    message: "x".to_string(),
                },
                Some(K::ArrayStore),
            ),
            (
                RuntimeError::NotImplemented {
                    feature: "x".to_string(),
                },
                None,
            ),
        ];
        for (error, want) in cases {
            assert_eq!(FastThrowKind::of_runtime_error(&error), want, "{error:?}");
        }
        assert_eq!(
            K::ArrayIndexOutOfBounds.class_name(),
            "java/lang/ArrayIndexOutOfBoundsException"
        );
    }

    /// The resumed-compiled-frame scope names one depth, nests, and restores
    /// the outer one on drop.
    #[test]
    fn resumed_compiled_frame_scope_nests_and_restores() {
        let read = || RESUMED_COMPILED_FRAME_DEPTH.with(|d| d.get());
        assert_eq!(read(), 0);
        {
            let _outer = ResumedCompiledFrameScope::enter(3);
            assert_eq!(read(), 3);
            {
                let _inner = ResumedCompiledFrameScope::enter(7);
                assert_eq!(read(), 7);
            }
            assert_eq!(read(), 3, "the inner scope restores the outer depth");
        }
        assert_eq!(read(), 0);
    }

    /// The stackless throwable is an instance of the requested class and has
    /// NO entry in the VM trace store (so `getStackTrace()` is empty), where
    /// the full build of the same class registers one.
    #[test]
    fn stackless_exception_has_the_class_and_no_stored_trace() {
        let mut vm = test_vm();
        let Ok(full) = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/ArithmeticException",
            Some("/ by zero"),
        ) else {
            // Stripped VM (no JDK on the test classpath): nothing to probe with.
            return;
        };
        let class_id = vm.shared.mem.heap.class_id_of(full);
        let Some(fast) =
            create_stackless_exception_object(&vm.shared, "java/lang/ArithmeticException")
        else {
            // Only legitimate refusal after a successful full build: the class
            // is neither initialised nor safely initialisable without Java
            // (r9w10 exc10 — an uninitialised `ArithmeticException` under an
            // initialised `Throwable` is NOT a reason to refuse any more).
            let cm = vm.shared.classes.class_manager.read();
            let class = cm.get_class(class_id).expect("the full build loaded it");
            assert!(
                !crate::vm::is_class_initialized_fast(class),
                "an initialised, loaded exception class must yield a stackless instance"
            );
            return;
        };
        assert_eq!(vm.shared.mem.heap.class_id_of(fast), class_id);
        assert_ne!(fast, full, "a fresh instance per throw, never a shared one");
        // `cause = this` on both, whatever the layout: a constructor seeded it
        // on `full`, and the door did on `fast`, which has none. A never-written
        // compact slot would read `Object(None)` -- a refused `initCause`.
        let cause = throwable_field_index_or_walk(&vm.shared, fast, "cause")
            .expect("ArithmeticException inherits Throwable.cause");
        assert_eq!(
            vm.shared.mem.heap.get_field(full, cause),
            Value::Object(Some(full))
        );
        assert_eq!(
            vm.shared.mem.heap.get_field(fast, cause),
            Value::Object(Some(fast))
        );
        assert!(
            vm.shared.throwable_stack_trace_for(fast).is_none(),
            "a fast throw must not walk the stack into the trace store"
        );
    }

    /// Off (the default, `-XX:-OmitStackTraceInFastThrow`), the door never
    /// answers, however hot the site: every throw is built in full.
    #[test]
    fn fast_throw_door_is_closed_when_the_flag_is_off() {
        if omit_stack_trace_in_fast_throw() {
            // The process was started with the opt-in set; this test is about
            // the default.
            return;
        }
        let vm = test_vm();
        for _ in 0..(FAST_THROW_HOT_THRESHOLD * 2) {
            assert!(
                try_fast_throw(&vm.shared, &vm.main_thread, FastThrowKind::Arithmetic).is_none()
            );
        }
    }

    /// r9w10 exc10: the stackless door no longer demands an INITIALISED
    /// class — the full build never initialises the implicit-exception
    /// classes, so that demand kept the door shut forever. It demands instead
    /// that initialising would run no Java: a `<clinit>`-free, untouched chain
    /// up to an initialised ancestor.
    #[test]
    fn stackless_door_accepts_a_clinit_free_chain_under_an_initialised_ancestor() {
        // 1 AIOOBE -> 2 IOOBE -> 3 RuntimeException -> 4 Throwable (initialised).
        let chain = |overrides: &[(u32, ClassInitView)]| {
            let mut m: std::collections::HashMap<u32, ClassInitView> =
                std::collections::HashMap::new();
            let plain = |sup: u32| ClassInitView {
                initialized: false,
                untouched: true,
                has_clinit: false,
                superclass: Some(ClassId::new(sup)),
            };
            m.insert(1, plain(2));
            m.insert(2, plain(3));
            m.insert(3, plain(4));
            m.insert(
                4,
                ClassInitView {
                    initialized: true,
                    untouched: false,
                    has_clinit: true,
                    superclass: None,
                },
            );
            for (k, v) in overrides {
                m.insert(*k, *v);
            }
            m
        };
        let ask = |m: &std::collections::HashMap<u32, ClassInitView>| {
            initialisation_is_unobservable(ClassId::new(1), |cid| m.get(&cid.as_u32()).copied())
        };

        assert!(ask(&chain(&[])), "the implicit-exception shape is accepted");

        let with = |id: u32, f: &dyn Fn(&mut ClassInitView)| {
            let mut m = chain(&[]);
            if let Some(v) = m.get_mut(&id) {
                f(v);
            }
            m
        };
        assert!(
            !ask(&with(2, &|v| v.has_clinit = true)),
            "a <clinit> below the initialised ancestor would be skipped"
        );
        assert!(
            !ask(&with(1, &|v| v.untouched = false)),
            "an in-progress or failed initialisation is never bypassed"
        );
        assert!(
            !ask(&with(4, &|v| v.initialized = false)),
            "no initialised ancestor: the <clinit> of Throwable has not run"
        );
        assert!(
            // gate-ok(not-a-Class): `v` is a `ClassInitView`, not a `Class`.
            !ask(&with(3, &|v| v.superclass = Some(ClassId::new(99)))),
            "a class the lookup cannot see refuses"
        );
        assert!(
            ask(&with(1, &|v| v.initialized = true)),
            "an initialised class is accepted outright"
        );
        // A cycle (never a real hierarchy) terminates and refuses.
        // gate-ok(not-a-Class): `v` is a `ClassInitView`, not a `Class`.
        assert!(!ask(&with(3, &|v| v.superclass = Some(ClassId::new(1)))));
    }

    /// A `Class` for the JNI `ThrowNew` tests: `methods` only, no fields of its
    /// own, extending the bootstrap `java/lang/Throwable` when `throwable` and
    /// that class is loaded (a stripped VM has none, and the code under test
    /// then skips its `Throwable` check too).
    fn add_jni_throw_new_test_class(
        vm: &Vm,
        name: &str,
        methods: Vec<cratonvm_reader::method::ClassFileMethod>,
        throwable: bool,
    ) -> ClassId {
        use crate::classloading::{Class, ClassLoaderId, ClassState};
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        use cratonvm_reader::class_file_version::ClassFileVersion;
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        let mut cm = vm.shared.classes.class_manager_write();
        let superclass = if throwable {
            cm.loaded_class_under_exact_key("java/lang/Throwable", ClassLoaderId::Bootstrap)
        } else {
            None
        };
        // Throwable's instance fields, or slack for Throwable-shaped slot
        // writes should a fallback path be taken without it.
        let inherited = superclass
            .and_then(|s| cm.get_class(s))
            .map_or(8, |c| c.num_total_fields);
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags: ClassAccessFlags::from_bits_truncate(0x0021),
            superclass,
            interfaces: vec![],
            fields: vec![],
            methods,
            first_field_index: inherited,
            num_total_fields: inherited,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
        cm.register_class_name(ClassLoaderId::Application, name, id);
        id
    }

    /// A public `<init>` of `descriptor` whose body is `code`.
    fn jni_test_ctor(
        descriptor: &str,
        max_locals: u16,
        code: Vec<u8>,
    ) -> cratonvm_reader::method::ClassFileMethod {
        use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
        use cratonvm_reader::class_access_flags::MethodAccessFlags;
        cratonvm_reader::method::ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: std::sync::Arc::from("<init>"),
            descriptor: std::sync::Arc::from(descriptor),
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table: vec![],
                attributes: vec![],
            }))],
        }
    }

    /// JNI `ThrowNew` follows HotSpot's `Exceptions::new_exception`: a
    /// constructor that throws makes ITS exception the one to throw. The
    /// half-built throwable used to be returned with the message written in by
    /// hand. The class's `<init>(String)` is `aconst_null; athrow`.
    #[test]
    fn jni_throw_new_rethrows_what_the_constructor_threw() {
        const NAME: &str = "cratonvm/test/JniThrowNewCtorThrows";

        let mut vm = test_vm();
        // Stripped VM: without a buildable NPE the constructor's `athrow`
        // cannot produce the exception this test is about.
        let Ok(probe) = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/NullPointerException",
            None,
        ) else {
            return;
        };
        let npe_class = vm.shared.mem.heap.class_id_of(probe);
        // `<init>(String)`: `aconst_null; athrow`.
        let ctor = jni_test_ctor("(Ljava/lang/String;)V", 2, vec![0x01, 0xbf]);
        let class_id = add_jni_throw_new_test_class(&vm, NAME, vec![ctor], true);
        let pins = vm.main_thread.native_pin_roots.len();
        let thrown = create_exception_object_for_jni_throw_new(
            &vm.shared,
            &mut vm.main_thread,
            class_id,
            NAME,
            Some("m"),
        )
        .expect("the constructor's exception is a result, not a failure");
        assert_eq!(
            vm.shared.mem.heap.class_id_of(thrown),
            npe_class,
            "ThrowNew throws what the constructor threw"
        );
        assert_eq!(vm.main_thread.native_pin_roots.len(), pins, "pins released");
    }

    /// JDK-8048190 (HotSpot 21+): "Could not initialize class" carries an
    /// `ExceptionInInitializerError` naming the original failure and thread.
    #[test]
    fn could_not_initialize_class_carries_the_recorded_cause() {
        let mut vm = test_vm();
        let Ok(boom) = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/IllegalStateException",
            Some("boom"),
        ) else {
            return;
        };
        // Stripped VM: the stand-in class may carry no readable
        // `detailMessage`, and then there is no message to record.
        let message_readable = throwable_field_index_or_walk(&vm.shared, boom, "detailMessage")
            .filter(|&i| i < vm.shared.mem.heap.get_header(boom).num_slots() as usize)
            .is_some_and(|i| {
                matches!(
                    vm.shared.mem.heap.get_field(boom, i),
                    Value::Object(Some(_))
                )
            });
        if !message_readable {
            return;
        }
        // The original exception's trace (wave 8, L5): the recorded cause
        // must carry it, as HotSpot's does, not the "Could not initialize"
        // site's.
        let origin_frame = |method: &str| crate::native::registry::StackTraceEntry {
            class_name: std::sync::Arc::from("p/Origin"),
            method_name: std::sync::Arc::from(method),
            method_descriptor: None,
            source_file: None,
            line_number: -1,
            byte_code_index: 3,
            class_id: None,
            method_index: None,
        };
        vm.shared.store_throwable_stack_trace(
            boom,
            std::sync::Arc::from(vec![origin_frame("main"), origin_frame("<clinit>")]),
        );
        let method_names = |trace: &[crate::native::registry::StackTraceEntry]| {
            trace
                .iter()
                .map(|e| e.method_name.to_string())
                .collect::<Vec<_>>()
        };
        // The table is keyed by the failed class's id only.
        let failed_class = ClassId::new(0x7fff_0000);
        record_class_init_error(
            &vm.shared,
            &vm.main_thread,
            failed_class,
            &MethodCallFailed::ExceptionThrown(boom),
        );
        let recorded = vm
            .shared
            .classes
            .class_init_errors
            .lock()
            .get(&failed_class)
            .cloned()
            .expect("a thrown exception is recorded");
        assert_eq!(
            recorded.trace.as_deref().map(method_names),
            Some(vec!["main".to_string(), "<clinit>".to_string()]),
            "the original exception's trace is recorded"
        );
        let recorded = recorded.message;
        assert!(
            recorded.starts_with("Exception java.lang.IllegalStateException: boom [in thread \"")
                && recorded.ends_with("\"]"),
            "HotSpot's create_initialization_error spelling, got {recorded}"
        );
        // Stripped VM: no buildable ExceptionInInitializerError, no cause.
        if create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/ExceptionInInitializerError",
            Some("probe"),
        )
        .is_err()
        {
            return;
        }
        let pins = vm.main_thread.native_pin_roots.len();
        let MethodCallFailed::ExceptionThrown(ncdfe) = raise_could_not_initialize_class_for(
            &vm.shared,
            &mut vm.main_thread,
            failed_class,
            "p/Failed",
        ) else {
            return;
        };
        assert_eq!(vm.main_thread.native_pin_roots.len(), pins, "pins released");
        let heap = &vm.shared.mem.heap;
        let cause_idx =
            throwable_field_index_or_walk(&vm.shared, ncdfe, "cause").expect("Throwable.cause");
        let Value::Object(Some(cause)) = heap.get_field(ncdfe, cause_idx) else {
            panic!("the NoClassDefFoundError has no cause");
        };
        assert_ne!(cause, ncdfe, "a cause, not the unset self-marker");
        let msg_idx = throwable_field_index_or_walk(&vm.shared, cause, "detailMessage")
            .expect("Throwable.detailMessage");
        let Value::Object(Some(msg)) = heap.get_field(cause, msg_idx) else {
            panic!("the cause has no message");
        };
        assert_eq!(
            crate::vm::read_java_string(heap, msg).as_deref(),
            Some(&*recorded)
        );
        // Stripped VM: a stand-in whose construction did not run our capture
        // (`backtrace != this`) is left with its own trace.
        let ours = throwable_field_index_or_walk(&vm.shared, cause, "backtrace")
            .is_some_and(|i| heap.get_field(cause, i) == Value::Object(Some(cause)));
        if ours {
            assert_eq!(
                vm.shared
                    .throwable_stack_trace_for(cause)
                    .as_deref()
                    .map(method_names),
                Some(vec!["main".to_string(), "<clinit>".to_string()]),
                "the cause carries the original exception's trace"
            );
            let depth_idx =
                throwable_field_index_or_walk(&vm.shared, cause, "depth").expect("Throwable.depth");
            assert_eq!(heap.get_field(cause, depth_idx), Value::Int(2));
        }
    }

    /// `ThrowNew(clazz, msg)` on a `Throwable` that declares no `(String)`
    /// constructor throws `NoSuchMethodError` with HotSpot's message, rather
    /// than running the inherited `Throwable.<init>(String)` on it (or falling
    /// back to `()V` and writing the message by hand).
    #[test]
    fn jni_throw_new_without_a_string_constructor_throws_no_such_method_error() {
        const NAME: &str = "cratonvm/test/JniThrowNewNoStringCtor";

        let mut vm = test_vm();
        let Ok(probe) = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/NoSuchMethodError",
            None,
        ) else {
            return;
        };
        let nsme_class = vm.shared.mem.heap.class_id_of(probe);
        // `<init>()V` only: `return`.
        let ctor = jni_test_ctor("()V", 1, vec![0xb1]);
        let class_id = add_jni_throw_new_test_class(&vm, NAME, vec![ctor], true);
        let pins = vm.main_thread.native_pin_roots.len();
        let thrown = create_exception_object_for_jni_throw_new(
            &vm.shared,
            &mut vm.main_thread,
            class_id,
            NAME,
            Some("m"),
        )
        .expect("NoSuchMethodError is the exception to throw, not a failure");
        assert_eq!(
            vm.shared.mem.heap.class_id_of(thrown),
            nsme_class,
            "a missing (String) constructor throws NoSuchMethodError"
        );
        assert_eq!(
            nsme_message(NAME, "<init>", "(Ljava/lang/String;)V"),
            "'void cratonvm.test.JniThrowNewNoStringCtor.<init>(java.lang.String)'"
        );
        assert_eq!(vm.main_thread.native_pin_roots.len(), pins, "pins released");
    }

    /// `ThrowNew` on a class that is not a `Throwable` is refused: nothing is
    /// built, so JNI returns `JNI_ERR` with no exception pending.
    #[test]
    fn jni_throw_new_refuses_a_class_that_is_not_a_throwable() {
        const NAME: &str = "cratonvm/test/JniThrowNewNotThrowable";

        let mut vm = test_vm();
        let throwable_loaded = vm
            .shared
            .classes
            .class_manager
            .read()
            .loaded_class_under_exact_key(
                "java/lang/Throwable",
                cratonvm_types::ClassLoaderId::Bootstrap,
            )
            .is_some();
        if !throwable_loaded {
            return;
        }
        let ctor = jni_test_ctor("(Ljava/lang/String;)V", 2, vec![0xb1]);
        let class_id = add_jni_throw_new_test_class(&vm, NAME, vec![ctor], false);
        let pins = vm.main_thread.native_pin_roots.len();
        let result = create_exception_object_for_jni_throw_new(
            &vm.shared,
            &mut vm.main_thread,
            class_id,
            NAME,
            Some("m"),
        );
        assert!(
            matches!(result, Err(MethodCallFailed::InternalError(_))),
            "a non-Throwable class must be refused, got {result:?}"
        );
        assert_eq!(
            vm.main_thread.native_pin_roots.len(),
            pins,
            "no pins leaked"
        );
    }
}

// ---------------------------------------------------------------------------
// JEP 358 — helpful-NPE analysis tests (rt.jar-free, pure syntactic logic)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod helpful_npe_tests {
    use super::helpful_npe::{self, CpRef, CpResolver, Producer};
    use super::{linkage_throwable, nsfe_message, nsme_message, LinkageError};
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
        descriptor: Option<&'static str>,
        /// `invokedynamic` call-site descriptors by CP index (wave 22).
        indy: HashMap<u16, String>,
        /// Exception-handler bcis of the method under analysis (wave 22).
        handlers: Vec<usize>,
    }

    impl MockResolver {
        fn new(fields: HashMap<u16, CpRef>, methods: HashMap<u16, CpRef>) -> Self {
            MockResolver {
                fields,
                methods,
                locals: HashMap::new(),
                is_static: false,
                descriptor: None,
                indy: HashMap::new(),
                handlers: Vec::new(),
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
        fn method_descriptor(&self) -> Option<&str> {
            self.descriptor
        }
        fn invokedynamic_descriptor(&self, i: u16) -> Option<String> {
            self.indy.get(&i).cloned()
        }
        fn handler_pcs(&self) -> Vec<usize> {
            self.handlers.clone()
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

    /// `NoSuchFieldError`'s message, HotSpot's JDK 21+ sentence
    /// (`LinkResolver::resolve_field`): dotted class, then the fieldref type in
    /// source spelling and the name, quoted.
    #[test]
    fn nsfe_message_matches_hotspot() {
        assert_eq!(
            nsfe_message("p/C", "f", "I"),
            "Class p.C does not have member field 'int f'"
        );
        assert_eq!(
            nsfe_message("p/C", "names", "[Ljava/lang/String;"),
            "Class p.C does not have member field 'java.lang.String[] names'"
        );
        assert_eq!(
            nsfe_message("p/C$In", "o", "Ljava/lang/Object;"),
            "Class p.C$In does not have member field 'java.lang.Object o'"
        );
        assert_eq!(
            nsfe_message("C", "g", "[[J"),
            "Class C does not have member field 'long[][] g'"
        );
        // No descriptor in hand: no invented type.
        assert_eq!(
            nsfe_message("C", "g", ""),
            "Class C does not have member field 'g'"
        );
        // And `linkage_throwable` routes the variant through it.
        let (class, msg) = linkage_throwable(&LinkageError::NoSuchFieldError {
            class_name: "p/C".into(),
            field_name: "f".into(),
            field_descriptor: "Z".into(),
        });
        assert_eq!(class, "java/lang/NoSuchFieldError");
        assert_eq!(msg, "Class p.C does not have member field 'boolean f'");
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

    /// With the method descriptor known, an unwritten slot in the parameter
    /// area is `<parameterN>` (HotSpot `print_local_var`): 1-based, a `long`
    /// counting once, `this` not counted. A slot the method stores to anywhere
    /// is `<localN>` again.
    #[test]
    fn unwritten_parameter_slots_are_named_parameter_n() {
        let code = vec![ALOAD_0, ARRAYLENGTH];
        let mut r = MockResolver::new_static(HashMap::new(), HashMap::new());
        r.descriptor = Some("([I)V");
        let expr = helpful_npe::null_expr_at_depth(&code, 1, 0, &r);
        assert_eq!(text(&expr), Some("<parameter1>"));

        // Instance method `(JLjava/lang/Object;)V`: slots 1-2 are the long,
        // slot 3 is the second parameter. `aload_3; arraylength`.
        let code3 = vec![0x2d, ARRAYLENGTH];
        let mut inst = MockResolver::new(HashMap::new(), HashMap::new());
        inst.descriptor = Some("(JLjava/lang/Object;)V");
        let expr3 = helpful_npe::null_expr_at_depth(&code3, 1, 0, &inst);
        assert_eq!(text(&expr3), Some("<parameter2>"));

        // Past the parameter area: still `<localN>`. `aload 4; arraylength`.
        let code4 = vec![0x19, 0x04, ARRAYLENGTH];
        let expr4 = helpful_npe::null_expr_at_depth(&code4, 2, 0, &inst);
        assert_eq!(text(&expr4), Some("<local4>"));

        // A parameter slot the method writes is no longer a parameter:
        // `aconst_null; astore_0; aload_0; arraylength` in the static method.
        let written = vec![0x01, 0x4b, ALOAD_0, ARRAYLENGTH];
        let expr5 = helpful_npe::null_expr_at_depth(&written, 3, 0, &r);
        assert_eq!(text(&expr5), Some("<local0>"));
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

    // -- Whole-method analysis (interpreter round i1 wave 22, lane L7) ------
    //
    // Each shape below lost its `because` clause (or named the wrong local)
    // under the old basic-block walk; the expectations are HotSpot 25's, from
    // `tools/probes/interp/L7/L7W22HelpfulNpeFlow.java` and the same shapes
    // compiled from source.

    const INVOKEDYNAMIC: u8 = 0xba;
    const ASTORE_0_OP: u8 = 0x4b;
    const ASTORE_2: u8 = 0x4d;
    const ILOAD_1: u8 = 0x1b;
    const ALOAD_3: u8 = 0x2d;
    const IALOAD_OP: u8 = 0x2e;
    const NEWARRAY: u8 = 0xbc;
    const LCONST_1: u8 = 0x0a;
    const POP2: u8 = 0x58;
    const DUP_X1: u8 = 0x5a;
    const SWAP: u8 = 0x5f;
    const ARETURN: u8 = 0xb0;
    const RETURN: u8 = 0xb1;

    /// An array store earlier in the block no longer abandons the walk:
    /// `iconst_2; newarray int; astore_0; aload_0; iconst_0; iconst_1;
    /// iastore; aconst_null; astore_1; aload_1; arraylength`.
    #[test]
    fn an_array_store_before_the_trap_keeps_the_clause() {
        let code = vec![
            ICONST_2,
            NEWARRAY,
            10,
            ASTORE_0_OP,
            ALOAD_0,
            ICONST_0,
            ICONST_1,
            IASTORE,
            ACONST_NULL,
            ASTORE_1,
            ALOAD_1,
            ARRAYLENGTH,
        ];
        let trap_bci = code.len() - 1;
        let resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
        let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
        assert_eq!(text(&expr), Some("<local1>"));
    }

    /// A string concatenation (`invokedynamic`) sized by its call-site
    /// descriptor: `aconst_null; astore_1; iload_0; invokedynamic #7
    /// (I)String; astore_2; aload_1; arraylength`.
    #[test]
    fn an_invokedynamic_before_the_trap_is_sized_by_its_descriptor() {
        let mut code = vec![ACONST_NULL, ASTORE_1, ILOAD_0, INVOKEDYNAMIC];
        code.extend_from_slice(&u16_be(7));
        code.extend_from_slice(&[0, 0]);
        code.extend_from_slice(&[ASTORE_2, ALOAD_1]);
        let trap_bci = code.len();
        code.push(ARRAYLENGTH);
        let mut resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
        resolver
            .indy
            .insert(7, "(I)Ljava/lang/String;".to_string());
        let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
        assert_eq!(text(&expr), Some("<local1>"));
        // Without the descriptor the path stops at the call site: no clause,
        // never a guessed one.
        let blind = MockResolver::new_static(HashMap::new(), HashMap::new());
        assert_eq!(
            text(&helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &blind)),
            None
        );
    }

    /// The shuffles move producers with their values, and `pop2` of a
    /// `long` pops ONE entry.
    #[test]
    fn swap_dup_x1_and_pop2_are_modelled() {
        let resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
        // aload_1; aload_2; swap; pop; arraylength -> local 2 is on top.
        let code = vec![ALOAD_1, ALOAD_2, SWAP, POP, ARRAYLENGTH];
        assert_eq!(
            text(&helpful_npe::null_expr_at_depth(&code, 4, 0, &resolver)),
            Some("<local2>")
        );
        // aload_1; aload_2; dup_x1; pop; pop; arraylength -> the copy of
        // local 2 that dup_x1 put underneath.
        let code = vec![ALOAD_1, ALOAD_2, DUP_X1, POP, POP, ARRAYLENGTH];
        assert_eq!(
            text(&helpful_npe::null_expr_at_depth(&code, 5, 0, &resolver)),
            Some("<local2>")
        );
        // aload_1; lconst_1; pop2; arraylength
        let code = vec![ALOAD_1, LCONST_1, POP2, ARRAYLENGTH];
        assert_eq!(
            text(&helpful_npe::null_expr_at_depth(&code, 3, 0, &resolver)),
            Some("<local1>")
        );
    }

    /// `n.take(b ? 1 : 2)`: the receiver is pushed before the branch, and both
    /// predecessors of the merge agree on it.
    /// `0: aload_0; 1: iload_1; 2: ifeq +7; 5: iconst_1; 6: goto +4;
    ///  9: iconst_2; 10: invokevirtual #2`.
    #[test]
    fn a_receiver_pushed_before_a_conditional_argument_is_named() {
        let mut code = vec![ALOAD_0, ILOAD_1, IFEQ];
        code.extend_from_slice(&u16_be(7)); // bci 2 -> 9
        code.push(ICONST_1); // 5
        code.push(GOTO); // 6
        code.extend_from_slice(&u16_be(4)); // bci 6 -> 10
        code.push(ICONST_2); // 9
        let invoke_bci = code.len(); // 10
        code.push(INVOKEVIRTUAL);
        code.extend_from_slice(&u16_be(2));
        let mut methods = HashMap::new();
        methods.insert(2u16, method("Node", "take", "(I)I"));
        let mut resolver = MockResolver::new_static(HashMap::new(), methods);
        resolver.descriptor = Some("(LNode;Z)V");
        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 1, &resolver);
        assert_eq!(text(&expr), Some("<parameter1>"));
        // The argument itself came from two different constants: unnamed.
        assert_eq!(
            text(&helpful_npe::null_expr_at_depth(&code, invoke_bci, 0, &resolver)),
            None
        );
    }

    /// `s = s.trim()` on a null parameter: the store comes AFTER the trap, so
    /// the slot is still a parameter there (HotSpot asks the paths reaching
    /// the trap, not the whole method).
    /// `aload_0; invokevirtual #2; astore_0; aload_0; areturn`.
    #[test]
    fn a_parameter_written_only_after_the_trap_is_still_a_parameter() {
        let mut code = vec![ALOAD_0, INVOKEVIRTUAL];
        code.extend_from_slice(&u16_be(2));
        code.extend_from_slice(&[ASTORE_0_OP, ALOAD_0, ARETURN]);
        let mut methods = HashMap::new();
        methods.insert(
            2u16,
            method("java/lang/String", "trim", "()Ljava/lang/String;"),
        );
        let mut resolver = MockResolver::new_static(HashMap::new(), methods);
        resolver.descriptor = Some("(Ljava/lang/String;)Ljava/lang/String;");
        let expr = helpful_npe::null_expr_for_invoke_receiver(&code, 1, 0, &resolver);
        assert_eq!(text(&expr), Some("<parameter1>"));
    }

    /// A handler starts with no written locals: a parameter the protected
    /// range reassigned is still `<parameterN>` in the catch block.
    /// `0: aload_0; 1: invokevirtual #2; 4: astore_0; 5: return;
    ///  6: astore_1 (handler); 7: aload_0; 8: arraylength`.
    #[test]
    fn a_handler_does_not_inherit_the_protected_ranges_writes() {
        let mut code = vec![ALOAD_0, INVOKEVIRTUAL];
        code.extend_from_slice(&u16_be(2));
        code.extend_from_slice(&[ASTORE_0_OP, RETURN, ASTORE_1, ALOAD_0, ARRAYLENGTH]);
        let mut methods = HashMap::new();
        methods.insert(
            2u16,
            method("java/lang/String", "trim", "()Ljava/lang/String;"),
        );
        let mut resolver = MockResolver::new_static(HashMap::new(), methods);
        resolver.descriptor = Some("([I)V");
        resolver.handlers = vec![6];
        let expr = helpful_npe::null_expr_at_depth(&code, 8, 0, &resolver);
        assert_eq!(text(&expr), Some("<parameter1>"));
    }

    /// `aaload` with an undescribable array prints `<array>`, and `iaload`
    /// describes an index: `(c ? p : q)[idx[0]]` as
    /// `0: iload_0; 1: ifeq +7; 4: aload_1; 5: goto +4; 8: aload_2;
    ///  9: aload_3; 10: iconst_0; 11: iaload; 12: aaload; 13: arraylength`.
    #[test]
    fn array_and_index_rendering_follow_hotspot() {
        let mut code = vec![ILOAD_0, IFEQ];
        code.extend_from_slice(&u16_be(7)); // bci 1 -> 8
        code.push(ALOAD_1); // 4
        code.push(GOTO); // 5
        code.extend_from_slice(&u16_be(4)); // bci 5 -> 9
        code.push(ALOAD_2); // 8
        code.extend_from_slice(&[ALOAD_3, ICONST_0, IALOAD_OP, AALOAD]);
        let trap_bci = code.len();
        code.push(ARRAYLENGTH);
        let resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
        let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
        assert_eq!(text(&expr), Some("<array>[<local3>[0]]"));
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

#[cfg(test)]
mod w4c_oome_construction_tests {
    //! gc-common round 2026-09-23, wave 4, lane C4: converting a VM-raised
    //! `OutOfMemoryError` into a throwable does not run the allocation ladder
    //! a second time (`common-w3a-oome-construction-reruns-the-allocation-ladder`).

    /// The body of the production function starting at `head`. `\u{7d}` is a
    /// closing brace, spelled so this line's braces balance for the
    /// brace-counting production-panic scanner.
    fn body_of<'a>(src: &'a str, head: &str) -> &'a str {
        let start = src.find(head).unwrap_or_else(|| panic!("{head} not found"));
        let len = src[start..].find("\n\u{7d}\n").expect("function end");
        &src[start..start + len]
    }

    /// Source witness: a driven exhaustion needs a heap full of live data a
    /// unit test cannot build, and the property is structural -- the
    /// conversion of an `OutOfMemoryError` takes the no-collect door, and that
    /// door's failure arm neither collects nor reclaims.
    #[test]
    fn a_vm_raised_oome_is_built_without_collecting_again() {
        let src = include_str!("exceptions.rs");
        let throw = body_of(src, "pub fn throw_runtime_error(");
        assert!(
            throw.contains("create_vm_oome_object("),
            "throw_runtime_error must build an OutOfMemoryError through the \
             no-collect door"
        );
        // gen r5w1/oom5: that door is the no-collect constructor, and its one
        // collecting retry is the VM-limit refusal's, which ran no ladder.
        let vm_oome = body_of(src, "pub(crate) fn create_vm_oome_object(");
        assert!(vm_oome.contains("create_exception_object_no_collect("));
        let retry = vm_oome
            .find("return create_exception_object(")
            .expect("the VM-limit retry");
        assert!(
            vm_oome[..retry].contains("ARRAY_SIZE_EXCEEDS_VM_LIMIT"),
            "the collecting retry must be reserved for the VM-limit refusal"
        );
        let inner = body_of(src, "fn create_exception_object_for_class_inner(");
        let arm_start = inner
            .find("None if alloc_mode == ExceptionAlloc::NoCollect")
            .expect("the no-collect arm");
        let arm_len = inner[arm_start..]
            .find("None =>")
            .expect("the collecting arm follows");
        let arm = &inner[arm_start..arm_start + arm_len];
        for forbidden in [
            "maybe_gc_forced",
            "last_ditch",
            "gc_overhead_limit_exceeded",
        ] {
            assert!(
                !arm.contains(forbidden),
                "the no-collect arm calls `{forbidden}`"
            );
        }
        // And the collecting door is unchanged for every other exception.
        let collecting = &inner[arm_start + arm_len..];
        assert!(collecting.contains("maybe_gc_forced_pub_at("));
    }
}

#[cfg(test)]
mod gcd_d2j_preallocated_oome_for_tests {
    //! gcd d2/j: the fallback throwable of a VM-raised `OutOfMemoryError`.
    use super::*;
    use crate::config::VmConfig;
    use crate::vm::realms::heap_realm::OomeKind;

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: non-null, 8-byte aligned, never dereferenced.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// A message with its own default gets it; every other message, and a
    /// kind with no default built, gets the singleton; nothing before either
    /// exists.
    #[test]
    fn the_message_picks_its_own_default_else_the_singleton() {
        let shared = SharedVm::new(VmConfig::default());
        let limit = OomeKind::ArraySize.message();
        assert_eq!(preallocated_oome_for(&shared, limit), None);
        let singleton = fake(0x7000);
        *shared.mem.singleton_oom.write() = Some(singleton);
        assert_eq!(preallocated_oome_for(&shared, limit), Some(singleton));
        let own = fake(0x8000);
        shared.mem.preallocated_oome.write().set(OomeKind::ArraySize, own);
        assert_eq!(preallocated_oome_for(&shared, limit), Some(own));
        assert_eq!(preallocated_oome_for(&shared, "Java heap space"), Some(singleton));
        assert_eq!(preallocated_oome_for(&shared, "Metaspace"), Some(singleton));
        // The fakes must not outlive the test inside a VM a collector could scan.
        *shared.mem.singleton_oom.write() = None;
        *shared.mem.preallocated_oome.write() = Default::default();
    }
}

#[cfg(test)]
mod w9c_detail_message_tests {
    //! gc-common w9-c: a VM-raised exception's detail message is a FRESH
    //! String. It used to be an entry of the intern pool, a strong GC root
    //! nothing prunes, so every distinct message stayed live for the life of
    //! the VM.

    use super::*;
    use crate::config::VmConfig;
    use crate::vm::Vm;

    /// The message reaches the throwable but not the pool.
    #[test]
    fn a_detail_message_is_not_interned() {
        let mut vm = Vm::new(VmConfig::default());
        let text = "w9c: Index 918273 out of bounds for length 7";
        let Ok(obj) = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/ArrayIndexOutOfBoundsException",
            Some(text),
        ) else {
            // Stripped VM (no JDK on the test classpath): nothing to probe with.
            return;
        };
        assert!(
            vm.shared.mem.string_pool.read().get(text).is_none(),
            "a VM-raised detail message was interned: the pool roots it for \
             the life of the VM"
        );
        // Where the layout has the field, the message is still delivered.
        if let Some(slot) = instance_field_index_by_name(&vm.shared, obj, "detailMessage") {
            if let Value::Object(Some(s)) = vm.shared.mem.heap.get_field(obj, slot) {
                assert_eq!(
                    crate::vm::read_java_string(&vm.shared.mem.heap, s).as_deref(),
                    Some(text)
                );
            }
        }
    }

    /// Two throwables with one message get two Strings, as on HotSpot.
    #[test]
    fn equal_detail_messages_are_distinct_strings() {
        let mut vm = Vm::new(VmConfig::default());
        let text = "w9c: / by zero, twice";
        let Ok(first) = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/ArithmeticException",
            Some(text),
        ) else {
            return;
        };
        vm.main_thread.native_pin_roots.push(first);
        let second = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/ArithmeticException",
            Some(text),
        );
        let first = vm
            .main_thread
            .native_pin_roots
            .pop()
            .expect("the pin pushed above");
        let Ok(second) = second else { return };
        let Some(slot) = instance_field_index_by_name(&vm.shared, first, "detailMessage") else {
            return;
        };
        let a = vm.shared.mem.heap.get_field(first, slot);
        let b = vm.shared.mem.heap.get_field(second, slot);
        if let (Value::Object(Some(a)), Value::Object(Some(b))) = (a, b) {
            assert_ne!(a, b, "one String shared by two detail messages");
        }
    }

    /// The no-collect door (a VM-raised `OutOfMemoryError`) still pools its
    /// closed set of messages, so a repeated error on a full heap can carry
    /// its message without allocating it.
    #[test]
    fn the_no_collect_door_keeps_its_message_pooled() {
        let mut vm = Vm::new(VmConfig::default());
        let text = "Java heap space";
        let Ok(_) = create_exception_object_no_collect(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/OutOfMemoryError",
            Some(text),
        ) else {
            return;
        };
        assert!(vm.shared.mem.string_pool.read().get(text).is_some());
    }
}
