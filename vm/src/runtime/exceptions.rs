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
use std::sync::{Arc, OnceLock};

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
    /// `CRATONVM_HELPFUL_NPE_OPCODES` gate of the VM configured by `config`.
    /// Returns `None` (unmessaged NPE, HotSpot's shape) when the flag is off
    /// or no action was recorded.
    pub fn jit_npe_message_gated(config: &crate::config::VmConfig, code: u8) -> Option<String> {
        // `-XX:-ShowCodeDetailsInExceptionMessages` suppresses the message
        // entirely, matching HotSpot.
        if crate::runtime::env_cache::helpful_npe_suppressed(config) {
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

    /// The per-bci operand states of one [`Analysis`] run, kept so a caller
    /// can ask where several operands came from without re-running it.
    /// Round 16 (lane compatdrain, proposal C5W4-1): the null-argument query
    /// (`runtime::null_arg_message`) walks a method's straight-line prefix
    /// and asks, at each instruction, which entries are its parameter.
    pub struct OperandOrigins {
        an: Analysis,
    }

    impl OperandOrigins {
        /// Run the analysis until the scan reaches `upto_bci` with a state.
        pub fn run(code: &[u8], upto_bci: usize, resolver: &dyn CpResolver) -> Option<Self> {
            Analysis::run(code, upto_bci, resolver).map(|an| OperandOrigins { an })
        }

        /// The local slot whose ORIGINAL value is the entry `depth_below_top`
        /// entries below the top of the stack before the instruction at `bci`:
        /// the entry was pushed by an `aload` of that slot, and no path to
        /// `bci` has written the slot. `None` for anything else (another
        /// producer, a merged entry, a written slot, no state at `bci`).
        pub fn unwritten_local_at(
            &self,
            code: &[u8],
            bci: usize,
            depth_below_top: usize,
        ) -> Option<u16> {
            let state = self.an.state(bci)?;
            let idx = state.stack.len().checked_sub(depth_below_top + 1)?;
            let producer_bci = state.stack.get(idx)?.producer_bci?;
            match Instruction::decode(code, producer_bci).ok()?.0 {
                Instruction::Aload(slot) if !state.local_written(slot) => Some(slot),
                _ => None,
            }
        }

        /// The operand-stack height before the instruction at `bci`, or
        /// `None` when the run gave `bci` no state.
        pub fn height_at(&self, bci: usize) -> Option<usize> {
            self.an.state(bci).map(|s| s.stack.len())
        }
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

/// `Throwable.backtrace`'s slot in this VM (resolved once, cached per VM):
/// where the throwable-owned backtrace holder is stored (interpreter round i3
/// wave 82, lane L2, `SharedVm::retain_captured_backtrace`).
pub(crate) fn throwable_backtrace_slot(shared: &SharedVm) -> Option<usize> {
    throwable_own_field_index(shared, "backtrace")
}

/// Does `backtrace` (the value of `obj`'s `Throwable.backtrace`) mark a trace
/// this VM's capture retained: the registry's self-reference marker, or --
/// with `CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1` -- the throwable-owned
/// holder (interpreter round i3 wave 82, lane L2)?
fn backtrace_marks_our_capture(shared: &SharedVm, obj: ObjectRef, backtrace: Value) -> bool {
    match backtrace {
        Value::Object(Some(marker)) if marker == obj => true,
        Value::Object(Some(_)) => {
            shared
                .threads
                .throwable_layout_cache
                .backtrace_in_object_enabled()
                && shared.backtrace_holder_of(obj).is_some()
        }
        _ => false,
    }
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
    if !backtrace_marks_our_capture(shared, obj, shared.mem.heap.get_field(obj, bt_idx)) {
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
                cratonvm_types::ClassLoaderId::Bootstrap | cratonvm_types::ClassLoaderId::Extension
            ),
            declares_fill: class.methods.iter().any(|m| {
                !m.is_static() && &*m.name == "fillInStackTrace" && m.descriptor.starts_with("()L")
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
        hotspot_external_name(&class_name),
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
    let class = hotspot_external_name(&class_name);
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
///
/// A HIDDEN class (a lambda proxy stored into a `String[]` through an
/// `Object[]` view) keeps its `/0x<hex>` tail, as `Klass::external_name()`
/// does (`convert_hidden_name_to_java`), also as the bottom element of an
/// array (`[LFoo$$Lambda/0x...;`); this used to dot it
/// (`ArrayStoreException: Foo$$Lambda.0x...`). Interpreter round i4 wave 84,
/// lane L2. Judged by the name's shape, as [`external_name_keeping_hidden_tail`]
/// does: a `/0x<hex>` LAST segment, which no javac-compiled class has (an
/// identifier cannot start with a digit). Also the rendering every VM-side
/// message text that names a class through `Klass::external_name()` /
/// `Class.getName()` uses (JIT round 21 wave 14, lane hidden-text).
pub(crate) fn hotspot_external_name(internal: &str) -> String {
    match internal
        .strip_suffix(';')
        .filter(|_| internal.starts_with('['))
    {
        Some(body) => format!("{};", external_name_keeping_hidden_tail(body, true)),
        None => external_name_keeping_hidden_tail(internal, true),
    }
}

/// `CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS` (JIT round 21 wave 22, lane
/// hidden-resid; item 7 of `r21-review-w15-native-findings-FIXED-20261020.md`;
/// correctness, both modes, default ON, `=0` restores the shape judgement):
/// a message text that names a class it HOLDS asks the class's own hidden
/// flag ([`hotspot_external_name_of_class`]) instead of judging the name's
/// `/0x<hex>` shape, so an ordinary class whose last segment is literally
/// `0x1F` (legal bytes, no javac output) is dotted as HotSpot's
/// `Klass::external_name()` dots it (`p.0x1F`). Read on message paths only.
pub(crate) fn hidden_name_text_by_class_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS")
}

/// `CRATONVM_CAST_TEXT_BY_RESOLVED_CLASS` (JIT round 21 wave 26, lane
/// hexname; probe `R21Fz10HiddenCastMsg1`; correctness, both modes, default
/// ON, `=0` restores the previous texts): a cast refusal names its TARGET by
/// the class the site resolved (a hidden class's self `checkcast Hd` names
/// `Hd/0x..`, as `Klass::external_name()` does, not the pool's `Hd`), and the
/// message funnel reads an object array's module/loader clause from the
/// operand's noted id walked to its bottom class (so an array of a hidden
/// class, which no name lookup finds, still gets HotSpot's full text).
/// Read on cast-refusal paths only.
pub(crate) fn cast_text_by_resolved_class_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_CAST_TEXT_BY_RESOLVED_CLASS")
}

/// The internal name of the class a cast site RESOLVED, when it is not what
/// the site's pool spells (`pool_name`) and its bottom class is hidden: a
/// hidden class's self reference (`checkcast Hd` in `Hd`'s own body, or
/// `[LHd;`) resolves to the hidden class, whose stored name carries the
/// `/0x<hex>` tail. `None` otherwise (the site keeps the pool's name), under
/// `CRATONVM_CAST_TEXT_BY_RESOLVED_CLASS=0`, or for an id the store does not
/// hold. The caller must hold no class-manager guard.
pub(crate) fn resolved_hidden_cast_target_name(
    shared: &SharedVm,
    target: ClassId,
    pool_name: &str,
) -> Option<String> {
    if !cast_text_by_resolved_class_enabled() {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    let name = cm.get_class(target)?.name.to_string();
    // Same array depth only: an array target's noted id may be its
    // COMPONENT's, whose bare name must never replace `[L..;`.
    let dims = |n: &str| n.bytes().take_while(|&b| b == b'[').count();
    if name == pool_name
        || dims(&name) != dims(pool_name)
        || bottom_class_hidden_in(&cm, target, &name) != Some(true)
    {
        return None;
    }
    Some(name)
}

/// The external text of a refused cast's TARGET: the resolved hidden class's
/// own name when [`resolved_hidden_cast_target_name`] answers (its tail kept),
/// otherwise the site's previous rendering (`fallback`, which then also
/// covers `=0`).
pub(crate) fn cast_target_display(
    shared: &SharedVm,
    target: Option<ClassId>,
    pool_name: &str,
    fallback: impl FnOnce(&str) -> String,
) -> String {
    match target.and_then(|t| resolved_hidden_cast_target_name(shared, t, pool_name)) {
        Some(name) => hotspot_external_name_by_bottom_flag(&name, true),
        None => fallback(pool_name),
    }
}

/// [`hotspot_external_name`] for a class whose `Class` (so whose `hidden`
/// flag) is at hand: the flag decides the `/0x<hex>` tail, not the shape
/// (`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`, default ON). An array class's own
/// flag says nothing about its bottom element, so a `[`-name keeps the shape
/// rule, as does `=0`.
pub(crate) fn hotspot_external_name_of_class(internal: &str, hidden: bool) -> String {
    hotspot_external_name_by_flag(internal, hidden, hidden_name_text_by_class_enabled())
}

/// The decision of [`hotspot_external_name_of_class`] from its facts.
fn hotspot_external_name_by_flag(internal: &str, hidden: bool, by_class: bool) -> String {
    if by_class && !internal.starts_with('[') {
        external_name_keeping_hidden_tail(internal, hidden)
    } else {
        hotspot_external_name(internal)
    }
}

/// JIT round 21 wave 24, lane names (item 5 of
/// `r21-review-w23-findings-FIXED-20261020.md`, under
/// `CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`): the hidden flag of the BOTTOM class
/// of `internal` (a class name or an array descriptor), read from `class_id`
/// -- the class itself, or any class on its array chain (a reference array's
/// header carries its COMPONENT's id) -- walked to the end of `array_info`.
/// HotSpot's `Klass::external_name()` keeps the `/0x<hex>` tail of an object
/// array exactly when its bottom class is hidden. `None` (the caller keeps the
/// shape rule) when the id is not in the store, the chain breaks, the
/// descriptor's bottom is a primitive, or the bottom class reached is not the
/// one `internal` names (so a mismatched id can never pick the flag).
pub(crate) fn bottom_class_hidden_in(
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
    internal: &str,
) -> Option<bool> {
    let dims = internal.bytes().take_while(|&b| b == b'[').count();
    let bottom_name = if dims == 0 {
        internal
    } else {
        internal[dims..].strip_prefix('L')?.strip_suffix(';')?
    };
    let mut class = cm.get_class(class_id)?;
    // Bounded: a JVMS array has at most 255 dimensions.
    for _ in 0..=255 {
        match class.array_info.as_ref() {
            Some(info) => class = cm.get_class(info.component_class_id)?,
            None => break,
        }
    }
    if class.array_info.is_some() || class.name.starts_with('[') || &*class.name != bottom_name {
        return None;
    }
    Some(class.hidden)
}

/// [`bottom_class_hidden_in`] under its own class-manager read (the caller
/// must hold none).
pub(crate) fn bottom_class_hidden(shared: &SharedVm, class_id: ClassId, internal: &str) -> Option<bool> {
    let cm = shared.classes.class_manager.read();
    bottom_class_hidden_in(&cm, class_id, internal)
}

/// `internal` (a class name or an array descriptor) as
/// `Klass::external_name()` spells it when its bottom class's hidden flag is
/// `bottom_hidden`: dotted, the `/0x<hex>` tail kept only for a hidden bottom,
/// the descriptor's brackets and `L`..`;` kept; a primitive descriptor as is.
fn hotspot_external_name_by_bottom_flag(internal: &str, bottom_hidden: bool) -> String {
    let dims = internal.bytes().take_while(|&b| b == b'[').count();
    if dims == 0 {
        return external_name_keeping_hidden_tail(internal, bottom_hidden);
    }
    match internal[dims..]
        .strip_prefix('L')
        .and_then(|body| body.strip_suffix(';'))
    {
        Some(body) => format!(
            "{}L{};",
            &internal[..dims],
            external_name_keeping_hidden_tail(body, bottom_hidden)
        ),
        None => internal.to_string(),
    }
}

/// The class-holding rendering of `internal` (wave 24, lane names): with
/// `CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS` on and the bottom class's flag known,
/// [`hotspot_external_name_by_bottom_flag`]; otherwise `fallback`, the text the
/// site produced before (so `=0` restores it exactly).
pub(crate) fn external_name_by_bottom_class_or(
    internal: &str,
    bottom_hidden: Option<bool>,
    fallback: impl FnOnce(&str) -> String,
) -> String {
    match bottom_hidden.filter(|_| hidden_name_text_by_class_enabled()) {
        Some(hidden) => hotspot_external_name_by_bottom_flag(internal, hidden),
        None => fallback(internal),
    }
}

/// The `ArrayStoreException` text of a refused `aastore` / JIT array store,
/// `internal` being the value's display name (`cce_display_class_name`): with
/// the value's bottom class known (`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`), its
/// external name decided by that class's hidden flag; otherwise `internal`
/// unchanged, for the funnel's shape rule (`hotspot_vm_type_error_message`).
/// The rendered text survives the funnel unchanged: an ordinary name has no
/// `/` left, and a hidden one keeps its tail under the shape rule too. The
/// caller must hold no class-manager guard.
pub(crate) fn array_store_element_text(shared: &SharedVm, value: ObjectRef, internal: String) -> String {
    if !hidden_name_text_by_class_enabled() {
        return internal;
    }
    let bottom = bottom_class_hidden(shared, shared.mem.heap.class_id_of(value), &internal);
    match bottom {
        Some(hidden) => hotspot_external_name_by_bottom_flag(&internal, hidden),
        None => internal,
    }
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
/// `None` for a user-defined loader: HotSpot renders those as
/// `<loader class name> @<identity hash>` (or `'<name>' @<hash>`), the
/// loader's own `ClassLoader.nameAndId`, which the callers then read
/// ([`user_loader_name_and_id`]); only when that read declines is the clause
/// left in its plain wording. Never a fabricated hash. (j27 wave 3 lane
/// BACKLOG, MH517-3: the old "a value we cannot reproduce" wording was stale.)
fn loader_name_and_id(loader: cratonvm_types::ClassLoaderId) -> Option<&'static str> {
    use cratonvm_types::ClassLoaderId;
    match loader {
        ClassLoaderId::Bootstrap => Some("'bootstrap'"),
        ClassLoaderId::Extension => Some("'platform'"),
        ClassLoaderId::Application => Some("'app'"),
        ClassLoaderId::UserDefined(_) => None,
    }
}

/// The loader a message names for a class this VM defined with `loader_id` in
/// `module`: `loader_id` itself, except a boot-namespace class of a named
/// module the JDK's `ModuleLoaderMap` assigns to the PLATFORM loader
/// (`java.sql`, `java.net.http`, ...), which HotSpot's platform loader defines
/// and names `'platform'` (measured on JDK 25, probe
/// `L4W46LambdaSiteForeignStaticArg` row `sql-0`). This VM defines every image
/// class in its boot namespace, so the table is asked, from its memo only
/// (no Java runs here); a cold memo keeps `loader_id` (interpreter round i1
/// wave 47, lane L4).
fn message_loader_id(
    shared: &SharedVm,
    loader_id: cratonvm_types::ClassLoaderId,
    module: Option<&str>,
) -> cratonvm_types::ClassLoaderId {
    use cratonvm_types::ClassLoaderId;
    match (loader_id, module) {
        (ClassLoaderId::Bootstrap, Some(module))
            if module != "java.base"
                && cratonvm_native_builtins::classloader::memoised_module_is_platform(
                    &shared.loader_identity,
                    module,
                ) == Some(true) =>
        {
            ClassLoaderId::Extension
        }
        _ => loader_id,
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
    if display_name.contains("$$Lambda/0x") {
        if let Some(origin) = lambda_proxy_origin(shared, display_name) {
            return Some(origin);
        }
    }
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
    // docs/feature-designs/jdk-only-mode.md §1):
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
    let module = message_module_of(class, &cm.module_registry);
    let loader_id = message_loader_id(shared, class.loader_id, module.as_deref());
    let module = versioned_message_module(class, &cm.module_registry, module);
    drop(cm);
    // A user-defined loader is named by its `nameAndId` under `--jdk-only`
    // (interpreter round i1 wave 32), and under `--compatible` with
    // `CRATONVM_COMPAT_MODULE_MESSAGES` (wave 49, lane L6); otherwise the
    // bare wording.
    let loader = match loader_name_and_id(loader_id) {
        Some(builtin) => builtin.into(),
        None if shared.natives.native_methods.messages_name_user_loaders() => {
            user_loader_name_and_id(shared, class_id)?.into()
        }
        None => return None,
    };
    Some(KlassOrigin {
        module,
        loader,
        loader_id,
    })
}

/// The clause of a lambda proxy named `display_name`
/// (`interpreter::lambda_proxy_display_name`: `<host>$$Lambda/0x<id>`): its
/// host class's module and loader, as HotSpot's hidden lambda class is defined
/// in its host's (`Lookup.defineHiddenClass`), so a `checkcast` of a lambda
/// prints `X$$Lambda/0x... is in unnamed module of loader 'app'`. `None` for a
/// name that is not exactly this VM's name of a live proxy (interpreter round
/// i1 wave 49, lane L7).
fn lambda_proxy_origin(shared: &SharedVm, display_name: &str) -> Option<KlassOrigin> {
    // Merge of round 15 (the `name_user_loaders` parameter, TH1-2) with i1
    // wave 49 (this function): user loaders are named exactly when the
    // module-message rule says so.
    let name_user_loaders = shared.natives.native_methods.messages_name_user_loaders();
    let (_, hex) = display_name.rsplit_once("$$Lambda/0x")?;
    let proxy = ClassId::new(u32::from_str_radix(hex, 16).ok()?);
    if crate::runtime::interpreter::lambda_proxy_display_name(shared, proxy)? != display_name {
        return None;
    }
    let (host_id, host) = crate::runtime::interpreter::lambda_proxy_host(shared, proxy)?;
    let host_display = host.replace('/', ".");
    match host_id {
        Some(host_id) => klass_origin_by_id(shared, host_id, &host_display, name_user_loaders),
        None => {
            let host_id = shared
                .classes
                .class_manager
                .read()
                .find_unique_class_by_name(&host)?;
            klass_origin_by_id(shared, host_id, &host_display, name_user_loaders)
        }
    }
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
///
/// `name_user_loaders` false answers `None` for a class of a user-defined
/// loader, as the by-name [`klass_origin`] does under `--compatible` (the
/// bare wording); the message funnel passes `messages_name_user_loaders()`
/// (round 15 wave 7, lane throw2, TH1-2; interpreter round i1 wave 49, lane
/// L6), every other caller `true`.
fn klass_origin_by_id(
    shared: &SharedVm,
    class_id: ClassId,
    display_name: &str,
    name_user_loaders: bool,
) -> Option<KlassOrigin> {
    // An array's clause is its bottom component's (see `klass_origin`); an
    // array class's own loader is not it. The name path resolves it, except
    // for a HIDDEN bottom class, which no name lookup finds: HotSpot printed
    // `class [LHd/0x..; cannot be cast to class [Ljava.lang.String; ([LHd/0x..;
    // is in unnamed module of loader 'app'; ...)` where this dropped the whole
    // rebuild to the bare `X cannot be cast to Y` (JIT round 21 wave 26, lane
    // hexname; probe `R21Fz10HiddenCastMsg1` rows `cc String[]`,
    // `cross hiddenArr toSeg`). Under `CRATONVM_CAST_TEXT_BY_RESOLVED_CLASS`
    // (default ON) the id -- a reference array's header id is its COMPONENT's,
    // a resolved target's may be the array class -- is walked down
    // `array_info` to the bottom class, which must spell the display's
    // bottom; `=0` (or a primitive bottom) asks by name, as before.
    let (class_id, display_name) = if display_name.starts_with('[') {
        if !cast_text_by_resolved_class_enabled() {
            return None;
        }
        let dims = display_name.bytes().take_while(|&b| b == b'[').count();
        let bottom_display = display_name[dims..].strip_prefix('L')?.strip_suffix(';')?;
        let cm = shared.classes.class_manager.read();
        let mut id = class_id;
        // Bounded: a JVMS array has at most 255 dimensions.
        for _ in 0..=255 {
            match cm.get_class(id)?.array_info.as_ref() {
                Some(info) => id = info.component_class_id,
                None => break,
            }
        }
        (id, bottom_display)
    } else {
        (class_id, display_name)
    };
    let (loader_id, module) = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        if class.array_info.is_some() || class.name.starts_with('[') {
            return None;
        }
        // A hidden class is displayed with its `/0x<hex>` tail kept
        // (interpreter round i5 wave 104, lane L7).
        if !internal_name_spells_display(&class.name, display_name)
            && !(class.hidden
                && external_name_keeping_hidden_tail(&class.name, true) == display_name)
        {
            return None;
        }
        let module = message_module_of(class, &cm.module_registry);
        (
            message_loader_id(shared, class.loader_id, module.as_deref()),
            versioned_message_module(class, &cm.module_registry, module),
        )
    };
    let loader = match loader_name_and_id(loader_id) {
        Some(builtin) => builtin.into(),
        None if !name_user_loaders => return None,
        None => user_loader_name_and_id(shared, class_id)?.into(),
    };
    Some(KlassOrigin {
        module,
        loader,
        loader_id,
    })
}

/// Is `display` the dotted spelling of the internal name `internal`
/// (`internal.replace('/', ".") == display`, without building the copy)?
fn internal_name_spells_display(internal: &str, display: &str) -> bool {
    internal.len() == display.len()
        && internal
            .bytes()
            .zip(display.bytes())
            .all(|(i, d)| if i == b'/' { d == b'.' } else { i == d })
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

/// [`message_module_of`]'s `module` as a clause spells it: HotSpot's
/// `name@version` when `ModuleEntry::should_show_version()` holds
/// (`access_control::message_module_version`: a module-path or non-boot
/// layer module with a version; never a JDK module of the image). The bare
/// name feeds `message_loader_id`; this, [`KlassOrigin::module`]. Interpreter
/// round i4 wave 99, lane L5
/// (`i98-orch-module-clauses-omit-version-of-named-application-modules`).
fn versioned_message_module(
    class: &crate::classloading::Class,
    registry: &crate::classloading::module::ModuleRegistry,
    module: Option<String>,
) -> Option<String> {
    let module = module?;
    Some(
        match crate::classloading::access_control::message_module_version(class, registry) {
            Some(version) => format!("{module}@{version}"),
            None => module,
        },
    )
}

/// A user-defined loader's `ClassLoader.nameAndId`, read through the class's
/// mirror (`Class.classLoader`).
fn user_loader_name_and_id(shared: &SharedVm, class_id: ClassId) -> Option<String> {
    // gcg g4/oo: the fallible mirror. A class a user-defined loader defined
    // has its mirror already (`defineClass` returned it), so this only ever
    // declines where the infallible arm would have aborted the process on a
    // full heap while building an exception message.
    let mirror = crate::vm::try_get_or_create_class_mirror(shared, class_id)?;
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

/// HotSpot's `Klass::class_in_module_of_loader`. `origin.module` already
/// carries the module's `@version` where HotSpot shows one
/// ([`versioned_message_module`], wave 99, lane L5); a JDK module of the
/// image shows none (`java.base`, measured).
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
///
/// Round 15 wave 5 (lane throw1, cost item C8 of
/// `r15w5-throw1-plain-throw-costs-3x-hotspot-CLOSED-20260930.md`): lock-free. This is
/// asked on EVERY refused cast, and it was a process-wide `Mutex<HashSet>`
/// probed with a SipHash fed one byte at a time -- one lock every thread's
/// `ClassCastException` serialised on, which is exactly the "failing cast as a
/// type test" traffic the paragraph above describes. It is now an
/// open-addressed table of `2 x` [`CAST_FORENSICS_PAIR_CAP`] atomic keys: a
/// repeat is one or two loads, a first sighting one compare-and-swap. The rule
/// is unchanged, with two diagnostic-only differences: the pair is one 64-bit
/// hash of (class, folded name) where it was a class id and a 64-bit hash of
/// the name (a collision loses a first report either way), and two threads
/// racing past the cap can record a few pairs more than it.
pub(crate) fn cast_refusal_forensics_admitted(actual_cid: u32, target: &str) -> bool {
    if actual_cid == 0 {
        return true;
    }
    static SEEN: CastForensicsSeen = CastForensicsSeen::new();
    SEEN.first_sighting(cast_forensics_key(actual_cid, target))
}

/// The pair key of [`cast_refusal_forensics_admitted`]: FNV-1a over `target`
/// with `/` folded onto `.` (the interpreter passes the dotted binary name and
/// the JIT helper the internal slashed one, and one pair should cost one scan
/// whichever tier refuses it first), mixed with the class id; never `0`, the
/// table's empty mark.
fn cast_forensics_key(actual_cid: u32, target: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in target.bytes() {
        h ^= u64::from(if b == b'/' { b'.' } else { b });
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= u64::from(actual_cid).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    // A final avalanche so the low bits (the table index) see every input bit.
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    if h == 0 {
        1
    } else {
        h
    }
}

/// Slots of [`CastForensicsSeen`]: twice the cap, a power of two.
const CAST_FORENSICS_SLOTS: usize = 2 * CAST_FORENSICS_PAIR_CAP;
const _: () = assert!(CAST_FORENSICS_SLOTS.is_power_of_two());

/// The pairs [`cast_refusal_forensics_admitted`] has admitted, lock-free.
struct CastForensicsSeen {
    slots: [std::sync::atomic::AtomicU64; CAST_FORENSICS_SLOTS],
    len: std::sync::atomic::AtomicUsize,
}

impl CastForensicsSeen {
    const fn new() -> Self {
        CastForensicsSeen {
            slots: [const { std::sync::atomic::AtomicU64::new(0) }; CAST_FORENSICS_SLOTS],
            len: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// `true` exactly once per `key` (the caller that records it), and never
    /// once [`CAST_FORENSICS_PAIR_CAP`] keys are recorded.
    fn first_sighting(&self, key: u64) -> bool {
        use std::sync::atomic::Ordering;
        let mask = CAST_FORENSICS_SLOTS - 1;
        // Truncation: the low bits are the index.
        let mut index = (key as usize) & mask;
        for _ in 0..CAST_FORENSICS_SLOTS {
            let slot = &self.slots[index];
            let current = slot.load(Ordering::Acquire);
            if current == key {
                return false;
            }
            if current == 0 {
                if self.len.load(Ordering::Relaxed) >= CAST_FORENSICS_PAIR_CAP {
                    return false;
                }
                match slot.compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => {
                        self.len.fetch_add(1, Ordering::Relaxed);
                        return true;
                    }
                    Err(now) if now == key => return false,
                    // Another pair took this slot first: keep probing.
                    Err(_) => {}
                }
            }
            index = (index + 1) & mask;
        }
        false
    }
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
    hotspot_class_cast_message_as(shared, thread, from_display, to_display, noted, true)
        .map(|(message, _)| message)
}

/// [`hotspot_class_cast_message`], with [`klass_origin_by_id`]'s
/// `name_user_loaders`, and whether BOTH operands were answered from their
/// noted class ids -- the one case whose text is a function of the two ids
/// and the two display names alone, which [`CastMessageMemo`] relies on (a
/// by-name answer may consult the current frame's loader).
fn hotspot_class_cast_message_as(
    shared: &SharedVm,
    thread: &JvmThread,
    from_display: &str,
    to_display: &str,
    noted: Option<(ClassId, ClassId)>,
    name_user_loaders: bool,
) -> Option<(String, bool)> {
    let by_id = |id: Option<ClassId>, display: &str| {
        id.and_then(|id| klass_origin_by_id(shared, id, display, name_user_loaders))
    };
    let from_by_id = by_id(noted.map(|n| n.0), from_display);
    let to_by_id = by_id(noted.map(|n| n.1), to_display);
    let both_by_id = from_by_id.is_some() && to_by_id.is_some();
    let from = from_by_id.or_else(|| klass_origin(shared, thread, from_display))?;
    let to = to_by_id.or_else(|| klass_origin(shared, thread, to_display))?;
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
    Some((
        format!("class {from_display} cannot be cast to class {to_display} ({parenthetical})"),
        both_by_id,
    ))
}

/// Round 15 wave 7 (lane throw2, TH1-2;
/// `r15w5-throw1-plain-throw-costs-3x-hotspot-CLOSED-20260930.md` cost item C4): the
/// last cast refusal message the funnel rebuilt on this thread.
///
/// A refused cast in a loop rebuilt the same text every time: two
/// class-manager reads, the module `String`s, a user loader's `nameAndId`
/// read through the mirror, and two or three `format!`s, per throw. The text
/// is a function of the two classes and the raise site's message when both
/// operands were answered from their noted ids
/// ([`hotspot_class_cast_message_as`]'s second answer): the display names are
/// in the raise message, and a class's loader, its module and a loader's
/// `nameAndId` (a final field) do not change while its id names it. The stamp
/// (the class definition epoch and the redefinition count, what the JIT NPE
/// message memo is stamped with) covers an id that comes to name another
/// class. A `JvmThread` belongs to one VM, so the VM is not part of the key.
/// One entry: the shape it serves is one refusal repeated.
pub(crate) struct CastMessageMemo {
    stamp: (u64, u64),
    ids: (ClassId, ClassId),
    name_user_loaders: bool,
    raised: String,
    rebuilt: String,
}

/// The stamp [`CastMessageMemo`] is valid under.
fn cast_message_memo_stamp(shared: &SharedVm) -> (u64, u64) {
    (
        cratonvm_classloading::store_epochs(shared.jit.class_layout_domain)
            .class_definition_epoch(),
        cratonvm_classloading::class_redefinition_count(),
    )
}

/// `CRATONVM_CCE_MESSAGE_BY_ID` (default ON): may the compiled `checkcast`
/// note its operands' class ids for the message funnel (and the funnel use a
/// note under `--compatible`)? Round 15 wave 7, lane throw2, TH1-2.
pub(crate) fn cast_message_by_id_enabled() -> bool {
    throw1_switches().cce_message_by_id
}

/// The funnel's cast refusal rebuild for `raised` (operands `from` / `to`),
/// through [`CastMessageMemo`].
fn rebuilt_cast_message(
    shared: &SharedVm,
    thread: &mut JvmThread,
    raised: &str,
    from: &str,
    to: &str,
    noted: Option<(ClassId, ClassId)>,
) -> Option<String> {
    let name_user_loaders = shared.natives.native_methods.messages_name_user_loaders();
    let memo_ids = noted.filter(|_| throw1_switches().cce_message_memo);
    let stamp = memo_ids.map(|_| cast_message_memo_stamp(shared));
    if let (Some(ids), Some(stamp)) = (memo_ids, stamp) {
        if let Some(memo) = thread.cast_message_memo.as_deref() {
            if memo.stamp == stamp
                && memo.ids == ids
                && memo.name_user_loaders == name_user_loaders
                && memo.raised == raised
            {
                return Some(memo.rebuilt.clone());
            }
        }
    }
    let (rebuilt, both_by_id) =
        hotspot_class_cast_message_as(shared, thread, from, to, noted, name_user_loaders)?;
    if let (Some(ids), Some(stamp), true) = (memo_ids, stamp, both_by_id) {
        thread.cast_message_memo = Some(Box::new(CastMessageMemo {
            stamp,
            ids,
            name_user_loaders,
            raised: raised.to_owned(),
            rebuilt: rebuilt.clone(),
        }));
    }
    Some(rebuilt)
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
    thread: &mut JvmThread,
    error: RuntimeError,
) -> RuntimeError {
    match error {
        RuntimeError::ClassCastException { message } => {
            // The raise site's class ids, under `--jdk-only` (wave 32), under
            // `--compatible` with `CRATONVM_COMPAT_MODULE_MESSAGES` (wave 49,
            // lane L6: `messages_name_user_loaders`), and in both modes since
            // round 15 wave 7 (lane throw2, TH1-2): an id is the same answer
            // as the name whenever the name is unambiguous and the right one
            // when two loaders define it, and where user loaders are not
            // named (`--compatible` without the module-messages switch) a
            // user loader's class still gets the bare wording
            // (`klass_origin_by_id`'s `name_user_loaders`).
            // `CRATONVM_CCE_MESSAGE_BY_ID=0` restores the
            // `messages_name_user_loaders` filter.
            let noted = take_cast_operands(thread).filter(|_| {
                shared.natives.native_methods.messages_name_user_loaders()
                    || cast_message_by_id_enabled()
            });
            let rebuilt = split_cast_operands(&message).and_then(|(from, to)| {
                rebuilt_cast_message(shared, thread, &message, from, to, noted)
            });
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
    // JIT round 23 wave 1, lane L3 (proposal L1W18-1, widened;
    // `CRATONVM_THROWABLE_DIRECT_IMPLICIT_BUILD_EVERY_DOOR`, default ON): the
    // JDK's implicit exceptions raised by the interpreter, by a JIT helper
    // whose compiled caller is live (a `checkcast`'s CCE, an `aastore`'s
    // ArrayStoreException, a negative `newarray` length) or by a native, built
    // without interpreting their constructor chains when the class bytes are
    // the JDK's. A refusal changed nothing: the constructors run as before.
    //
    // The retained-trace registry's collection request is taken FIRST, as the
    // constructed path takes it before its allocation (the built throwable is
    // returned unpinned, so nothing may collect after it exists).
    if direct_build_every_door_enabled() && implicit_direct_class_slot(class_name).is_some() {
        collect_if_requested(shared, thread);
        let built = try_build_implicit_throwable_directly(shared, thread, class_name, message);
        if let Some(built) = built {
            return built;
        }
    }
    create_exception_object_constructed(shared, thread, class_name, message)
}

/// [`create_exception_object`] without the direct build: the class's
/// constructor chain always runs. For the trap-frame door, which asked the
/// direct build itself first.
pub(crate) fn create_exception_object_constructed(
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
            .class_manager_write_settling()
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
            .class_manager_write_settling()
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
    create_vm_oome_object_with_array_retry(shared, thread, class_name, message, true)
}

/// The VM OOME hooks, debt discipline and throwable shape, without a collecting
/// retry. JNI length refusals must not turn an impossible length into a GC.
pub(crate) fn create_vm_oome_object_no_collect(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    message: Option<&str>,
) -> Result<ObjectRef, MethodCallFailed> {
    create_vm_oome_object_with_array_retry(shared, thread, class_name, message, false)
}

fn create_vm_oome_object_with_array_retry(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    message: Option<&str>,
    allow_array_retry: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    // zs s6/no: the ladder this error reports is over. Its exhausted verdict
    // (opt-in `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR`) refused the ladder's own
    // attempts; the throwable, its message and its stack trace allocate as
    // they always did. No-op off ZGC and with the switch off.
    //
    // zt t2/rv (`zgc-zs10rv7-wave-s9-review-residuals-CLOSED-20261019.md` item 4):
    // only a heap refusal (`Java heap space`, or no message) SPENDS the
    // verdict. Any other VM-raised error (the VM-limit refusal, `Metaspace`)
    // drops this thread's verdict so its throwable allocates, but leaves the
    // per-heap spent mark alone: it was not the refusal the verdict judged.
    // Acts only inside the opt-in arm.
    if message.is_none_or(|m| m == "Java heap space") {
        shared.mem.heap.zgc_lift_exhausted_verdict();
    } else {
        shared.mem.heap.zgc_drop_exhausted_verdict();
    }
    // zs s10/sp: the final-refusal rule (`CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS`,
    // default ON since zt t12): after this refusal no small free-list hole
    // serves a later small request until a collection changes the picture.
    // No-op off ZGC and under `=0`.
    // zt t1/sp: only for a heap refusal (`Java heap space`, or no message).
    // The VM-limit refusal (`Requested array size exceeds VM limit`) and the
    // non-heap errors never reached the heap, so a small `alloc_raw` refusal
    // recorded since the latest collection is not the one this error reports.
    if message.is_none_or(|m| m == "Java heap space") {
        shared.mem.heap.zgc_note_final_refusal();
    }
    // Interpreter round i2 wave 63, lane L1: a C JVMTI env's
    // `ResourceExhausted`, before the throwable is built (HotSpot posts it
    // where the allocation fails); one relaxed load when none listens.
    #[cfg(feature = "experimental-debug")]
    if shared
        .debug
        .debugger_gates
        .native_event_armed(crate::debug::NativeEvent::ResourceExhausted)
        && class_name == "java/lang/OutOfMemoryError"
    {
        // gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
        // item 1c, review): on this thread's own handle.
        let tid = thread.thread_id.0;
        let text = message.unwrap_or("Java heap space");
        crate::runtime::interpreter::post_on_own_thread(thread, || {
            crate::jvmti::native_env::post_resource_exhausted(shared, tid, text)
        });
    }
    // gcg g1/o: once HotSpot's traced pool is spent, the message's traceless
    // default, under `CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE` (every door
    // that builds a VM-raised OOME comes here: the interpreter's
    // `throw_runtime_error`, the JIT's `jit_alloc_oom`, the JNI door).
    match hotspot_pool_answer(shared, message.unwrap_or("")) {
        OomePoolAnswer::Default(default) => return Ok(default),
        // zs s9/la: one of HotSpot's traced slots, served from the pool
        // preallocated at boot (`CRATONVM_ZGC_OOME_TRACED_POOL`) with a fresh
        // trace and no heap allocation, as `Universe::gen_out_of_memory_error`
        // does; without the pool (or for another message) it is built as
        // before.
        OomePoolAnswer::TracedSlot(index) => {
            if let Some(pooled) = pooled_traced_oome(shared, thread, message, index) {
                return Ok(pooled);
            }
        }
        OomePoolAnswer::Construct => {}
    }
    // gcf f4/o (`gcd-d9b-proposal-arm-the-native-oome-debt-after-delivery`):
    // under `CRATONVM_GC_OOME_DEBT_SKIP_CONSTRUCTION` the natives this
    // construction calls (`Throwable.<init>` -> `fillInStackTrace`) do not
    // pay the post-OOME debt on the heap the verdict was just taken on.
    let skip = super::interpreter::gc_and_alloc::oome_debt_skip_construction();
    if skip {
        thread.oome_construct_depth = thread.oome_construct_depth.saturating_add(1);
    }
    // The exception constructor itself may call NativeContext string helpers.
    // While it runs below a native callback, those helpers normally unwind on
    // allocation failure so the callback can report OOME. Here that unwind is
    // the failure of materializing the original OOME: turn it back into the
    // ordinary allocation error so `throw_runtime_error` can use the
    // preallocated singleton instead of replacing the caught Java error with
    // a second native-allocation panic.
    // gcg g4/oo: the construction runs under `catch_unwind` so that the
    // depth raised above is restored on EVERY exit. `catch_alloc_oom` resumes
    // any panic that is not the allocators' unwind, and a contained panic
    // (the native funnel, a JIT helper's `contain`) used to leave this
    // thread's depth raised for good: under the switch it then never paid the
    // post-OOME debt again.
    let constructed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::runtime::native_oom::catch_alloc_oom(|| {
            let created = create_exception_object_no_collect(shared, thread, class_name, message);
            let heap_refused = matches!(
                &created,
                Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::OutOfMemoryError { .. }
                )))
            );
            // gcf f2/o: HotSpot collects nothing for the VM-limit refusal (it throws
            // `Universe::out_of_memory_error_array_size`); this collecting retry was
            // only ever there because no preallocated error of that message existed.
            // Once one does (`CRATONVM_GC_PREALLOCATED_OOME_KINDS`), the caller's
            // fallback (`preallocated_oome_for`) throws it and the collection is
            // skipped -- item 5 of `gengc-r5w1-oom5-proposal-preallocated-oome-per-message`.
            if allow_array_retry
                && heap_refused
                && message == Some(super::interpreter::ARRAY_SIZE_EXCEEDS_VM_LIMIT)
                && shared
                    .mem
                    .preallocated_oome
                    .read()
                    .get(crate::vm::realms::heap_realm::OomeKind::ArraySize)
                    .is_none()
            {
                create_exception_object(shared, thread, class_name, message)
            } else {
                created
            }
        })
    }));
    if skip {
        thread.oome_construct_depth = thread.oome_construct_depth.saturating_sub(1);
    }
    let built = match constructed {
        Ok(Ok(created)) => created,
        Ok(Err(oom)) => Err(MethodCallFailed::InternalError(VmError::Runtime(
            oom.into_runtime_error(),
        ))),
        Err(panic) => std::panic::resume_unwind(panic),
    };
    vm_oome_hotspot_shape(shared, built)
}

/// gcf f3/o: under `CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE`, a fresh
/// VM-raised `OutOfMemoryError` takes HotSpot's shape
/// ([`VM_OOME_CLEARED_FIELDS`]); off, `built` unchanged.
fn vm_oome_hotspot_shape(
    shared: &SharedVm,
    built: Result<ObjectRef, MethodCallFailed>,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Ok(obj) = &built {
        if preallocated_oome_immutable_enabled() {
            let fields: Vec<(&str, Value)> = VM_OOME_CLEARED_FIELDS
                .iter()
                .map(|name| (*name, Value::Object(None)))
                .collect();
            set_throwable_fields_by_name(shared, *obj, &fields);
        }
    }
    built
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

    // JIT round 20 (lane throw): the retained-trace registry's collection
    // request (`collect_if_requested`), on the door that may collect anyway (a
    // young miss below collects): the interpreter re-executing a deopted
    // guard raises its implicit exception here, not through the trap door.
    if alloc_mode == ExceptionAlloc::MayCollect {
        collect_if_requested(shared, thread);
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
            if super::interpreter::gc_overhead_limit_exceeded(shared) && shared.mem.heap.is_g1() {
                // gcd d10/o, then the gcg g3o residual (2026-10-04): on G1 the
                // forced collection above is a young pause, which cannot reach
                // a dead Old or humongous region, so G1's latched exit runs
                // (`g1_latched_overhead_exit`: the marking cycle, judged, the
                // recovery pause, the soft rung; one attempt after a
                // productive cycle or on the bounded grace) -- the exit every
                // door takes. It used to attempt first and cycle only after a
                // soft rung that collected nothing.
                let before_live = shared.mem.heap.live_bytes_estimate();
                let may_attempt = super::interpreter::gc_and_alloc::g1_latched_overhead_exit(
                    shared,
                    thread,
                    before_live,
                );
                let retried = if may_attempt {
                    shared.mem.heap.try_alloc_object_full(class_id, num_fields)
                } else {
                    None
                };
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
            } else if super::interpreter::gc_overhead_limit_exceeded(shared) {
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
                // (G1 took its own rung above.)
                match shared.mem.heap.try_alloc_object_full(class_id, num_fields) {
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
                retried.ok_or_else(|| {
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

    // An implicit NPE's JEP 358 text is not a detail message on HotSpot: the
    // NPE is built by `NullPointerException()` and the text lives in the
    // transient `extendedMessage` (step 6, [`store_npe_extended_message`]).
    // Interpreter round i4 wave 99, lane L9. JNI `ThrowNew` keeps the
    // caller's message as a detail message whatever it says.
    let (message, extended_text) = if !jni_throw_new
        && is_implicit_npe_text(class_name, message)
        && npe_extended_message_slots(shared, class_id).is_some()
    {
        (None, message)
    } else {
        (message, None)
    };

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
                .or_else(|| pooled_message_hit(shared, msg))
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
        let init_result = invoke_throwable_constructor(
            shared,
            thread,
            class_id,
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
                let _ = invoke_throwable_constructor(
                    shared,
                    thread,
                    class_id,
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
        let init_result = invoke_throwable_constructor(
            shared,
            thread,
            class_id,
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

    // 6. An implicit NPE's JEP 358 text, where HotSpot keeps it once
    //    `getMessage()` computed it (see above). A fresh String, as step 3
    //    makes a message (it may collect: the throwable is re-read from its
    //    pin).
    if let Some(text) = extended_text {
        let fresh = if alloc_mode == ExceptionAlloc::NoCollect {
            try_create_java_string(shared, text)
        } else {
            crate::vm::try_create_java_string_uninterned(shared, text)
                .or_else(|| pooled_message_hit(shared, text))
        };
        let Some(string_ref) = fresh else {
            thread.native_pin_roots.truncate(pin_base);
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::OutOfMemoryError {
                    message: "Java heap space".to_string(),
                },
            )));
        };
        let obj_ref = thread.native_pin_roots[pin_base];
        if !store_npe_extended_message(shared, obj_ref, string_ref) {
            set_detail_message_by_name(shared, obj_ref, string_ref);
        }
    }

    let obj_ref = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);
    Ok(obj_ref)
}

/// Round 15 wave 5 (lane throw1): the kill switches of the throw-path cuts,
/// read once per process into ONE static (`vm/tests/per_vm_state_statics_ratchet.rs`
/// counts each `OnceLock`; these are environment flags, not VM state).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Throw1Switches {
    /// `CRATONVM_THROWABLE_CTOR_MEMO` (default ON): [`invoke_throwable_constructor`].
    pub(crate) ctor_memo: bool,
    /// `CRATONVM_THROWABLE_STANDIN_OPCODE_SCREEN` (default ON):
    /// `stackwalker::compiled_innermost_gets_no_standin_frames`.
    pub(crate) standin_opcode_screen: bool,
    /// Round 15 wave 7 (lane throw2, TH1-5): `CRATONVM_NPE_DEFERRED_MESSAGE`
    /// (default ON): [`create_compiled_implicit_npe`].
    pub(crate) npe_deferred_message: bool,
    /// Round 15 wave 7 (lane throw2, TH1-2): `CRATONVM_CCE_MESSAGE_BY_ID`
    /// (default ON): the compiled `checkcast` notes its operands' class ids
    /// and the funnel consumes a note in both modes ([`cast_message_by_id_enabled`]).
    pub(crate) cce_message_by_id: bool,
    /// Round 15 wave 7 (lane throw2, TH1-2): `CRATONVM_CCE_MESSAGE_MEMO`
    /// (default ON): [`CastMessageMemo`].
    pub(crate) cce_message_memo: bool,
    /// nio21 wave 18 L1: `CRATONVM_JIT_LIVE_TRAP_SNAPSHOT_STANDS_IN` (default
    /// ON): [`create_exception_object_with_trap_frames_live`].
    pub(crate) live_trap_snapshot_stands_in: bool,
    /// nio21 wave 19 L1: `CRATONVM_THROWABLE_DIRECT_IMPLICIT_BUILD` (default
    /// ON): [`try_build_implicit_throwable_directly`].
    pub(crate) direct_implicit_build: bool,
    /// JIT round 23 wave 1, lane L3 (proposal L1W18-1, widened):
    /// `CRATONVM_THROWABLE_DIRECT_IMPLICIT_BUILD_EVERY_DOOR` (default ON, `=0`
    /// off): [`create_exception_object`] builds the six implicit kinds
    /// directly too, and the class list adds `ClassCastException`,
    /// `ArrayStoreException` and `NegativeArraySizeException`. See
    /// [`direct_build_every_door_enabled`].
    pub(crate) direct_build_every_door: bool,
    /// nio21 wave 20 L1: `CRATONVM_THROWABLE_RECAPTURE_IF_MOVED` (default
    /// ON): [`try_build_implicit_throwable_directly`] captures again when its
    /// throwable moved during the capture.
    pub(crate) recapture_if_moved: bool,
    /// nio21 wave 20 L1: `CRATONVM_JIT_DIRECT_THROW_NEW` (default ON):
    /// [`jit_new_throwable_direct`].
    pub(crate) direct_throw_new: bool,
    /// nio21 wave 21 L1: `CRATONVM_JIT_LAMBDA_HANDLER_IMPL` (default ON after the m5 host battery,
    /// `=0` off): [`lambda_handler_impl_enabled`].
    pub(crate) lambda_handler_impl: bool,
    /// JIT round 20 wave 18 (lane xThrow, THROW20-5):
    /// `CRATONVM_JIT_IMPLICIT_THROW_LEAN_BUILD` (default ON, `=0` off):
    /// [`implicit_throw_lean_build_enabled`].
    pub(crate) implicit_throw_lean_build: bool,
    /// JIT round 20 wave 18 (lane xThrow, THROW20-5): the existing
    /// `CRATONVM_JIT_TRAP_SNAPSHOT_BOUNDED` (default ON), latched here for the
    /// lean arm of `jit::helpers::note_trap_snapshot_bound`, which reads it
    /// twice per compiled implicit catch.
    pub(crate) trap_snapshot_bounded: bool,
    /// JIT round 21 (lane npecompat, R15B-39):
    /// `CRATONVM_INTERP_NPE_DIRECT_BUILD` (default ON, `=0` off):
    /// [`throw_interpreted_npe_deferred`] builds the NPE directly.
    pub(crate) interp_npe_direct_build: bool,
}

/// [`Throw1Switches`], cached.
pub(crate) fn throw1_switches() -> Throw1Switches {
    static SWITCHES: OnceLock<Throw1Switches> = OnceLock::new();
    *SWITCHES.get_or_init(|| Throw1Switches {
        ctor_memo: cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_CTOR_MEMO"),
        standin_opcode_screen: cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_THROWABLE_STANDIN_OPCODE_SCREEN",
        ),
        npe_deferred_message: cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_NPE_DEFERRED_MESSAGE",
        ),
        cce_message_by_id: cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_CCE_MESSAGE_BY_ID",
        ),
        cce_message_memo: cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_CCE_MESSAGE_MEMO",
        ),
        live_trap_snapshot_stands_in: cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_LIVE_TRAP_SNAPSHOT_STANDS_IN",
        ),
        direct_implicit_build: cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_THROWABLE_DIRECT_IMPLICIT_BUILD",
        ),
        direct_build_every_door: cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_THROWABLE_DIRECT_IMPLICIT_BUILD_EVERY_DOOR",
        ),
        recapture_if_moved: cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_THROWABLE_RECAPTURE_IF_MOVED",
        ),
        direct_throw_new: cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_DIRECT_THROW_NEW",
        ),
        lambda_handler_impl: cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_LAMBDA_HANDLER_IMPL",
        ),
        implicit_throw_lean_build: cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_IMPLICIT_THROW_LEAN_BUILD",
        ),
        trap_snapshot_bounded: cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_TRAP_SNAPSHOT_BOUNDED",
        ),
        interp_npe_direct_build: cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_INTERP_NPE_DIRECT_BUILD",
        ),
    })
}

/// Run a VM-built throwable's constructor `<init>descriptor` on `args[0]` (a
/// fresh instance of `class_id`) -- [`invoke_on_class_shared`], through the
/// native->Java callback memo (`runtime::native_callee_memo`).
///
/// Round 15 wave 5 (lane throw1, `r15w5-throw1-plain-throw-costs-3x-hotspot-CLOSED-20260930.md`
/// cost item C3): every VM-minted exception -- a compiled `checkcast`'s CCE, a
/// compiled null receiver's NPE, every `RuntimeError` the funnel converts --
/// reached its constructor BY NAME, through `invoke_on_class_shared_inner`'s
/// whole prefix (the pinned argument copy, a class-manager read and a class
/// name `String`, the proxy / SSL / attach / Path arms, the registry probes, the
/// method walk) before the ordinary tail ran `interpreter::execute`. Under
/// `--jdk-only` that tail is where every JDK throwable's `<init>` goes (its
/// retired shadow is a `SyntheticStub` that yields to the real bytecode), and
/// a caught-in-a-loop throw paid the prefix every time. HotSpot's
/// `Exceptions::new_exception` calls a resolved constructor.
///
/// The memo is the one the native context's `invoke_virtual` already uses, and
/// it is not a prediction: `native_callee_memo::arm` marks this dispatch, the
/// tail claims it only when THIS call reached it (same receiver class, the same
/// name and descriptor ADDRESSES, the same native and frame depth), and a later
/// call replays exactly that tail's `interpreter::execute` on the declaring
/// class it reached, for as long as the memo's stamp (native registry
/// generation, class definitions and origins, redefinitions, the VM identity)
/// holds. A constructor a special arm or a registered native answers
/// (`--compatible`'s `native_exc_init_*` shadows, a stub class) never reaches
/// the tail and is never memoised, so it takes the by-name route every time,
/// as before. `<init>` is never `ACC_SYNCHRONIZED` or `ACC_NATIVE`, the tail's
/// two refusals.
///
/// `CRATONVM_THROWABLE_CTOR_MEMO=0` (or `CRATONVM_NATIVE_CALLBACK_MEMO=0`)
/// restores the by-name call every time.
fn invoke_throwable_constructor(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    descriptor: &str,
    args: &[Value],
) -> crate::error::MethodCallResult {
    use crate::runtime::native_callee_memo;
    // One `&str` for the arm, the dispatch and the fill: the tail claims the
    // witness by the name's ADDRESS.
    let method_name: &str = "<init>";
    let eligible = throw1_switches().ctor_memo
        && crate::runtime::env_cache::native_callback_memo()
        && (native_callee_memo::SURVIVES_REDEFINITION
            || !crate::classloading::any_class_redefined())
        // The receiver the tail reports is `args[0]`'s class; a caller that
        // hands anything else keeps the by-name route.
        && matches!(
            args.first(),
            Some(Value::Object(Some(obj))) if shared.mem.heap.class_id_of(*obj) == class_id
        );
    if !eligible {
        return invoke_on_class_shared(shared, thread, class_id, method_name, descriptor, args);
    }
    if let Some(declaring) = native_callee_memo::lookup(shared, class_id, method_name, descriptor) {
        return crate::runtime::interpreter::execute(
            shared,
            thread,
            declaring,
            method_name,
            descriptor,
            args,
        );
    }
    let save = native_callee_memo::arm(class_id, method_name, descriptor, thread.frames.len());
    let result = invoke_on_class_shared(shared, thread, class_id, method_name, descriptor, args);
    if let Some(declaring) = native_callee_memo::disarm(save) {
        native_callee_memo::fill(shared, class_id, method_name, descriptor, declaring);
    }
    result
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
// nio21 wave 16, lane L5: this is now HotSpot's contract, PER VM and DEFAULT
// ON, as `java` runs it. `-XX:-OmitStackTraceInFastThrow` (the launcher and
// `libcratonvm` parse it into `VmConfig::omit_stack_trace_in_fast_throw`)
// turns it off for that VM; absent, `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW`
// decides (default ON, `=0` off). Before wave 16 it was an opt-in process-wide
// switch and `-XX:±OmitStackTraceInFastThrow` was silently dropped by the
// launcher's `-XX:` catch-all. A conformance run that gives HotSpot
// `-XX:-OmitStackTraceInFastThrow` because it prints the message or trace of a
// REPEATED implicit exception must pass the same flag to CratonVM.
//
// What HotSpot 25 does (`GraphKit::builtin_throw`, `is_builtin_throw_hot`,
// `Universe::_null_ptr_exception` & co., `BuiltinException::init_if_empty`):
//
// * the five kinds of [`FastThrowKind`], thrown by C2-compiled code only (the
//   interpreter and C1 always build in full);
// * a site is "hot" once it has trapped there before (`too_many_traps`, or a
//   trap of that reason in a method with a handler) and C2 recompiled it, so
//   in practice after hundreds to tens of thousands of full throws, timing
//   dependent;
// * the exception thrown is ONE preallocated instance per kind per VM,
//   allocated WITHOUT a constructor (every field zero: no `cause = this`, no
//   `SUPPRESSED_SENTINEL`, no backtrace), so `getStackTrace()` is empty,
//   `initCause` throws `IllegalStateException`, `addSuppressed` is dropped,
//   and two throws are `==`;
// * compiled code stores `null` into its `detailMessage` on every throw, so
//   `getMessage()` is null even if someone wrote it.
//
// CratonVM matches all of that ([`preallocated_fast_throw_instance`];
// `CRATONVM_FAST_THROW_SHARED_INSTANCE=0` restores the r9w9 fresh stackless
// object per throw), and models WHEN a site is hot on HotSpot's profile
// (2026-10-10, `n21-omit-stack-trace-in-fast-throw-site-turns-hot-far-later-than-hotspot-FIXED-20261010.md`):
//
// * the interpreter records each implicit exception it raises from its own
//   bytecode at (method, bci, kind) in a per-VM profile
//   ([`FastThrowTrapProfile`]; HotSpot's `InterpreterRuntime::note_trap`);
// * a compiled throw names its site -- the innermost inlined method and bci --
//   from the trap helper's compiled-frame snapshot ([`fast_throw_site_of`]),
//   and whether the body that raised it stands for C2 code
//   ([`body_is_c2_equivalent`]: an optimizing-tier body, or a single-pass one
//   past HotSpot's tier-4 count);
// * it is fast when both hold and the profile already records that site
//   ([`fast_throw_profile_hot`]); otherwise it is full and recorded, as C2's
//   uncommon trap records it;
// * the interpreter re-executing a bytecode a compiled body just trapped on
//   (a deopt sink resumed the frame there) is that body's throw
//   (`cratonvm_jit::deopt::take_trap_site_if`, [`ResumedCompiledFrameScope`]).
//
// The pre-profile rule stays as a fallback (`CRATONVM_FAST_THROW_COUNT_FALLBACK`,
// default ON): a site turns hot after [`FAST_THROW_HOT_THRESHOLD`] throws from
// compiled code, counted per thread on the interpreter frame the compiled code
// was entered from and its pc, plus the kind. It covers a site the profile
// rule cannot name (no snapshot) and a body that keeps throwing at a site it
// never recorded, which HotSpot would have recompiled.
//
// An exception the interpreter raises for its own bytecode is never made
// stackless, exactly as in HotSpot, where the interpreter always fills in the
// trace.

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
    /// This kind's index in `ThrowableLayoutCache::fast_throw_handles`.
    pub fn slot(self) -> usize {
        match self {
            FastThrowKind::NullPointer => 0,
            FastThrowKind::ArrayIndexOutOfBounds => 1,
            FastThrowKind::Arithmetic => 2,
            FastThrowKind::ClassCast => 3,
            FastThrowKind::ArrayStore => 4,
        }
    }

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
    /// trapped compiled activation (helper precise-resume), or 0 for none;
    /// then the bci it resumed at (the trapping bytecode, `u32::MAX` when not
    /// known) and whether the body that trapped stands for C2 code
    /// (`body_is_c2_equivalent`).
    static RESUMED_COMPILED_FRAME_DEPTH: std::cell::Cell<(usize, u32, bool)> =
        const { std::cell::Cell::new((0, u32::MAX, false)) };
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
    prev: (usize, u32, bool),
}

impl ResumedCompiledFrameScope {
    /// A scope that knows neither the trapping bci nor the trapped body: its
    /// throws are compiled throws whose site is never C2 code, so only the
    /// count fallback can make them fast.
    pub fn enter(depth: usize) -> Self {
        Self::enter_at(depth, u32::MAX, false)
    }

    /// A scope for a frame resumed at `bci`, the bytecode its body trapped
    /// on. The exception that bytecode raises is the one HotSpot's C2 code
    /// would have thrown in place (a fast throw where the site is hot, so no
    /// trap at all) when `c2_tier` -- the trapped body stands for C2 code.
    pub fn enter_at(depth: usize, bci: u32, c2_tier: bool) -> Self {
        let prev = RESUMED_COMPILED_FRAME_DEPTH.with(|d| d.replace((depth, bci, c2_tier)));
        ResumedCompiledFrameScope { prev }
    }
}

impl Drop for ResumedCompiledFrameScope {
    fn drop(&mut self) {
        RESUMED_COMPILED_FRAME_DEPTH.with(|d| d.set(self.prev));
    }
}

/// t1 wave 5 lane E2 (stage 2 of
/// `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`):
/// replace this OS thread's resumed-compiled-frame mark, answering the old one.
/// A continuation slice run on its carrier's OS thread starts from "none"
/// (`(0, u32::MAX, false)`): the carrier's mark names a frame count of the
/// CARRIER's stack, which the continuation's own count could equal, and then
/// an implicit exception of the slice would count as a compiled throw.
pub(crate) fn replace_resumed_compiled_frame_depth(v: (usize, u32, bool)) -> (usize, u32, bool) {
    RESUMED_COMPILED_FRAME_DEPTH.with(|d| d.replace(v))
}

fn in_resumed_compiled_frame(thread: &JvmThread) -> bool {
    let (depth, _, _) = RESUMED_COMPILED_FRAME_DEPTH.with(|d| d.get());
    depth != 0 && thread.frames.len() == depth
}

/// Is the resumed frame's current bytecode the one its C2-equivalent body
/// trapped on? Only that bytecode stands for the compiled code; the rest of
/// the resumed activation runs in the interpreter on HotSpot too.
fn resumed_compiled_frame_at_c2_trap(thread: &JvmThread) -> bool {
    let (depth, bci, c2_tier) = RESUMED_COMPILED_FRAME_DEPTH.with(|d| d.get());
    c2_tier
        && depth != 0
        && thread.frames.len() == depth
        && thread
            .frames
            .last()
            .is_some_and(|f| u32::try_from(f.last_instr_pc).is_ok_and(|pc| pc == bci))
}

/// Throws at one site (per thread, per kind) that are still built in full.
/// The next one and every later one are stackless. HotSpot's equivalent is
/// C2 compiling the method, the site trapping once, and C2 recompiling it as a
/// fast throw -- hundreds to tens of thousands of throws, timing dependent
/// (observed at 1 375 on `RArrayStoreTiers`, never within `RExceptions`'
/// 1 200). It was 16 while the door was opt-in (r9w9); nio21 wave 16 L5 made
/// the door default ON and raised it to 10 000, above every count a
/// deterministic HotSpot run is known to keep full.
pub const FAST_THROW_HOT_THRESHOLD: u32 = 10_000;

/// The site of one implicit exception as the profile rule names it: the
/// method whose bytecode raised it (`"class/Name.method:descriptor"`, the
/// shape of `CompiledMethod::method_label` and of an inlined level's label),
/// the bci in that method, and whether the body that raised it stands for
/// C2 code ([`body_is_c2_equivalent`]; `false` for the interpreter and for a
/// single-pass body still below the tier-4 count -- HotSpot's interpreter and
/// C1 never fast-throw).
///
/// For a throw from inside a callee the body INLINED, the site is the
/// innermost inlined level's method and bci, and the body is the outer
/// artifact: C2 asks `too_many_traps` of the inlined method's profile at the
/// inlined bci while parsing it.
#[derive(Clone, Debug)]
pub struct FastThrowSite {
    pub label: Arc<str>,
    pub bci: u32,
    pub c2_tier: bool,
}

/// The profile half of HotSpot's per-bci trap bits: the (method, bci, kind)
/// triples at which an implicit exception has been raised. HotSpot's
/// interpreter records one there (`InterpreterRuntime::note_trap` from
/// `create_exception` / `throw_ArrayIndexOutOfBoundsException` /
/// `throw_ClassCastException` / `create_klass_exception`), and so does a C2
/// uncommon trap at a site that was not yet hot. Per VM
/// (`ThrowableLayoutCache::fast_throw_traps`); written only while fast throw
/// is on for the VM.
///
/// Keyed by the method's name, not its code address, because the interpreter
/// and every body that inlines the method must agree on it, and the label is
/// the one name both have. Two loaders defining a same-named class share rows,
/// which can only make a site hot early, never wrong.
#[derive(Default)]
pub struct FastThrowTrapProfile {
    sites: parking_lot::Mutex<rustc_hash::FxHashSet<(Arc<str>, u32, FastThrowKind)>>,
    /// JIT round 22 wave 2 (lane throws, THROWS21-5;
    /// `CRATONVM_FAST_THROW_PROFILE_BY_LABEL`, perf, opt-in): the same triples,
    /// keyed by the label with a short `(bci, kind)` list as the value, behind
    /// a reader-writer lock, plus the number of rows (the cap's count). A
    /// probe is a shared read lock and a lookup by `&str` -- no `Arc` clone (a
    /// shared refcount RMW) and no exclusive lock per throw -- and a `note` of
    /// a site already recorded takes no write lock. Used instead of `sites`
    /// when [`Self::keyed_by_label`] latches on; the two are never mixed.
    by_label: parking_lot::RwLock<(rustc_hash::FxHashMap<Arc<str>, Vec<(u32, FastThrowKind)>>, usize)>,
    /// The switch, latched per VM at the first use (never per throw).
    by_label_on: std::sync::OnceLock<bool>,
}

/// Bound on [`FastThrowTrapProfile`]. When it fills it is cleared: a site then
/// has to throw again before it can be hot -- slower, never a wrong answer.
const FAST_THROW_TRAP_PROFILE_CAP: usize = 1 << 16;

impl FastThrowTrapProfile {
    /// `CRATONVM_FAST_THROW_PROFILE_BY_LABEL` (JIT round 22 wave 2, lane
    /// throws; perf, opt-in, default OFF), latched once per VM.
    fn keyed_by_label(&self) -> bool {
        *self.by_label_on.get_or_init(|| {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_FAST_THROW_PROFILE_BY_LABEL")
        })
    }

    /// Record an implicit exception of `kind` at `label`'s `bci`.
    pub fn note(&self, label: &Arc<str>, bci: u32, kind: FastThrowKind) {
        if self.keyed_by_label() {
            if self.has_trap_at(label, bci, kind) {
                return;
            }
            let mut guard = self.by_label.write();
            let (map, rows) = &mut *guard;
            if let Some(list) = map.get(&**label) {
                if list.contains(&(bci, kind)) {
                    return; // a racing writer recorded it first
                }
            }
            if *rows >= FAST_THROW_TRAP_PROFILE_CAP {
                map.clear();
                *rows = 0;
            }
            match map.get_mut(&**label) {
                Some(list) => list.push((bci, kind)),
                None => {
                    map.insert(Arc::clone(label), vec![(bci, kind)]);
                }
            }
            *rows += 1;
            return;
        }
        let mut sites = self.sites.lock();
        let key = (Arc::clone(label), bci, kind);
        if sites.len() >= FAST_THROW_TRAP_PROFILE_CAP && !sites.contains(&key) {
            sites.clear();
        }
        sites.insert(key);
    }

    /// Has an implicit exception of `kind` been recorded at `label`'s `bci`?
    pub fn has_trap_at(&self, label: &Arc<str>, bci: u32, kind: FastThrowKind) -> bool {
        if self.keyed_by_label() {
            return self
                .by_label
                .read()
                .0
                .get(&**label)
                .is_some_and(|list| list.contains(&(bci, kind)));
        }
        self.sites.lock().contains(&(Arc::clone(label), bci, kind))
    }
}

/// HotSpot's tier-4 invocation threshold (`Tier4InvocationThreshold`): a
/// single-pass body entered this many times -- back edges weighed one eighth
/// of an entry, `Tier4BackEdgeThreshold / Tier4InvocationThreshold`, as the
/// tiering drain weighs them -- stands for a method HotSpot would have
/// compiled with C2 by now. Measured on HotSpot 25.0.3 (`Nio21FastThrow
/// -Dnio21.n=<n>`): every row is still full at 2 000 throws of one site,
/// the first rows go stackless between 3 000 and 5 000, and all of them by
/// 8 000. This VM promotes a single-pass body to the optimizing tier only at
/// 20 000, and some methods (a catch in the same method, a large body) never.
pub const FAST_THROW_C2_EQUIVALENT_ENTRIES: u64 = 5_000;

/// Does `cm` stand for the code HotSpot's C2 would be running? An
/// optimizing-tier body always; a single-pass body once its entry counter
/// (`CompiledMethod::entry_count`, back edges weighed as above) has reached
/// [`FAST_THROW_C2_EQUIVALENT_ENTRIES`]. HotSpot's C1 never fast-throws, and a
/// single-pass body below that count is what C1 would be running.
pub fn body_is_c2_equivalent(cm: &cratonvm_jit::CompiledMethod) -> bool {
    cm.used_ir_backend
        || cm.entry_count().saturating_add(cm.entry_back_edges() >> 3)
            >= FAST_THROW_C2_EQUIVALENT_ENTRIES
}

/// Is `site` hot by HotSpot's rule? It was raised in a body that stands for C2
/// code, and the method profile already records an implicit exception of
/// `kind` at that method and bci (`GraphKit::builtin_throw` ->
/// `is_builtin_throw_hot` -> `too_many_traps`).
///
/// HotSpot reads the profile when C2 parses the method, so a trap recorded
/// AFTER the compile makes the site hot only through a recompile: the C2 code
/// takes an uncommon trap there, which records it, and after a few such traps
/// (`PerBytecodeTrapLimit`) the method is recompiled with the site hot. This
/// VM does not recompile a body because a site in it threw, so the rule reads
/// the profile at the throw instead: the first throw a C2-equivalent body
/// makes at a site the profile has not seen is full (and recorded), every
/// later one is fast. The site of `StackTraceAfterOsr` / `Nio21FastThrow`
/// (thrown first by the interpreter, compiled afterwards) is fast at its first
/// compiled throw on both VMs.
fn fast_throw_profile_hot(shared: &SharedVm, site: &FastThrowSite, kind: FastThrowKind) -> bool {
    site.c2_tier
        && shared
            .threads
            .throwable_layout_cache
            .fast_throw_traps
            .has_trap_at(&site.label, site.bci, kind)
}

/// The site of a compiled implicit exception, from the compiled frames the
/// trap helper snapshotted (outermost first, so the frame that raised is the
/// last). `None` when there is no snapshot, or the frame that raised has no
/// usable bci: the rule then cannot name the site and the count fallback
/// decides.
pub fn fast_throw_site_of(
    frames: Option<&[crate::jit::conservative_roots::ActiveCompiledFrame]>,
) -> Option<FastThrowSite> {
    let top = frames?.last()?;
    let (label, bci) = match top.chain().first() {
        Some(level) => (Arc::clone(&level.label), level.bci),
        None => (Arc::clone(&top.label), u32::try_from(top.bci).ok()?),
    };
    Some(FastThrowSite {
        label,
        bci,
        c2_tier: top.fast_throw_c2_tier,
    })
}

/// [`fast_throw_site_of`] for a helper called FROM compiled code (a cast or
/// an array-store check), whose frames are all still live: walks them. Only
/// while fast throw is on for this VM -- a walk per throw is the price of the
/// profile rule on a site that has no snapshot.
pub fn fast_throw_site_of_live_frames(shared: &SharedVm) -> Option<FastThrowSite> {
    if !omit_stack_trace_in_fast_throw(shared) {
        return None;
    }
    let frames = crate::jit::conservative_roots::active_compiled_frames();
    fast_throw_site_of(Some(&frames))
}

/// Does the interpreter's opcode at `bci` raise `kind` as an IMPLICIT
/// exception? Screens out an exception a native raised while the caller's
/// frame stands at an unrelated bytecode, which HotSpot's interpreter does
/// not record as a trap.
fn opcode_raises_implicitly(opcode: u8, kind: FastThrowKind) -> bool {
    let array_access = matches!(opcode, 0x2e..=0x35 | 0x4f..=0x56);
    match kind {
        // xaload/xastore, getfield/putfield, invokevirtual/special/interface,
        // arraylength, athrow, monitorenter/monitorexit.
        FastThrowKind::NullPointer => {
            array_access
                || matches!(
                    opcode,
                    0xb4 | 0xb5 | 0xb6 | 0xb7 | 0xb9 | 0xbe | 0xbf | 0xc2 | 0xc3
                )
        }
        FastThrowKind::ArrayIndexOutOfBounds => array_access,
        // idiv, ldiv, irem, lrem.
        FastThrowKind::Arithmetic => matches!(opcode, 0x6c | 0x6d | 0x70 | 0x71),
        FastThrowKind::ClassCast => opcode == 0xc0,
        FastThrowKind::ArrayStore => opcode == 0x53,
    }
}

/// The site of an implicit exception the INTERPRETER raises from its top
/// frame: that frame's method at the bytecode it is executing.
fn interpreter_fast_throw_site(
    shared: &SharedVm,
    thread: &JvmThread,
    kind: FastThrowKind,
) -> Option<FastThrowSite> {
    let frame = thread.frames.last()?;
    let bci = frame.last_instr_pc;
    if !frame
        .code
        .get(bci)
        .is_some_and(|&op| opcode_raises_implicitly(op, kind))
    {
        return None;
    }
    let class_name = shared
        .classes
        .class_manager
        .read()
        .get_class(frame.class_id)
        .map(|c| c.name.to_string())?;
    let label: Arc<str> = Arc::from(format!(
        "{}.{}:{}",
        class_name,
        frame.method_name(),
        frame.method_descriptor()
    ));
    Some(FastThrowSite {
        label,
        bci: u32::try_from(bci).ok()?,
        c2_tier: false,
    })
}

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

/// The [`ImplicitThrowTls::classes`] rows: one per class
/// [`implicit_direct_class_slot_as`] names.
const IMPLICIT_DIRECT_CLASS_SLOTS: usize = 6;

/// The per-thread state of this module's implicit-exception doors: the fast
/// throw's site counts and (nio21 wave 19 L1) the direct-build verdicts
/// ([`try_build_implicit_throwable_directly`]). One thread-local for both
/// (`vm/tests/per_vm_state_statics_ratchet.rs` counts each declaration).
#[derive(Default)]
struct ImplicitThrowTls {
    sites: rustc_hash::FxHashMap<FastThrowSiteKey, u32>,
    /// `(vm_identity, class id, with a message)` -> the verdict, valid while
    /// `direct_stamp` is the class-redefinition count it was computed under.
    // addr-keyed: not a heap address -- the `usize` is `vm_identity()`, a per-VM id that no collection moves.
    direct: rustc_hash::FxHashMap<(usize, u32, bool), Option<DirectBuildPlan>>,
    direct_stamp: u64,
    /// JIT round 20 wave 18 (lane xThrow, THROW20-5): per implicit kind
    /// ([`implicit_direct_class_slot`]), `(vm_identity, class id, instance
    /// field count)` of the bootstrap class [`try_build_implicit_throwable_directly`]
    /// found initialized and linked, valid while `classes_stamp` is the
    /// class-redefinition count it was found under.
    classes: [Option<(usize, u32, usize)>; IMPLICIT_DIRECT_CLASS_SLOTS],
    classes_stamp: u64,
}

thread_local! {
    static FAST_THROW_SITES: std::cell::RefCell<ImplicitThrowTls> =
        std::cell::RefCell::new(ImplicitThrowTls::default());
}

/// Is `OmitStackTraceInFastThrow` in effect for THIS VM? `-XX:±` on the
/// command line, else `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW` (opt-in,
/// `=1` on: the owner's decision, see `docs/JIT_OPTIMIZATION.md`); latched once per VM (nio21 wave 16, lane L5 -- it was a
/// process-wide `OnceLock` over the switch alone, default OFF).
pub fn omit_stack_trace_in_fast_throw(shared: &SharedVm) -> bool {
    shared
        .threads
        .throwable_layout_cache
        .fast_throw_enabled(|| shared.config.omit_stack_trace_in_fast_throw_resolved())
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

/// The per-thread site key [`try_fast_throw`] counts under.
fn fast_throw_site_key(
    shared: &SharedVm,
    thread: &JvmThread,
    kind: FastThrowKind,
) -> Option<FastThrowSiteKey> {
    let frame = thread.frames.last()?;
    Some((
        shared.vm_identity,
        frame.code.as_ptr() as usize,
        frame.pc,
        kind,
    ))
}

/// Will the NEXT [`try_fast_throw`] of `kind` from this thread's current site
/// answer the preallocated instance WHATEVER its site turns out to be? A peek:
/// counts nothing. `true` only when the count fallback is on and the site is
/// already past [`FAST_THROW_HOT_THRESHOLD`] AND this VM's instance of `kind`
/// is already built, so the answer cannot be a "build it in full after all"
/// (a failed first allocation). nio21 wave 16, lane L5: for a trap helper
/// that would otherwise snapshot the compiled frames (`walk_jit_chain`) for a
/// throwable that will never carry them. The profile rule needs that snapshot
/// to name the site, so a `false` here is not "full": it is "ask with the
/// site".
pub fn fast_throw_will_fire(shared: &SharedVm, thread: &JvmThread, kind: FastThrowKind) -> bool {
    use std::sync::atomic::Ordering;
    let cache = &shared.threads.throwable_layout_cache;
    if !omit_stack_trace_in_fast_throw(shared)
        || !cache.fast_throw_count_fallback_enabled()
        || !cache.fast_throw_shared_instance_enabled()
        || cache.fast_throw_handles[kind.slot()].load(Ordering::Acquire) == 0
        || super::interpreter::exception_events_may_be_armed(shared)
    {
        return false;
    }
    let Some(key) = fast_throw_site_key(shared, thread, kind) else {
        return false;
    };
    FAST_THROW_SITES.with(|sites| {
        sites
            .try_borrow()
            .ok()
            .and_then(|tls| tls.sites.get(&key).copied())
            .is_some_and(|n| n >= FAST_THROW_HOT_THRESHOLD)
    })
}

/// An implicit exception the INTERPRETER raises from its own bytecode, while
/// fast throw is on for this VM ([`throw_runtime_error`]). Two cases:
///
/// * It re-executes the bytecode a compiled body just trapped on (a deopt sink
///   resumed the frame there: `cratonvm_jit::deopt::take_trap_site_if`). That
///   is the compiled body's throw, which HotSpot's C2 code would have thrown
///   in place -- fast, where the site is hot -- so it goes through
///   [`try_fast_throw`] with the trapping body's tier.
/// * Otherwise it is an interpreted throw: never fast, and recorded in the
///   method profile for the hotness rule, as HotSpot's interpreter records a
///   trap (`InterpreterRuntime::note_trap`).
fn interpreter_implicit_exception(
    shared: &SharedVm,
    thread: &JvmThread,
    kind: FastThrowKind,
) -> Option<ObjectRef> {
    let mut site = interpreter_fast_throw_site(shared, thread, kind)?;
    if let Some(body) = cratonvm_jit::deopt::take_trap_site_if(&site.label, site.bci) {
        site.c2_tier = body.is_some_and(|body| body_is_c2_equivalent(&body));
        return try_fast_throw(shared, thread, kind, Some(&site));
    }
    shared
        .threads
        .throwable_layout_cache
        .fast_throw_traps
        .note(&site.label, site.bci, kind);
    None
}

/// The stackless throwable for an implicit exception raised by compiled code,
/// when `OmitStackTraceInFastThrow` is on and this site is hot; `None` means
/// "build it in full, as always".
///
/// Hot is HotSpot's rule ([`fast_throw_profile_hot`]): `site` (see
/// [`fast_throw_site_of`]) was raised in a body that stands for C2 code and the
/// profile already records an implicit exception of `kind` there. A site that
/// is not hot by that rule is recorded now, as C2's uncommon trap records it.
/// Then, unless `CRATONVM_FAST_THROW_COUNT_FALLBACK=0`, the pre-profile rule:
/// hot after [`FAST_THROW_HOT_THRESHOLD`] throws counted per thread at the
/// interpreter frame the compiled code was entered from (`site == None` is
/// decided by it alone).
///
/// Call it at a drain that is about to construct one of the [`FastThrowKind`]
/// exceptions on behalf of compiled code, BEFORE building the message (the
/// JEP 358 NPE message is itself a bytecode analysis). Allocates on the
/// fast path (one object, no Java code, no stack walk), so it is a GC point
/// exactly where the full construction it replaces was one (on the first
/// hot throw of a kind only, with the shared instance).
pub fn try_fast_throw(
    shared: &SharedVm,
    thread: &JvmThread,
    kind: FastThrowKind,
    site: Option<&FastThrowSite>,
) -> Option<ObjectRef> {
    if !omit_stack_trace_in_fast_throw(shared) {
        return None;
    }
    // HotSpot's `builtin_throw` takes an uncommon trap instead of the
    // preallocated throw while JVMTI may post exception events
    // (`uncommon_trap_if_should_post_on_exceptions`): an `Exception` client
    // sees full exceptions. Same here, per VM.
    if super::interpreter::exception_events_may_be_armed(shared) {
        return None;
    }
    let cache = &shared.threads.throwable_layout_cache;
    let profile_hot = site.is_some_and(|site| fast_throw_profile_hot(shared, site, kind));
    if !profile_hot {
        if let Some(site) = site {
            cache.fast_throw_traps.note(&site.label, site.bci, kind);
        }
    }
    // The count runs for a profile-hot site too: past the threshold
    // [`fast_throw_will_fire`] lets the trap helper skip the snapshot the
    // profile rule would otherwise need on every throw.
    let count_hot = cache.fast_throw_count_fallback_enabled()
        && fast_throw_site_key(shared, thread, kind).is_some_and(|key| {
            FAST_THROW_SITES.with(|sites| match sites.try_borrow_mut() {
                Ok(mut tls) => fast_throw_note_site(
                    &mut tls.sites,
                    key,
                    FAST_THROW_HOT_THRESHOLD,
                    FAST_THROW_SITE_CAP,
                ),
                // Re-entered from inside a count (cannot happen: nothing above
                // calls out) -- answer "not hot", which is the full,
                // always-correct build.
                Err(_) => false,
            })
        });
    if !profile_hot && !count_hot {
        return None;
    }
    if cache.fast_throw_shared_instance_enabled() {
        return preallocated_fast_throw_instance(shared, kind);
    }
    create_stackless_exception_object(shared, kind.class_name())
}

/// This VM's ONE preallocated instance of `kind`, as HotSpot's compiled code
/// throws it (`Universe::null_ptr_exception_instance()` & co.), with its
/// `detailMessage` cleared as `GraphKit::builtin_throw` clears it on every
/// throw. nio21 wave 16, lane L5.
///
/// Built on first use: allocated WITHOUT a constructor and with every field at
/// its default (`BuiltinException::init_if_empty` is a bare
/// `allocate_instance`), so `cause` and `suppressedExceptions` are `null` --
/// `initCause` refuses, `addSuppressed` drops, `getCause()` is null,
/// `getStackTrace()` is empty -- exactly HotSpot's answers. Held by a JNI
/// global ref, whose table every collector roots and remaps; the handle is
/// per VM (`ThrowableLayoutCache::fast_throw_handles`). Two threads racing
/// the first build keep the first published handle and release the other.
///
/// `None` (the caller builds in full) when the class cannot be instantiated
/// without running Java or the young generation is full, as for
/// [`create_stackless_exception_object`].
fn preallocated_fast_throw_instance(shared: &SharedVm, kind: FastThrowKind) -> Option<ObjectRef> {
    use std::sync::atomic::Ordering;
    let slot = &shared.threads.throwable_layout_cache.fast_throw_handles[kind.slot()];
    let mut handle = slot.load(Ordering::Acquire);
    if handle == 0 {
        let (class_id, num_fields) = stackless_exception_class(shared, kind.class_name())?;
        let obj = shared.mem.heap.try_alloc_object(class_id, num_fields)?;
        // Every field at its declared default (the tag words a zero fill does
        // not give); no constructor, no `Throwable` initialisers. No
        // allocation and no Java between the allocation and the root.
        let _ = super::interpreter::init_primitive_fields(shared, obj, class_id);
        let mine = shared.natives.jni_global_refs.lock().add(obj);
        handle = match slot.compare_exchange(0, mine, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => mine,
            Err(theirs) => {
                let _ = shared.natives.jni_global_refs.lock().remove(mine);
                theirs
            }
        };
    }
    let cache = &shared.threads.throwable_layout_cache;
    if cache.fast_throw_unlocked_handout_enabled() {
        return preallocated_fast_throw_handout_unlocked(shared, handle);
    }
    let exc = shared.natives.jni_global_refs.lock().resolve(handle)?;
    if let Some(idx) = throwable_field_index_or_walk(shared, exc, "detailMessage") {
        shared.mem.heap.set_field(exc, idx, Value::Object(None));
    }
    Some(exc)
}

/// L5W17-5 (`docs/feature-designs/jit-ir-optimizations.md`; JIT
/// round 20 wave 11, lane pNat; `CRATONVM_FAST_THROW_UNLOCKED_HANDOUT`, default
/// ON, `=0` restores the locked tail of [`preallocated_fast_throw_instance`]):
/// the same hand-out without the JNI global-ref table's mutex on every hot
/// throw.
///
/// `handle` is a published `fast_throw_handles` slot: a STRONG handle `add`
/// returned, never removed for the life of the VM (only a race LOSER's handle
/// is removed, and it is never published), on this VM's table. This thread is
/// a mutator about to throw, outside any collection pause, so the slot holds
/// what the locked `resolve` would read (`JniGlobalRefs::resolve_strong_unlocked`).
/// `detailMessage`'s slot is the per-VM `ThrowableLayoutCache::detail_message`
/// cell `throwable_own_field_index` resolves and keeps; read directly once
/// resolved, else through the same lookup.
fn preallocated_fast_throw_handout_unlocked(shared: &SharedVm, handle: u64) -> Option<ObjectRef> {
    use crate::vm::realms::thread_realm::UNRESOLVED_THROWABLE_FIELD_INDEX;
    use std::sync::atomic::Ordering;
    // Safety: see the doc above -- a published, never-removed strong handle of
    // this VM's live table, read by a mutator outside a pause.
    let exc = unsafe { crate::native::jni::JniGlobalRefs::resolve_strong_unlocked(handle) }?;
    let cache = &shared.threads.throwable_layout_cache;
    let idx = match cache.detail_message.load(Ordering::Relaxed) {
        UNRESOLVED_THROWABLE_FIELD_INDEX => {
            throwable_field_index_or_walk(shared, exc, "detailMessage")
        }
        idx => Some(idx),
    };
    if let Some(idx) = idx {
        shared.mem.heap.set_field(exc, idx, Value::Object(None));
    }
    cache.note_fast_throw_unlocked_handout();
    Some(exc)
}

/// [`try_fast_throw`] for a drain that builds its exception through
/// [`throw_runtime_error`]. `error` is only evaluated on the full path, so an
/// expensive message is not built for a throwable that will not carry it.
pub fn throw_implicit_runtime_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    kind: FastThrowKind,
    site: Option<&FastThrowSite>,
    error: impl FnOnce() -> RuntimeError,
) -> MethodCallFailed {
    if let Some(exc) = try_fast_throw(shared, thread, kind, site) {
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
    site: Option<&FastThrowSite>,
    message: impl FnOnce() -> String,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(exc) = try_fast_throw(shared, thread, kind, site) {
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
    let (class_id, num_fields) = stackless_exception_class(shared, class_name)?;
    let obj = shared.mem.heap.try_alloc_object(class_id, num_fields)?;
    // No allocation and no Java between the allocation and the return, so the
    // bare `obj` cannot go stale here. No constructor runs, so both of
    // `Throwable`'s field initialisers are this door's to write.
    seed_throwable_cause(shared, obj);
    mirror_suppressed_initialiser(shared, obj);
    Some(obj)
}

/// The class and slot count a constructor-less fast-throw instance of
/// `class_name` is allocated with, or `None` when there is none to build
/// without Java (see [`create_stackless_exception_object`]).
fn stackless_exception_class(shared: &SharedVm, class_name: &str) -> Option<(ClassId, usize)> {
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
        let initialized = crate::vm::is_class_initialized_fast(class);
        if !initialized
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
        // Interpreter round i5 wave 106, lane L1 (i102 residue): an instance of
        // a class not initialized, maybe never linked, is about to exist; the
        // JDWP listing must keep its chain. One relaxed load once recorded,
        // none for an initialized class.
        #[cfg(feature = "experimental-debug")]
        {
            if !initialized {
                crate::debug::note_chain_used_unlinked(shared, &cm, class_id);
            }
        }
        (class_id, class.num_total_fields)
    };
    Some((class_id, num_fields))
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
    // `-XX:-StackTraceInThrowable`: the capture stored an empty trace and
    // nothing is spliced onto it (interpreter round i4 wave 85, lane L2).
    if !shared.config.stack_trace_in_throwable {
        return;
    }
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
        shared.config.hides_hidden_frames(),
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
    create_exception_object_with_trap_frames_live(
        shared, thread, class_name, message, snapshot, false,
    )
}

/// [`create_exception_object_with_trap_frames`], told whether `snapshot` is
/// LIVE: taken on this thread with every frame it names still on the machine
/// stack, and no bytecode run and no interpreter frame pushed or popped since
/// (`thread.frames` is the stack the walk interleaved it against).
///
/// nio21 wave 18 L1
/// (`r21-implicit-local-handlers-without-precise-frames-mistrace-division-FIXED-20261009.md`):
/// the one door that builds with a live snapshot is the compiled local
/// handler's implicit arm (`jit::helpers::local_handler_enter_implicit`), which
/// constructs the throwable from inside the trap pad of the frame that will
/// catch it. When an INTERPRETED frame sat between two compiled ones at the
/// trap -- `hot` running in an OSR body, `call` back in the interpreter after a
/// deopt, the compiled `div0` it called trapping -- the OSR continuation is
/// recorded below the innermost interpreter frame, so
/// [`crate::runtime::stackwalker::trap_snapshot_can_stand_in_for_walk`]
/// refused the one-walk route and the snapshot was SPLICED onto a capture that
/// had walked the same frames live. The live walk reads the trapping frame from
/// its safepoint-id slot, which a zero-divisor guard or a bounds check never
/// publishes (and the probe has just cleared): `-1`, or its last call's line.
/// The snapshot names it at the stub's stamped bci. The overlap merge compares
/// bcis, so the frame printed twice (`div0:99`, `div0:-1`, `call:...`): the
/// ~1% `badtrace` of `Nio21ImplicitCatch`'s hot division rows, in the runs
/// where `div0` compiled before `call` recompiled.
///
/// A live snapshot IS the compiled half of the stack the capture would walk,
/// at every depth, with the trapping frame's bci better than the walk's: the
/// two concerns the stand-in test guards (a frame under an interpreter frame
/// rebuilt since the trap; a frame recorded deeper than the stack is now)
/// cannot arise without code running between the trap and the construction.
/// So it stands in whenever every frame sits at or below the innermost
/// interpreter frame, and the splice -- which cannot repair a still-live frame
/// read at another bci (`stackwalker::trailing_overlap`) -- serves only an arm
/// that fails (a door nested in another door's construction).
/// `CRATONVM_JIT_LIVE_TRAP_SNAPSHOT_STANDS_IN=0` restores the stand-in test
/// alone.
#[allow(clippy::type_complexity)]
pub(crate) fn create_exception_object_with_trap_frames_live(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    message: Option<&str>,
    snapshot: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    live: bool,
) -> Result<
    ObjectRef,
    (
        MethodCallFailed,
        Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    ),
> {
    // JIT round 20 (lane throw): a collection the retained-trace registry
    // asked for (`SharedVm::store_throwable_backtrace_shared_at`). This door
    // allocates and may run Java (the constructors), so it is a GC point
    // already; a compiled loop that only throws implicit exceptions reaches no
    // other `maybe_gc`, and its dead throwables' traces (off the Java heap)
    // would otherwise grow without bound.
    collect_if_requested(shared, thread);
    // JIT round 20 (lane throw): the next trap snapshot on this thread walks
    // with this VM's bound (`jit::helpers::trap_snapshot_walk`).
    crate::jit::helpers::note_trap_snapshot_bound(shared);
    let mut snapshot = snapshot;
    let mut armed = None;
    if let Some(frames) = snapshot.take() {
        let live_stands_in = live
            && live_trap_snapshot_stands_in_enabled()
            && live_trap_snapshot_fits_stack(thread.frames.len(), &frames);
        if live_stands_in
            || (trap_capture_one_walk_enabled()
                && crate::runtime::stackwalker::trap_snapshot_can_stand_in_for_walk(
                    &thread.frames,
                    &frames,
                ))
        {
            match crate::runtime::stackwalker::arm_trap_capture(frames, thread.frames.len()) {
                Ok(window) => armed = Some(window),
                Err(frames) => snapshot = Some(frames),
            }
        } else {
            snapshot = Some(frames);
        }
    }
    // nio21 wave 19 L1: the JDK's implicit exceptions without interpreting
    // their constructor chains, when the class bytes are the JDK's (the capture
    // inside takes the armed snapshot exactly as `fillInStackTrace(0)` would).
    let created = match try_build_implicit_throwable_directly(shared, thread, class_name, message) {
        Some(built) => built,
        None => {
            // JIT round 20 (lane throw,
            // `r19-a-capture-nested-in-an-implicit-exception-construction-takes-the-trap-snapshot-FIXED-20261012.md`):
            // the constructors run Java, so a throwable of another class may
            // be built inside the window; name the owner so only this
            // throwable's captures take the snapshot (the direct build above
            // runs no Java and captures once, at the trap's depth).
            if armed.is_some() {
                let owner = shared
                    .classes
                    .class_manager
                    .read()
                    .resolve_fast_path_class_id(class_name);
                if let Some(owner) = owner {
                    crate::runtime::stackwalker::bind_trap_capture_owner(owner);
                }
            }
            // The direct build was asked above and refused: the constructors,
            // without asking it a second time.
            create_exception_object_constructed(shared, thread, class_name, message)
        }
    };
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

/// JIT round 20 (lane throw): run the collection a `gc_requested` latch asks
/// for (the retained-trace registry's budget, `jcmd GC.run`), through
/// [`super::interpreter::maybe_gc`], which consumes it. One relaxed load when
/// nothing is latched. Only at a door that may collect.
fn collect_if_requested(shared: &SharedVm, thread: &mut JvmThread) {
    if shared
        .mem
        .gc_requested
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        super::interpreter::maybe_gc(shared, thread);
    }
}

/// Can a LIVE trap snapshot (see [`create_exception_object_with_trap_frames_live`])
/// stand in for the compiled walk of a capture made on top of `depth`
/// interpreter frames? Non-empty, and no frame recorded above the innermost
/// interpreter frame's own top (`interp_depth <= depth`): every frame is then
/// one the live walk would find at the same place. Pure, for the unit tests.
pub(crate) fn live_trap_snapshot_fits_stack(
    depth: usize,
    snapshot: &[crate::jit::conservative_roots::ActiveCompiledFrame],
) -> bool {
    !snapshot.is_empty()
        && snapshot
            .iter()
            .all(|f| usize::try_from(f.interp_depth).is_ok_and(|d| d <= depth))
}

/// `CRATONVM_JIT_LIVE_TRAP_SNAPSHOT_STANDS_IN` -- default ON (nio21 wave 18
/// L1); `0` sends a live snapshot through the ordinary stand-in test, i.e.
/// back to the splice that printed the catching frame twice. Read per compiled
/// implicit catch, cached in [`Throw1Switches`] (no static of its own:
/// `vm/tests/per_vm_state_statics_ratchet.rs`).
/// `CRATONVM_THROWABLE_RECAPTURE_IF_MOVED` (default ON; nio21 wave 20 L1,
/// widened in wave 21 to every capture): `NativeContextImpl::capture_throwable_stack_trace`
/// captures again when its throwable moved during the capture.
/// `CRATONVM_JIT_LAMBDA_HANDLER_IMPL` -- default ON (`=0` off; flipped after the m5 host battery: every conformance arm equal to HotSpot in both modes and under G1 / Generational, `rlock` 402 -> 59 ns/op), nio21 wave 21 L1
/// (`r21-lambda-handler-impl-sam-calls-round-trip-through-rust-FIXED-20261010.md`):
/// a static lambda impl WITH an exception table is admitted to the compiled
/// lambda site (`interpreter::lambda_jit_site`), so to the Rust direct arm and
/// the machine-code thunk, because a throwable it leaves is routed through
/// ITS table by both services (`jit::helpers::lambda_route_thrown_through_impl`).
/// Off: such an impl keeps the generic one-shot route, as before. Cached with
/// the other throw-path switches.
pub(crate) fn lambda_handler_impl_enabled() -> bool {
    throw1_switches().lambda_handler_impl
}

pub(crate) fn throwable_recapture_if_moved_enabled() -> bool {
    throw1_switches().recapture_if_moved
}

fn live_trap_snapshot_stands_in_enabled() -> bool {
    throw1_switches().live_trap_snapshot_stands_in
}

// ===========================================================================
// nio21 wave 19 L1: the JDK's implicit exceptions built without interpreting
// their constructor chains (proposal L1W18-1)
// ===========================================================================
//
// A compiled `a[5]`, `x / 0` or null dereference builds
// `ArrayIndexOutOfBoundsException(String)`, `ArithmeticException(String)` or
// `NullPointerException()` / `(String)`. Through `create_exception_object` that
// is four or five interpreted `<init>` frames (each a bare delegation), then
// `Throwable.<init>`'s field initialisers, the virtual `fillInStackTrace()`
// (synchronized) and its native `fillInStackTrace(0)`: most of the ~2.5 us a
// default-path caught implicit exception cost on the w18 host.
//
// The JDK 25 bytecode of that chain is a fixed function of the receiver:
//
//   cause = this; stackTrace = UNASSIGNED_STACK;
//   suppressedExceptions = SUPPRESSED_SENTINEL;
//   [NullPointerException.fillInStackTrace(): extendedMessageState 0 -> 1]
//   fillInStackTrace(0)  -- the VM capture: backtrace, depth;
//   stackTrace = UNASSIGNED_STACK; detailMessage = message;
//   if (jfrTracing) ThrowableTracer.traceThrowable(getClass(), message)
//
// so [`try_build_implicit_throwable_directly`] performs exactly those stores
// and the same capture, and refuses (the caller interprets the constructors as
// before) unless the class bytes ARE that chain: every constructor from the
// class up to `Throwable` must match its JDK 25 opcode shape byte for byte
// (constant-pool indices aside), no class below `Throwable` may declare a
// `fillInStackTrace` except `NullPointerException`'s own (shape-checked too),
// `fillInStackTrace(int)` must be native, the classes initialized and
// bootstrap-defined, `jfrTracing` false, and nothing may observe the
// constructors running (a JVMTI listener, an interpreter-only event, a JDWP
// breakpoint, method or exception request). The capture runs with no
// constructor frame above the throw site; the fill-frame trim
// (`stackwalker::trim_throwable_fill_frames`) applies HotSpot's rule to the
// frames that are there, so the trace is the one the trimmed constructor path
// stores. The verdict is cached per thread, per VM and class, and recomputed
// after any class redefinition. `CRATONVM_THROWABLE_DIRECT_IMPLICIT_BUILD=0`
// interprets the constructors always.

/// What [`try_build_implicit_throwable_directly`] needs about one class, once
/// its constructor chain has been proven to be the JDK's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirectBuildPlan {
    /// `java/lang/Throwable`, for its two statics.
    throwable_class: ClassId,
    /// Static-field index of `Throwable.UNASSIGNED_STACK`.
    unassigned_stack_static: usize,
    /// Static-field index of `Throwable.jfrTracing`.
    jfr_tracing_static: usize,
    /// The chain contains `NullPointerException.fillInStackTrace()`, which the
    /// constructor's virtual call runs: `extendedMessageState` 0 -> 1.
    npe_fill_override: bool,
}

/// One byte of an opcode shape: `Some(b)` must match, `None` is a
/// constant-pool index byte (any value).
type ShapeByte = Option<u8>;
const CP: ShapeByte = None;
const fn op(b: u8) -> ShapeByte {
    Some(b)
}

/// `X(String s) { super(s); }`: `aload_0 aload_1 invokespecial #s return`.
const DELEGATE_INIT_STRING: &[ShapeByte] = &[op(0x2a), op(0x2b), op(0xb7), CP, CP, op(0xb1)];
/// `X() { super(); }`: `aload_0 invokespecial #s return`.
const DELEGATE_INIT_EMPTY: &[ShapeByte] = &[op(0x2a), op(0xb7), CP, CP, op(0xb1)];

/// JDK 25 `Throwable(String message)` (48 bytes): `Object.<init>`, the three
/// field initialisers, `fillInStackTrace(); detailMessage = message;` and the
/// `jfrTracing` tail (`ifeq +11`).
#[rustfmt::skip]
const THROWABLE_INIT_STRING: &[ShapeByte] = &[
    op(0x2a), op(0xb7), CP, CP, // aload_0; invokespecial Object.<init>
    op(0x2a), op(0x2a), op(0xb5), CP, CP, // cause = this
    op(0x2a), op(0xb2), CP, CP, op(0xb5), CP, CP, // stackTrace = UNASSIGNED_STACK
    op(0x2a), op(0xb2), CP, CP, op(0xb5), CP, CP, // suppressedExceptions = SUPPRESSED_SENTINEL
    op(0x2a), op(0xb6), CP, CP, op(0x57), // fillInStackTrace(); pop
    op(0x2a), op(0x2b), op(0xb5), CP, CP, // detailMessage = message
    op(0xb2), CP, CP, op(0x99), op(0x00), op(0x0b), // if (jfrTracing)
    op(0x2a), op(0xb6), CP, CP, op(0x2b), op(0xb8), CP, CP, // traceThrowable(getClass(), message)
    op(0xb1),
];

/// JDK 25 `Throwable()` (43 bytes): as above without `detailMessage`, and
/// `aconst_null` for the traced message.
#[rustfmt::skip]
const THROWABLE_INIT_EMPTY: &[ShapeByte] = &[
    op(0x2a), op(0xb7), CP, CP,
    op(0x2a), op(0x2a), op(0xb5), CP, CP,
    op(0x2a), op(0xb2), CP, CP, op(0xb5), CP, CP,
    op(0x2a), op(0xb2), CP, CP, op(0xb5), CP, CP,
    op(0x2a), op(0xb6), CP, CP, op(0x57),
    op(0xb2), CP, CP, op(0x99), op(0x00), op(0x0b),
    op(0x2a), op(0xb6), CP, CP, op(0x01), op(0xb8), CP, CP,
    op(0xb1),
];

/// JDK 25 `synchronized Throwable fillInStackTrace()` (29 bytes): `if
/// (stackTrace != null || backtrace != null) { fillInStackTrace(0);
/// stackTrace = UNASSIGNED_STACK; } return this;`.
#[rustfmt::skip]
const THROWABLE_FILL_IN_STACK_TRACE: &[ShapeByte] = &[
    op(0x2a), op(0xb4), CP, CP, op(0xc7), op(0x00), op(0x0a), // stackTrace ifnonnull +10
    op(0x2a), op(0xb4), CP, CP, op(0xc6), op(0x00), op(0x10), // backtrace ifnull +16
    op(0x2a), op(0x03), op(0xb6), CP, CP, op(0x57), // fillInStackTrace(0); pop
    op(0x2a), op(0xb2), CP, CP, op(0xb5), CP, CP, // stackTrace = UNASSIGNED_STACK
    op(0x2a), op(0xb0),
];

/// JDK 25 `synchronized Throwable NullPointerException.fillInStackTrace()`
/// (41 bytes): `extendedMessageState` 0 -> 1 (1 -> 2 computing the message),
/// then `super.fillInStackTrace()`.
#[rustfmt::skip]
const NPE_FILL_IN_STACK_TRACE: &[ShapeByte] = &[
    op(0x2a), op(0xb4), CP, CP, op(0x9a), op(0x00), op(0x0b), // state ifne +11
    op(0x2a), op(0x04), op(0xb5), CP, CP, op(0xa7), op(0x00), op(0x18), // state = 1; goto +24
    op(0x2a), op(0xb4), CP, CP, op(0x04), op(0xa0), op(0x00), op(0x10), // state if_icmpne 1, +16
    op(0x2a), op(0x2a), op(0xb6), CP, CP, op(0xb5), CP, CP, // extendedMessage = getExtendedNPEMessage()
    op(0x2a), op(0x05), op(0xb5), CP, CP, // state = 2
    op(0x2a), op(0xb7), CP, CP, op(0xb0), // return super.fillInStackTrace()
];

/// Does `code` have exactly `shape`'s opcodes and fixed operands? Pure.
fn code_matches_shape(code: &[u8], shape: &[ShapeByte]) -> bool {
    code.len() == shape.len()
        && code
            .iter()
            .zip(shape)
            .all(|(byte, want)| want.is_none_or(|w| w == *byte))
}

/// A non-native method whose decoded `Code` has no handlers and `shape`.
fn method_has_shape(
    method: &cratonvm_reader::method::ClassFileMethod,
    shape: &[ShapeByte],
) -> bool {
    !method.is_native()
        && method
            .code()
            .is_some_and(|c| c.exception_table.is_empty() && code_matches_shape(&c.code, shape))
}

/// One constant-pool operand of a shape the direct build relies on: the
/// operand's byte offset in the code, and the member it must name -- owner
/// (`None`: any), name, descriptor. JIT round 23 wave 1, lane L3: the shapes
/// leave every index free, so an agent or a patched class library that kept
/// the opcodes but pointed a `putfield` / `invokevirtual` at another member
/// passed the proof. Only the operands the build models are listed: the
/// `jfrTracing` tail's `getClass` / `traceThrowable` run only when the build
/// refuses anyway.
type CpRef = (usize, Option<&'static str>, &'static str, &'static str);

const THROWABLE: Option<&str> = Some("java/lang/Throwable");
const STACK_ARRAY: &str = "[Ljava/lang/StackTraceElement;";

/// The members `Throwable(String)` names, at its operand offsets.
#[rustfmt::skip]
const THROWABLE_INIT_STRING_REFS: &[CpRef] = &[
    (2, Some("java/lang/Object"), "<init>", "()V"),
    (7, THROWABLE, "cause", "Ljava/lang/Throwable;"),
    (11, THROWABLE, "UNASSIGNED_STACK", STACK_ARRAY),
    (14, THROWABLE, "stackTrace", STACK_ARRAY),
    (18, THROWABLE, "SUPPRESSED_SENTINEL", "Ljava/util/List;"),
    (21, THROWABLE, "suppressedExceptions", "Ljava/util/List;"),
    (25, THROWABLE, "fillInStackTrace", "()Ljava/lang/Throwable;"),
    (31, THROWABLE, "detailMessage", "Ljava/lang/String;"),
    (34, THROWABLE, "jfrTracing", "Z"),
];

/// The members `Throwable()` names.
#[rustfmt::skip]
const THROWABLE_INIT_EMPTY_REFS: &[CpRef] = &[
    (2, Some("java/lang/Object"), "<init>", "()V"),
    (7, THROWABLE, "cause", "Ljava/lang/Throwable;"),
    (11, THROWABLE, "UNASSIGNED_STACK", STACK_ARRAY),
    (14, THROWABLE, "stackTrace", STACK_ARRAY),
    (18, THROWABLE, "SUPPRESSED_SENTINEL", "Ljava/util/List;"),
    (21, THROWABLE, "suppressedExceptions", "Ljava/util/List;"),
    (25, THROWABLE, "fillInStackTrace", "()Ljava/lang/Throwable;"),
    (29, THROWABLE, "jfrTracing", "Z"),
];

/// The members `Throwable.fillInStackTrace()` names.
#[rustfmt::skip]
const THROWABLE_FILL_REFS: &[CpRef] = &[
    (2, THROWABLE, "stackTrace", STACK_ARRAY),
    (9, THROWABLE, "backtrace", "Ljava/lang/Object;"),
    (17, THROWABLE, "fillInStackTrace", "(I)Ljava/lang/Throwable;"),
    (22, THROWABLE, "UNASSIGNED_STACK", STACK_ARRAY),
    (25, THROWABLE, "stackTrace", STACK_ARRAY),
];

/// The members `NullPointerException.fillInStackTrace()` names. The
/// `super.fillInStackTrace()` owner is left free (javac names the direct
/// superclass); the plan's walk refuses any `fillInStackTrace` between NPE and
/// `Throwable`, so it can only reach `Throwable`'s.
#[rustfmt::skip]
const NPE_FILL_REFS: &[CpRef] = &[
    (2, Some("java/lang/NullPointerException"), "extendedMessageState", "I"),
    (10, Some("java/lang/NullPointerException"), "extendedMessageState", "I"),
    (17, Some("java/lang/NullPointerException"), "extendedMessageState", "I"),
    (26, Some("java/lang/NullPointerException"), "getExtendedNPEMessage", "()Ljava/lang/String;"),
    (29, Some("java/lang/NullPointerException"), "extendedMessage", "Ljava/lang/String;"),
    (34, Some("java/lang/NullPointerException"), "extendedMessageState", "I"),
    (38, None, "fillInStackTrace", "()Ljava/lang/Throwable;"),
];

/// The `CONSTANT_Fieldref` / `CONSTANT_Methodref` the two operand bytes at
/// `code[pos..pos + 2]` index: `(owner, name, descriptor)`.
fn cp_member_at<'a>(
    cp: &'a cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    pos: usize,
) -> Option<(&'a str, &'a str, &'a str)> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let index = u16::from_be_bytes([*code.get(pos)?, *code.get(pos.checked_add(1)?)?]);
    let (class_index, nat_index) = match cp.get(index)? {
        ConstantPoolEntry::FieldReference {
            class_index,
            name_and_type_index,
        }
        | ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        } => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let owner = cp.get_class_name(class_index)?;
    let (name, descriptor) = cp.get_name_and_type(nat_index)?;
    Some((owner, name, descriptor))
}

/// Does every operand `refs` lists name its member? Pure over the pool.
fn code_refs_match(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    refs: &[CpRef],
) -> bool {
    refs.iter().all(|&(pos, owner, name, descriptor)| {
        cp_member_at(cp, code, pos).is_some_and(|(o, n, d)| {
            owner.is_none_or(|want| want == o) && n == name && d == descriptor
        })
    })
}

/// [`method_has_shape`], and its operands name `refs` in `cp` (the declaring
/// class's pool).
fn method_has_shape_and_refs(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    method: &cratonvm_reader::method::ClassFileMethod,
    shape: &[ShapeByte],
    refs: &[CpRef],
) -> bool {
    method_has_shape(method, shape)
        && method
            .code()
            .is_some_and(|c| code_refs_match(cp, &c.code, refs))
}

/// The static-field index (`get_static_shared`'s) of `name` in `fields`.
fn static_field_index(
    fields: &[cratonvm_reader::field::ClassFileField],
    name: &str,
) -> Option<usize> {
    fields
        .iter()
        .filter(|f| f.is_static())
        .position(|f| &*f.name == name)
}

/// Prove `class_id`'s `<init>` chain (`(String)V` when `with_message`, else
/// `()V`) is the JDK 25 one up to `java/lang/Throwable`; see the section note.
fn direct_build_plan(
    shared: &SharedVm,
    class_id: ClassId,
    with_message: bool,
) -> Option<DirectBuildPlan> {
    const NPE: &str = "java/lang/NullPointerException";
    let descriptor = if with_message {
        "(Ljava/lang/String;)V"
    } else {
        "()V"
    };
    let (delegate, root, root_refs, delegate_operand) = if with_message {
        (
            DELEGATE_INIT_STRING,
            THROWABLE_INIT_STRING,
            THROWABLE_INIT_STRING_REFS,
            3,
        )
    } else {
        (
            DELEGATE_INIT_EMPTY,
            THROWABLE_INIT_EMPTY,
            THROWABLE_INIT_EMPTY_REFS,
            2,
        )
    };
    let cm = shared.classes.class_manager.read();
    let throwable = cm.loaded_class_under_exact_key(
        "java/lang/Throwable",
        cratonvm_types::ClassLoaderId::Bootstrap,
    )?;
    let mut npe_fill_override = false;
    let mut cursor = class_id;
    // Bounded: a real hierarchy from these classes is five deep.
    for _ in 0..16 {
        let cls = cm.get_class(cursor)?;
        // Initialisation is the caller's per-throw test (of the class itself;
        // its superclasses are initialised before it).
        if cls.hidden || cls.has_finalizer {
            return None;
        }
        if cursor == throwable {
            let init = cls.find_method("<init>", descriptor)?;
            let fill = cls.find_method("fillInStackTrace", "()Ljava/lang/Throwable;")?;
            let fill_native = cls.find_method("fillInStackTrace", "(I)Ljava/lang/Throwable;")?;
            let cp = &cls.constant_pool;
            if !method_has_shape_and_refs(cp, init, root, root_refs)
                || !fill.is_synchronized()
                || !method_has_shape_and_refs(
                    cp,
                    fill,
                    THROWABLE_FILL_IN_STACK_TRACE,
                    THROWABLE_FILL_REFS,
                )
                || !fill_native.is_native()
            {
                return None;
            }
            return Some(DirectBuildPlan {
                throwable_class: throwable,
                unassigned_stack_static: static_field_index(&cls.fields, "UNASSIGNED_STACK")?,
                jfr_tracing_static: static_field_index(&cls.fields, "jfrTracing")?,
                npe_fill_override,
            });
        }
        // Below `Throwable`: no `fillInStackTrace` of any descriptor, except
        // NullPointerException's own JDK override.
        for m in cls
            .methods
            .iter()
            .filter(|m| &*m.name == "fillInStackTrace")
        {
            let jdk_npe_override = &*cls.name == NPE
                && !npe_fill_override
                && &*m.descriptor == "()Ljava/lang/Throwable;"
                && m.is_synchronized()
                && method_has_shape_and_refs(
                    &cls.constant_pool,
                    m,
                    NPE_FILL_IN_STACK_TRACE,
                    NPE_FILL_REFS,
                );
            if !jdk_npe_override {
                return None;
            }
            npe_fill_override = true;
        }
        // The delegation must call the SUPERCLASS's constructor of the same
        // descriptor (JIT round 23 wave 1, lane L3: the shape alone admitted
        // any `<init>` the index named).
        let superclass = cls.superclass?;
        let super_name = cm.get_class(superclass)?.name.clone();
        let delegate_ref: [CpRef; 1] = [(delegate_operand, None, "<init>", descriptor)];
        let init = cls.find_method("<init>", descriptor)?;
        if !method_has_shape_and_refs(&cls.constant_pool, init, delegate, &delegate_ref)
            || init
                .code()
                .and_then(|c| cp_member_at(&cls.constant_pool, &c.code, delegate_operand))
                .is_none_or(|(owner, _, _)| owner != &*super_name)
        {
            return None;
        }
        cursor = superclass;
    }
    None
}

/// [`direct_build_plan`], cached in this thread's [`ImplicitThrowTls`] per VM,
/// class and constructor, and recomputed after any class redefinition. Only a
/// proof is cached: a refusal (bytes that are not the JDK's, or a `Code`
/// attribute not decoded yet) is asked again on the next throw, which then
/// interprets its constructors as before.
fn cached_direct_build_plan(
    shared: &SharedVm,
    class_id: ClassId,
    with_message: bool,
) -> Option<DirectBuildPlan> {
    let stamp = cratonvm_classloading::class_redefinition_count();
    let key = (shared.vm_identity, class_id.as_u32(), with_message);
    let hit = FAST_THROW_SITES.with(|tls| {
        let mut tls = tls.try_borrow_mut().ok()?;
        if tls.direct_stamp != stamp {
            tls.direct.clear();
            tls.direct_stamp = stamp;
        }
        tls.direct.get(&key).copied()
    });
    if let Some(verdict) = hit {
        return verdict;
    }
    let plan = direct_build_plan(shared, class_id, with_message);
    FAST_THROW_SITES.with(|tls| {
        if let Ok(mut tls) = tls.try_borrow_mut() {
            // Bounded: six classes times two constructors per VM.
            if plan.is_some() && tls.direct_stamp == stamp && tls.direct.len() < 64 {
                tls.direct.insert(key, plan);
            }
        }
    });
    plan
}

/// Could anything observe the constructors NOT running? A JVMTI listener of
/// this VM (`MethodEntry`, `FieldModification`, ...), an interpreter-only
/// event, a JDWP breakpoint / method / exception request, an `Exception`
/// client. Each makes the direct build refuse.
fn direct_build_observable(shared: &SharedVm) -> bool {
    if crate::runtime::interpreter::jvmti_requires_interpreter(shared)
        || super::interpreter::exception_events_may_be_armed(shared)
        || (crate::runtime::jvmti::any_listener_active()
            && crate::runtime::jvmti::any_listener_active_for_vm(shared.vm_identity))
    {
        return true;
    }
    #[cfg(feature = "experimental-debug")]
    {
        if shared.debug.debugger_gates.method_events_armed()
            || shared.debug.debugger_gates.session_attached()
        {
            return true;
        }
    }
    false
}

/// The field slots and statics a direct build stores, resolved before
/// anything is allocated or changed, so a refusal leaves no trace.
#[derive(Clone, Copy)]
struct DirectBuildSlots {
    plan: DirectBuildPlan,
    cause: usize,
    stack_trace: usize,
    suppressed: usize,
    detail_message: usize,
    backtrace: usize,
    depth: usize,
    /// NullPointerException's `extendedMessageState`, when its override runs.
    npe_state: Option<usize>,
}

/// Every per-throw precondition of a direct build of `class_id` (already
/// known initialized) with or without a message: the switch-independent half
/// shared by [`try_build_implicit_throwable_directly`] and
/// [`jit_new_throwable_direct`].
fn direct_build_slots(
    shared: &SharedVm,
    class_id: ClassId,
    with_message: bool,
) -> Option<DirectBuildSlots> {
    if direct_build_observable(shared) {
        return None;
    }
    let plan = cached_direct_build_plan(shared, class_id, with_message)?;
    // Read per throw: JFR can switch it on at any time.
    if !matches!(
        crate::vm::get_static_shared(shared, plan.throwable_class, plan.jfr_tracing_static),
        Value::Int(0)
    ) {
        return None;
    }
    let slots = DirectBuildSlots {
        plan,
        cause: throwable_own_field_index(shared, "cause")?,
        stack_trace: throwable_own_field_index(shared, "stackTrace")?,
        suppressed: throwable_own_field_index(shared, "suppressedExceptions")?,
        detail_message: throwable_own_field_index(shared, "detailMessage")?,
        backtrace: throwable_own_field_index(shared, "backtrace")?,
        depth: throwable_own_field_index(shared, "depth")?,
        npe_state: if plan.npe_fill_override {
            Some(npe_extended_message_slots(shared, class_id)?.1)
        } else {
            None
        },
    };
    if !matches!(
        crate::vm::get_static_shared(shared, plan.throwable_class, plan.unassigned_stack_static),
        Value::Object(Some(_))
    ) || throwable_suppressed_sentinel(shared).is_none()
    {
        return None;
    }
    Some(slots)
}

/// The constructor chain's effect on the throwable pinned at
/// `thread.native_pin_roots[pin_base]` (a fresh instance, every field at its
/// default), with the message `String` pinned at `message_pin` (`None`: the
/// `()V` chain, which leaves `detailMessage` alone). Allocates nothing.
fn fill_throwable_directly(
    shared: &SharedVm,
    thread: &mut JvmThread,
    slots: &DirectBuildSlots,
    pin_base: usize,
    message_pin: Option<usize>,
) {
    use cratonvm_native_api::registry::{NativeExceptionAccess, NativeHeapAccess};
    // The field initialisers (statics re-read: an allocation before this may
    // have collected).
    let heap = &shared.mem.heap;
    let unassigned = crate::vm::get_static_shared(
        shared,
        slots.plan.throwable_class,
        slots.plan.unassigned_stack_static,
    );
    let sentinel = throwable_suppressed_sentinel(shared).unwrap_or(Value::Object(None));
    let obj = thread.native_pin_roots[pin_base];
    heap.set_field(obj, slots.cause, Value::Object(Some(obj)));
    heap.set_field(obj, slots.stack_trace, unassigned);
    heap.set_field(obj, slots.suppressed, sentinel);
    // `fillInStackTrace()`: NullPointerException's override first.
    if let Some(slot) = slots.npe_state {
        heap.set_field(obj, slot, Value::Int(1));
    }
    // `fillInStackTrace(0)`: the native's capture and its two stores.
    let depth = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        ctx.capture_throwable_stack_trace(obj)
    };
    // nio21 wave 20 L1 / wave 21 L1: a collection inside the capture moving
    // the throwable is repaired by the capture itself (it pins the throwable
    // and captures again at the new address under
    // `CRATONVM_THROWABLE_RECAPTURE_IF_MOVED`), for this door and the
    // interpreted constructors' alike.
    let obj = thread.native_pin_roots[pin_base];
    // Cast: bounded by the frame count or `MaxJavaStackTraceDepth`.
    let depth = i32::try_from(depth).unwrap_or(i32::MAX);
    let filled = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        ctx.finish_throwable_fill(obj, depth)
    };
    let obj = thread.native_pin_roots[pin_base];
    if filled.is_none() {
        heap.set_field(obj, slots.backtrace, Value::Object(Some(obj)));
        heap.set_field(obj, slots.depth, Value::Int(depth));
    }
    // `stackTrace = UNASSIGNED_STACK` again (no-op), then `detailMessage`.
    if let Some(pin) = message_pin {
        let message = thread.native_pin_roots[pin];
        heap.set_field(obj, slots.detail_message, Value::Object(Some(message)));
    }
}

/// `CRATONVM_JIT_IMPLICIT_THROW_LEAN_BUILD` -- default ON (JIT round 20 wave
/// 18, lane xThrow, THROW20-5 of `docs/feature-designs/jit-ir-optimizations.md`),
/// `=0` off: the per-throw work of a compiled implicit exception's direct build
/// that does not change what is built. [`try_build_implicit_throwable_directly`]
/// keeps the bootstrap class it found initialized per thread and allocates the
/// throwable and its message from the thread's TLAB;
/// `jit::helpers::materialize_implicit_signal_or_frames` formats the AIOOBE
/// message without a heap `String` ([`format_aioobe_message`]);
/// `jit::helpers::note_trap_snapshot_bound` stops re-reading its switch from the
/// flag table twice per throw. Cached with the other throw-path switches.
pub(crate) fn implicit_throw_lean_build_enabled() -> bool {
    throw1_switches().implicit_throw_lean_build
}

/// The [`ImplicitThrowTls::classes`] slot of one of the classes
/// [`try_build_implicit_throwable_directly`] builds, `None` for any other.
fn implicit_direct_class_slot(class_name: &str) -> Option<usize> {
    implicit_direct_class_slot_as(class_name, direct_build_every_door_enabled())
}

/// [`implicit_direct_class_slot`] with the widening switch given. Pure. The
/// three classes past slot 2 (JIT round 23 wave 1, lane L3) are raised by the
/// interpreter and by JIT helpers called with their compiled caller live, so
/// they reach the direct build only through [`create_exception_object`]; each
/// delegates `(String)` to `RuntimeException` exactly as the other three do,
/// which [`direct_build_plan`] proves from the bytes before any build.
fn implicit_direct_class_slot_as(class_name: &str, every_door: bool) -> Option<usize> {
    match class_name {
        "java/lang/ArrayIndexOutOfBoundsException" => Some(0),
        "java/lang/ArithmeticException" => Some(1),
        "java/lang/NullPointerException" => Some(2),
        "java/lang/ClassCastException" if every_door => Some(3),
        "java/lang/ArrayStoreException" if every_door => Some(4),
        "java/lang/NegativeArraySizeException" if every_door => Some(5),
        _ => None,
    }
}

/// `CRATONVM_THROWABLE_DIRECT_IMPLICIT_BUILD_EVERY_DOOR` -- default ON (JIT
/// round 23 wave 1, lane L3; proposal L1W18-1 widened to every door), `=0`
/// off: the direct build of the JDK's implicit exceptions
/// ([`try_build_implicit_throwable_directly`]) is asked by
/// [`create_exception_object`] too -- the interpreter's implicit exceptions,
/// the JIT helpers' (`checkcast`, `aastore`, negative `newarray`) and a
/// native's -- and covers `ClassCastException`, `ArrayStoreException` and
/// `NegativeArraySizeException` besides AIOOBE, `ArithmeticException` and
/// NPE. Off: only the compiled trap-frame door builds directly, and only the
/// first three (the code before this round). Under
/// `CRATONVM_THROWABLE_DIRECT_IMPLICIT_BUILD=0` nothing builds directly
/// whatever this says. Cached with the other throw-path switches.
pub(crate) fn direct_build_every_door_enabled() -> bool {
    throw1_switches().direct_build_every_door
}

/// The class [`remember_implicit_direct_class`] kept in `slot` for this VM,
/// while no class was redefined since. Sound to reuse without the
/// class-manager: a bootstrap class is never unloaded, its id never reissued
/// (`vm_identity` is never reused either), initialization and linking only
/// ever go forward, and a redefinition (which cannot change the field count)
/// drops the row anyway.
fn cached_implicit_direct_class(shared: &SharedVm, slot: usize) -> Option<(ClassId, usize)> {
    let stamp = cratonvm_classloading::class_redefinition_count();
    FAST_THROW_SITES.with(|tls| {
        let tls = tls.try_borrow().ok()?;
        if tls.classes_stamp != stamp {
            return None;
        }
        let (vm, class_raw, num_fields) = (*tls.classes.get(slot)?)?;
        (vm == shared.vm_identity).then(|| (ClassId::new(class_raw), num_fields))
    })
}

/// Keep `found` (initialized and linked: the caller's test) for
/// [`cached_implicit_direct_class`].
fn remember_implicit_direct_class(shared: &SharedVm, slot: usize, found: (ClassId, usize)) {
    let stamp = cratonvm_classloading::class_redefinition_count();
    FAST_THROW_SITES.with(|tls| {
        if let Ok(mut tls) = tls.try_borrow_mut() {
            if tls.classes_stamp != stamp {
                tls.classes = [None; IMPLICIT_DIRECT_CLASS_SLOTS];
                tls.classes_stamp = stamp;
            }
            if let Some(row) = tls.classes.get_mut(slot) {
                *row = Some((shared.vm_identity, found.0.as_u32(), found.1));
            }
        }
    });
}

/// The fresh implicit throwable from this thread's TLAB, shaped as the
/// interpreter's `new` shapes it (`gc_alloc_object`: the same planner and
/// site), or `None` (an object too large for a TLAB, or a miss) and the caller
/// takes the heap's no-GC allocator as before. Performs no collection.
fn implicit_throwable_tlab_alloc(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    num_fields: usize,
) -> Option<ObjectRef> {
    let (total_size, _, _) = super::interpreter::plan_tlab_object_shape_for(
        shared,
        class_id,
        num_fields,
        super::interpreter::tlab_site::INTERPRETER,
    );
    if total_size > cratonvm_gc::tlab::tlab_max_alloc() {
        return None;
    }
    super::interpreter::tlab_alloc_object(thread, shared, class_id, num_fields, total_size)
}

/// A fixed buffer [`format_aioobe_message`] writes into: the longest message
/// (two `i64::MIN`s) is 71 bytes.
pub(crate) type AioobeMessageBuf = [u8; 96];

/// HotSpot's AIOOBE message for a compiled array access, `Index <index> out of
/// bounds for length <length>`, formatted into `buf` with no heap `String`
/// (`CRATONVM_JIT_IMPLICIT_THROW_LEAN_BUILD`). `None` only if `buf` were too
/// small, which its size rules out; the caller then formats as before.
pub(crate) fn format_aioobe_message(
    buf: &mut AioobeMessageBuf,
    index: i64,
    length: i64,
) -> Option<&str> {
    use std::io::Write;
    let mut cursor = std::io::Cursor::new(&mut buf[..]);
    write!(cursor, "Index {index} out of bounds for length {length}").ok()?;
    let end = usize::try_from(cursor.position()).ok()?;
    std::str::from_utf8(&buf[..end]).ok()
}

/// Build the implicit exception `class_name` (one of the classes
/// [`implicit_direct_class_slot`] names) with `message` WITHOUT interpreting
/// its constructor chain, or `None` to have the caller run the constructors
/// ([`create_exception_object_constructed`]; nothing
/// was allocated or changed; at most an unreachable instance was). See the
/// section note above for the contract.
///
/// Never `Some(Err(..))` since JIT round 23 wave 1 (lane L3): a message the
/// heap cannot hold is a refusal, so the constructor path's collection ladder
/// decides the `OutOfMemoryError`. The `Result` stays for the callers' shape.
pub(crate) fn try_build_implicit_throwable_directly(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    message: Option<&str>,
) -> Option<Result<ObjectRef, MethodCallFailed>> {
    if !throw1_switches().direct_implicit_build {
        return None;
    }
    // JIT round 20 wave 18 (lane xThrow, THROW20-5;
    // `CRATONVM_JIT_IMPLICIT_THROW_LEAN_BUILD`, default ON): the class found
    // initialized and linked on an earlier throw, without the class-manager
    // read lock and the by-name probe; the instance and its message from this
    // thread's TLAB, as the interpreter's `new` and a dynamic `String` get
    // them. The same objects, fields and capture.
    let lean = implicit_throw_lean_build_enabled();
    let class_slot = implicit_direct_class_slot(class_name)?;
    let cached = if lean {
        cached_implicit_direct_class(shared, class_slot)
    } else {
        None
    };
    let (class_id, num_fields) = match cached {
        Some(hit) => hit,
        None => {
            let found = {
                let cm = shared.classes.class_manager.read();
                let class_id = cm.loaded_class_under_exact_key(
                    class_name,
                    cratonvm_types::ClassLoaderId::Bootstrap,
                )?;
                let class = cm.get_class(class_id)?;
                if !crate::vm::is_class_initialized_fast(class)
                    || crate::vm::class_awaits_link(class)
                {
                    return None;
                }
                (class_id, class.num_total_fields)
            };
            if lean {
                remember_implicit_direct_class(shared, class_slot, found);
            }
            found
        }
    };
    // An implicit NPE's JEP 358 text is built HotSpot's way: the `()V` chain,
    // then the text in `extendedMessage` ([`store_npe_extended_message`];
    // interpreter round i4 wave 99, lane L9). A class library without those
    // fields takes the constructor path, which keeps the `detailMessage`.
    let implicit_npe = is_implicit_npe_text(class_name, message);
    if implicit_npe {
        npe_extended_message_slots(shared, class_id)?;
    }
    let slots = direct_build_slots(shared, class_id, message.is_some() && !implicit_npe)?;
    // `new`: a young-generation miss goes to the constructor path, whose
    // collection ladder this door does not duplicate.
    let obj = lean
        .then(|| implicit_throwable_tlab_alloc(shared, thread, class_id, num_fields))
        .flatten()
        .or_else(|| shared.mem.heap.try_alloc_object(class_id, num_fields))?;
    let _ = super::interpreter::init_primitive_fields(shared, obj, class_id);
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(obj);
    // The message: a fresh `String`, as `create_exception_object_for_class_inner`
    // makes it (an existing pool entry only when the heap cannot hold one).
    let message_pin = match message {
        Some(msg) => {
            let fresh = if lean {
                // Performs no collection (TLAB bump or refill, else the
                // heap's no-GC allocator), so `obj` stays where it is pinned.
                crate::vm::try_create_java_string_uninterned_gc_safe_threaded(shared, thread, msg)
            } else {
                crate::vm::try_create_java_string_uninterned(shared, msg)
            }
            .or_else(|| pooled_message_hit(shared, msg));
            // JIT round 23 wave 1, lane L3: a message the heap cannot hold
            // without collecting is a refusal (the unreachable instance above
            // is garbage), so the constructor path's collection ladder runs
            // before any `OutOfMemoryError` -- the answer this door gave
            // straight away while the throwable itself had found room.
            let Some(string_ref) = fresh else {
                thread.native_pin_roots.truncate(pin_base);
                return None;
            };
            thread.native_pin_roots.push(string_ref);
            Some(pin_base + 1)
        }
        None => None,
    };
    if implicit_npe {
        fill_throwable_directly(shared, thread, &slots, pin_base, None);
        if let Some(pin) = message_pin {
            let obj = thread.native_pin_roots[pin_base];
            let text = thread.native_pin_roots[pin];
            if !store_npe_extended_message(shared, obj, text) {
                shared
                    .mem
                    .heap
                    .set_field(obj, slots.detail_message, Value::Object(Some(text)));
            }
        }
    } else {
        fill_throwable_directly(shared, thread, &slots, pin_base, message_pin);
    }
    let obj = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);
    Some(Ok(obj))
}

/// nio21 wave 20 L1 (proposal L1W19-1; `CRATONVM_JIT_DIRECT_THROW_NEW`,
/// default ON): `invokespecial X.<init>(String)` / `X.<init>()` reached from
/// compiled code through the dispatch helper, on the instance a `new X` just
/// made, for a bootstrap-defined `X` whose constructor chain is the JDK 25
/// one ([`direct_build_plan`]): the chain's stores and capture, as
/// [`try_build_implicit_throwable_directly`] makes them, instead of the
/// interpreted chain. `true` when done (the call is complete: `<init>` returns
/// nothing); `false` when the caller must invoke the constructor as before,
/// with nothing changed.
///
/// The receiver must be EXACTLY `X` (`new X; invokespecial X.<init>`): a
/// `super(msg)` from a subclass constructor reaches here with a subclass
/// receiver, whose own `fillInStackTrace` override, field initialisers and
/// trim frames this does not model, so it is refused. The capture runs with
/// the compiled caller innermost -- the frames the trimmed constructor path
/// leaves -- and the `-XX:MaxJavaStackTraceDepth` cap, memo and stand-in rules
/// are the capture's own.
///
/// `message` is the `String` argument, `args[1]` of the call: `None` for
/// `()V` and for a null `String` (whose `detailMessage = null` is the field's
/// default already).
pub(crate) fn jit_new_throwable_direct(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    descriptor: &str,
    receiver: ObjectRef,
    message: Option<ObjectRef>,
) -> bool {
    let with_message = match descriptor {
        "(Ljava/lang/String;)V" => true,
        "()V" => false,
        _ => return false,
    };
    if !throw1_switches().direct_throw_new || (!with_message && message.is_some()) {
        return false;
    }
    let class_id = shared.mem.heap.class_id_of(receiver);
    {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return false;
        };
        if &*class.name != class_name
            || cm.loaded_class_under_exact_key(class_name, cratonvm_types::ClassLoaderId::Bootstrap)
                != Some(class_id)
            || !crate::vm::is_class_initialized_fast(class)
            || crate::vm::class_awaits_link(class)
        {
            return false;
        }
    }
    // A throwable at all (the plan walks to `Throwable` or refuses).
    let Some(slots) = direct_build_slots(shared, class_id, with_message) else {
        return false;
    };
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(receiver);
    let message_pin = message.map(|m| {
        thread.native_pin_roots.push(m);
        pin_base + 1
    });
    fill_throwable_directly(shared, thread, &slots, pin_base, message_pin);
    thread.native_pin_roots.truncate(pin_base);
    // JIT round 20 (lane throw): a compiled-only `throw new X(..)` loop reaches
    // no other door that takes the retained-trace registry's collection
    // request (`collect_if_requested`). Here the constructor's effect is
    // complete and the dispatch helper returns straight to compiled code
    // without reading `receiver` / `message` again; the call site is a GC
    // point (the ordinary route interprets the constructor), so the compiled
    // frame's references are mapped and remapped as on that route.
    collect_if_requested(shared, thread);
    true
}

/// The `NullPointerException` a compiled null dereference signalled, built
/// with the trap snapshot ([`create_exception_object_with_trap_frames`]) and
/// its JEP 358 message -- now, or, under `--jdk-only`, on `getMessage()`.
///
/// Round 15 wave 7 (lane throw2, TH1-5;
/// `r15w5-throw1-plain-throw-costs-3x-hotspot-CLOSED-20260930.md` cost item C7). Two
/// differences from HotSpot, one cost and one answer:
///
/// * **Cost.** Every compiled implicit NPE built its message at the throw: the
///   trap site's three `String`s, the per-site memo probe and its message
///   clone, and the message's Java `String` -- for an exception that is
///   usually caught and dropped unread. HotSpot computes the message only in
///   `NullPointerException.getExtendedNPEMessage()`, which `getMessage()` and
///   a second `fillInStackTrace()` call.
/// * **Answer.** HotSpot's implicit NPE is constructed with NO message:
///   `detailMessage` stays `null` for its whole life (the computed text goes
///   to the transient `extendedMessage`), so serialization writes `null`, a
///   deserialized copy answers `getMessage() == null`, and HotSpot's own
///   readers of the field (a cause's message in an `ExceptionInInitializerError`
///   / a recorded resolution failure) see none. CratonVM wrote the text into
///   `detailMessage`.
///
/// So when the deferral is admitted (`CRATONVM_NPE_DEFERRED_MESSAGE`, default
/// ON; either mode, see below; the helpful NPE gates open; a snapshot naming an EXACT trap site,
/// `jit_npe_message::snapshot_names_an_exact_trap_site`) the NPE is built by
/// `NullPointerException()`, as HotSpot builds it, and marked
/// ([`mark_npe_message_deferred`]); the `getExtendedNPEMessage` native asks
/// [`deferred_npe_message`], which computes the message from the stored
/// trace's top frame -- the trap site, by the admission -- exactly as the
/// throw would have. Otherwise, or when the mark cannot be set, the message is
/// built now and stored -- since interpreter round i4 wave 99 (lane L9) where
/// HotSpot's `getMessage()` leaves it, in `extendedMessage` with
/// `extendedMessageState` 2 and no `detailMessage`
/// ([`store_npe_extended_message`], done by the builders this calls).
///
/// `--compatible` too (JIT round 21, lane npecompat, R15B-39: both modes target
/// HotSpot). Its `NullPointerException.getMessage()` is a `Bridge`, which read
/// only `detailMessage` -- the reason this was `--jdk-only` until then; the
/// bridge (`native-builtins` `native_npe_get_message`) now runs the JDK
/// override's body, `extendedMessageState` and `getExtendedNPEMessage()`
/// included, so a deferred NPE answers there as under `--jdk-only`. An NPE the
/// `--compatible` constructor shadow built (not [`try_build_implicit_throwable_directly`])
/// is at `extendedMessageState == 0`, which the mark refuses: its message is
/// written now, the same text.
#[allow(clippy::type_complexity)]
pub(crate) fn create_compiled_implicit_npe(
    shared: &SharedVm,
    thread: &mut JvmThread,
    snapshot: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    npe_action: u8,
) -> Result<
    ObjectRef,
    (
        MethodCallFailed,
        Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    ),
> {
    create_compiled_implicit_npe_live(shared, thread, snapshot, npe_action, false)
}

/// [`create_compiled_implicit_npe`] with a snapshot that may be LIVE; see
/// [`create_exception_object_with_trap_frames_live`] (nio21 wave 18 L1).
#[allow(clippy::type_complexity)]
pub(crate) fn create_compiled_implicit_npe_live(
    shared: &SharedVm,
    thread: &mut JvmThread,
    snapshot: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    npe_action: u8,
    live: bool,
) -> Result<
    ObjectRef,
    (
        MethodCallFailed,
        Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    ),
> {
    const NPE: &str = "java/lang/NullPointerException";
    // j27 wave 9 (lane W9EXC): a null `MethodHandle` at a compiled
    // `invokeExact` / `invoke` stands in HotSpot's `Invokers` check.
    let mut snapshot = snapshot;
    crate::runtime::interpreter::jit_npe_mh_invoker::note_null_invoker_level(
        shared,
        snapshot.as_mut(),
    );
    if npe_message_deferral_admitted(shared, snapshot.as_deref()) {
        let exc = create_exception_object_with_trap_frames_live(
            shared, thread, NPE, None, snapshot, live,
        )?;
        if mark_npe_message_deferred(shared, exc) {
            return Ok(exc);
        }
        // Not marked (a class library without the JDK's two fields, or a
        // constructor that left the state elsewhere): the message the throw
        // would have built, from the trace just stored, written as the eager
        // path writes it (`extendedMessage`, else `detailMessage`; wave 99 L9).
        let pin_base = thread.native_pin_roots.len();
        thread.native_pin_roots.push(exc);
        if let Some(text) = deferred_npe_message_text(shared, exc) {
            // May collect: `exc` is re-read from its pin.
            if let Some(string_ref) = crate::vm::try_create_java_string_uninterned(shared, &text) {
                let exc = thread.native_pin_roots[pin_base];
                if !store_npe_extended_message(shared, exc, string_ref) {
                    set_detail_message_by_name(shared, exc, string_ref);
                }
            }
        }
        let exc = thread.native_pin_roots[pin_base];
        thread.native_pin_roots.truncate(pin_base);
        return Ok(exc);
    }
    let message = crate::runtime::interpreter::jit_npe_message::jit_npe_message(
        shared,
        snapshot.as_deref(),
        npe_action,
    )
    .filter(|m| !m.is_empty());
    create_exception_object_with_trap_frames_live(
        shared,
        thread,
        NPE,
        message.as_deref(),
        snapshot,
        live,
    )
}

/// The text a deferred NPE's `extendedMessage` holds until `getMessage()`
/// replaces it: a POOLED `String` (per VM; the pool is a strong GC root on G1 /
/// ZGC, while a concurrent cycle is open, and under
/// `CRATONVM_GC_WEAK_STRING_TABLE=0` -- under the weak table,
/// `memory::string_table`, default on Generational since gcg g6/rs, the mark
/// dies when no NPE holds it and the next deferral re-creates it, so a lookup
/// miss in `deferred_npe_message_mark` just means "build the message eagerly"), so the
/// slot always holds a `String` -- a reflective or debugger read of the
/// private transient field sees text, never another type -- and identity with
/// the pool's entry is the mark. Nothing else stores this object there: the
/// JDK writes the field only with `getExtendedNPEMessage()`'s answer, and a
/// program would have to intern this exact text and write it reflectively.
const DEFERRED_NPE_MESSAGE_MARK: &str =
    "\u{0}cratonvm: JEP 358 message deferred to NullPointerException.getExtendedNPEMessage";

/// May [`create_compiled_implicit_npe`] defer the message? Creates the
/// pooled mark on the VM's first deferral (BEFORE the throwable exists, so the
/// allocation moves nothing this function's caller holds unpinned).
fn npe_message_deferral_admitted(
    shared: &SharedVm,
    snapshot: Option<&[crate::jit::conservative_roots::ActiveCompiledFrame]>,
) -> bool {
    throw1_switches().npe_deferred_message
        && !crate::runtime::env_cache::helpful_npe_suppressed_in(shared)
        && crate::runtime::interpreter::jit_npe_message::snapshot_names_an_exact_trap_site(snapshot)
        // A hidden trap site has no message (interpreter round i1 wave 52,
        // lane L4): `jit_npe_message` answers it, not the stored trace's top.
        && !crate::runtime::interpreter::jit_npe_message::snapshot_trap_site_is_hidden(
            shared, snapshot,
        )
        && try_create_java_string(shared, DEFERRED_NPE_MESSAGE_MARK).is_some()
}

/// The interpreter's deferred implicit NPE (interpreter round i4 wave 85, lane
/// L2; `VmConfig::interp_npe_deferred_message`): built WITHOUT a message while
/// the trapping frame is still the innermost one, so its stored trace's top
/// entry is that frame at `last_instr_pc`, then marked for
/// `getExtendedNPEMessage` exactly as [`create_compiled_implicit_npe`] marks
/// a compiled one. When it cannot be marked (no pooled mark, a class library
/// without the JDK's two fields), the message is built now from the stored
/// trace's top frame -- the same text either way.
///
/// JIT round 21 (lane npecompat, R15B-39; `CRATONVM_INTERP_NPE_DIRECT_BUILD`,
/// default ON, `=0` off): the NPE is built first by
/// [`try_build_implicit_throwable_directly`] -- the JDK 25 `NullPointerException()`
/// chain's stores and capture, as a compiled trap builds it, with the
/// trapping frame innermost (the frame the trimmed constructor path leaves).
/// Under `--compatible` the constructor path runs the native shadow, which
/// never runs the JDK's `fillInStackTrace()` override: the NPE stays at
/// `extendedMessageState == 0`, the mark refuses it, and the message lands in
/// `detailMessage`, where HotSpot leaves `null`. Under `--jdk-only` it skips
/// interpreting the constructor chain (the cost half; the result is the same
/// object state). Off, or refused, the constructors run as before.
#[cold]
fn throw_interpreted_npe_deferred(shared: &SharedVm, thread: &mut JvmThread) -> MethodCallFailed {
    const NPE: &str = "java/lang/NullPointerException";
    // The pooled mark first, BEFORE the throwable exists (the allocation moves
    // nothing held unpinned here).
    let mark_ready = try_create_java_string(shared, DEFERRED_NPE_MESSAGE_MARK).is_some();
    let built = if throw1_switches().interp_npe_direct_build {
        try_build_implicit_throwable_directly(shared, thread, NPE, None)
    } else {
        None
    };
    let created = match built {
        Some(created) => created,
        None => create_exception_object(shared, thread, NPE, None),
    };
    let exc = match created {
        Ok(exc) => exc,
        // The ordinary funnel's fallbacks (the preallocated `OutOfMemoryError`
        // on a full heap), for an NPE that then has no message.
        Err(_) => {
            return throw_runtime_error(
                shared,
                thread,
                RuntimeError::NullPointerException { message: None },
            )
        }
    };
    if mark_ready && mark_npe_message_deferred(shared, exc) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    if let Some(text) = deferred_npe_message_text(shared, exc) {
        // May collect: `exc` is re-read from its pin.
        if let Some(string_ref) = crate::vm::try_create_java_string_uninterned(shared, &text) {
            let exc = thread.native_pin_roots[pin_base];
            if !store_npe_extended_message(shared, exc, string_ref) {
                set_detail_message_by_name(shared, exc, string_ref);
            }
        }
    }
    let exc = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);
    MethodCallFailed::ExceptionThrown(exc)
}

/// The pooled mark, if this VM has made it. No allocation.
///
/// gci i21/sw: a hit answers through `string_table::keep_alive_on_hit`, since
/// `mark_npe_message_deferred` stores it into a freshly built NPE (above TAMS
/// during a G1 mark, so never scanned) under the weak G1 table.
fn deferred_npe_message_mark(shared: &SharedVm) -> Option<ObjectRef> {
    pooled_message_hit(shared, DEFERRED_NPE_MESSAGE_MARK)
}

/// gci i21/sw: an EXISTING pool entry for `text` (no insert, no allocation),
/// answered through the weak G1 table's keep-alive
/// (`memory::string_table::keep_alive_on_hit`): every caller stores it into a
/// heap object (a detail message, an NPE's `extendedMessage`).
fn pooled_message_hit(shared: &SharedVm, text: &str) -> Option<ObjectRef> {
    let hit = shared.mem.string_pool.read().get(text).copied();
    hit.map(|obj| crate::memory::string_table::keep_alive_on_hit(shared, obj))
}

/// `(extendedMessage, extendedMessageState)` slots of an instance of
/// `class_id`: the JDK's `NullPointerException` fields, `None` for a class
/// library without them.
fn npe_extended_message_slots(shared: &SharedVm, class_id: ClassId) -> Option<(usize, usize)> {
    Some((
        crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, class_id, "extendedMessage")?,
        crate::vm::vm_exec::resolve_field_slot_by_name_cached(
            shared,
            class_id,
            "extendedMessageState",
        )?,
    ))
}

/// Does `text` start with one of HotSpot's `print_NPE_failed_action` phrases,
/// i.e. is it a JEP 358 helpful-NPE text? No `NullPointerException(String)` the
/// VM raises on a program's behalf (the JDK's own `requireNonNull` texts,
/// `"charset"`, `"Name is null"`, ...) starts with one, and a Java
/// `new NullPointerException(msg)` never travels through the VM's builders.
/// Pure. Interpreter round i4 wave 98 / 99, lane L9.
pub(crate) fn is_helpful_npe_action_text(text: &str) -> bool {
    const ACTIONS: [&str; 9] = [
        "Cannot invoke \"",
        "Cannot read field \"",
        "Cannot assign field \"",
        "Cannot read the array length",
        "Cannot load from ",
        "Cannot store to ",
        "Cannot enter synchronized block",
        "Cannot exit synchronized block",
        "Cannot throw exception",
    ];
    ACTIONS.iter().any(|a| text.starts_with(a))
}

/// Is a VM-built `class_name` throwable with `message` an IMPLICIT
/// `NullPointerException` whose JEP 358 text is known at the throw? Those are
/// built HotSpot's way ([`store_npe_extended_message`]): no `detailMessage`,
/// the text in the transient `extendedMessage`. Interpreter round i4 wave 99,
/// lane L9 (`i98-L9-compatible-implicit-npe-text-is-a-detail-message`).
fn is_implicit_npe_text(class_name: &str, message: Option<&str>) -> bool {
    class_name == "java/lang/NullPointerException"
        && message.is_some_and(is_helpful_npe_action_text)
}

/// Give the freshly built implicit NPE `exc` the shape HotSpot's has once
/// `getMessage()` computed its JEP 358 text: `detailMessage` null,
/// `extendedMessage` = `text`, `extendedMessageState` = 2 ("computed"). The
/// JDK's `getMessage()` (and `--compatible`'s `getMessage` bridges,
/// `lang_misc::npe_computed_extended_message`) answer `extendedMessage` for
/// it, its `fillInStackTrace()` override leaves state 2 alone, and the fields
/// are transient, so serialization writes a null message and a deserialized
/// copy answers `getMessage() == null`, as on HotSpot. The eager message used
/// to be the `detailMessage`, which survived the round trip
/// (`tools/probes/interp/L9/L9W98NpeMessageSerialized.java`). `false` (nothing
/// written) for a class library without the JDK's two fields or an NPE whose
/// `detailMessage` is set; the caller then writes the `detailMessage` as
/// before. No allocation. Interpreter round i4 wave 99, lane L9.
fn store_npe_extended_message(shared: &SharedVm, exc: ObjectRef, text: ObjectRef) -> bool {
    let heap = &shared.mem.heap;
    let Some((extended_slot, state_slot)) =
        npe_extended_message_slots(shared, heap.class_id_of(exc))
    else {
        return false;
    };
    if !detail_message_is_null(shared, exc) {
        return false;
    }
    heap.set_field(exc, extended_slot, Value::Object(Some(text)));
    heap.set_field(exc, state_slot, Value::Int(2));
    true
}

/// The text `NullPointerException.getMessage()` answers from the transient
/// `extendedMessage` of `exc`: `Some` only for an instance of the bootstrap
/// `java.lang.NullPointerException` (or a subclass) whose
/// `extendedMessageState` is 2 and whose `extendedMessage` holds a `String`.
/// For the VM's readers of a throwable's MESSAGE (the launcher's and the
/// tests' renderings), which used to find an implicit NPE's text in
/// `detailMessage`; readers of the FIELD (`java_lang_Throwable::message` on
/// HotSpot) must not use it. No allocation. Interpreter round i4 wave 99,
/// lane L9.
pub fn npe_computed_extended_message(shared: &SharedVm, exc: ObjectRef) -> Option<ObjectRef> {
    let heap = &shared.mem.heap;
    let class_id = heap.class_id_of(exc);
    let npe = {
        let cm = shared.classes.class_manager.read();
        let npe = cm.loaded_class_under_exact_key(
            "java/lang/NullPointerException",
            cratonvm_types::ClassLoaderId::Bootstrap,
        )?;
        if class_id != npe && !cm.is_subclass_of(class_id, npe) {
            return None;
        }
        npe
    };
    let (extended_slot, state_slot) = npe_extended_message_slots(shared, npe)?;
    // Widening: u32 -> usize.
    let slots = heap.get_header(exc).num_slots() as usize;
    if extended_slot >= slots || state_slot >= slots {
        return None;
    }
    if !matches!(heap.get_field(exc, state_slot), Value::Int(2)) {
        return None;
    }
    match heap.get_field(exc, extended_slot) {
        Value::Object(Some(s)) if Some(s) != deferred_npe_message_mark(shared) => Some(s),
        _ => None,
    }
}

/// Mark the freshly built implicit NPE `exc` as carrying a deferred message:
/// its `extendedMessage` takes [`DEFERRED_NPE_MESSAGE_MARK`]. Only in the state
/// HotSpot's NPE is in after its constructor -- `extendedMessageState == 1`
/// (the `fillInStackTrace()` override ran once), no `extendedMessage`, no
/// `detailMessage` -- because only there does the JDK's code write
/// `extendedMessage` before it reads it (`getMessage()` and
/// `fillInStackTrace()` both call `getExtendedNPEMessage()` at state 1 and
/// store its answer), so the mark is never returned as a message. Nothing
/// here allocates.
fn mark_npe_message_deferred(shared: &SharedVm, exc: ObjectRef) -> bool {
    let heap = &shared.mem.heap;
    let Some((extended_slot, state_slot)) =
        npe_extended_message_slots(shared, heap.class_id_of(exc))
    else {
        return false;
    };
    if !matches!(heap.get_field(exc, state_slot), Value::Int(1))
        || !matches!(heap.get_field(exc, extended_slot), Value::Object(None))
        || !detail_message_is_null(shared, exc)
    {
        return false;
    }
    let Some(mark) = deferred_npe_message_mark(shared) else {
        return false;
    };
    heap.set_field(exc, extended_slot, Value::Object(Some(mark)));
    true
}

/// The deferred JEP 358 message of `throwable`, for the
/// `NullPointerException.getExtendedNPEMessage` native
/// (`NativeExceptionAccess::deferred_npe_message`): `Some(answer)` when
/// [`mark_npe_message_deferred`] marked it (and the JDK has not yet replaced
/// the mark), `answer` being `None` for "no message"; `None` when it is not a
/// deferred NPE, and the native answers as it always has.
pub(crate) fn deferred_npe_message(
    shared: &SharedVm,
    throwable: ObjectRef,
) -> Option<Option<String>> {
    let heap = &shared.mem.heap;
    let (extended_slot, _) = npe_extended_message_slots(shared, heap.class_id_of(throwable))?;
    let Value::Object(Some(held)) = heap.get_field(throwable, extended_slot) else {
        return None;
    };
    if held != deferred_npe_message_mark(shared)? {
        return None;
    }
    Some(deferred_npe_message_text(shared, throwable))
}

/// The message from the innermost entry of `throwable`'s stored trace
/// (`jit_npe_message::deferred_message_from_top_frame`).
fn deferred_npe_message_text(shared: &SharedVm, throwable: ObjectRef) -> Option<String> {
    let len = shared.throwable_stack_trace_len_for(throwable)?;
    let top = shared.throwable_stack_trace_entry_for(throwable, len.checked_sub(1)?)?;
    crate::runtime::interpreter::jit_npe_message::deferred_message_from_top_frame(shared, &top)
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
    // default; with fast throw off this is one latched read.
    //
    // Every other implicit exception the interpreter raises from its own
    // bytecode is either the re-execution of a bytecode a compiled body just
    // trapped on (a compiled throw) or an interpreted one, recorded in the
    // method profile for the hotness rule (`interpreter_implicit_exception`).
    if omit_stack_trace_in_fast_throw(shared) {
        if let Some(kind) = FastThrowKind::of_runtime_error(&error) {
            if in_resumed_compiled_frame(thread) {
                let site = interpreter_fast_throw_site(shared, thread, kind).map(|mut site| {
                    // The precise-resume took the stash too: claim its record
                    // so no later throw mistakes itself for this trap.
                    let taken = cratonvm_jit::deopt::take_trap_site_if(&site.label, site.bci);
                    site.c2_tier = resumed_compiled_frame_at_c2_trap(thread)
                        || taken
                            .flatten()
                            .is_some_and(|body| body_is_c2_equivalent(&body));
                    site
                });
                if let Some(exc) = try_fast_throw(shared, thread, kind, site.as_ref()) {
                    return MethodCallFailed::ExceptionThrown(exc);
                }
            } else if let Some(exc) = interpreter_implicit_exception(shared, thread, kind) {
                return MethodCallFailed::ExceptionThrown(exc);
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
    // `CRATONVM_INTERP_NPE_DEFERRED_MESSAGE` (interpreter round i4 wave 85,
    // lane L2): an interpreter null-dereference arm asked for its message to
    // be deferred (`invoke::interp_npe_message_deferred`).
    if let RuntimeError::NullPointerException { message: Some(m) } = &error {
        if m == cratonvm_types::error::INTERP_DEFERRED_NPE_MESSAGE {
            return throw_interpreted_npe_deferred(shared, thread);
        }
    }
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
            // gcf f5/o (vf3 o3-5: `GcfF3oOomeShapeProbe` died of an UNCATCHABLE
            // `OutOfMemoryError` on Generational and G1): building a VM-raised
            // OOME on a full heap can fail OTHERWISE than with the allocator's
            // OOM -- its constructor runs Java (`Throwable.<init>` ->
            // `fillInStackTrace`), and a native in there that cannot allocate
            // throws a Java `OutOfMemoryError` out of the constructor
            // (`Err(ExceptionThrown(..))`). That fell to the internal-error arm
            // below, which no Java `catch` sees: the error unwound every frame
            // and `run()` returned it with no stack. HotSpot never constructs
            // the OOMEs it raises, so its error is always catchable; ours must
            // be too -- whatever went wrong while building an OOME, throw the
            // preallocated one.
            let building_oome = matches!(&error, RuntimeError::OutOfMemoryError { .. });
            if is_oom || building_oome {
                if building_oome && !is_oom {
                    tracing::debug!(
                        target: "cratonvm::gc::guard",
                        "building a VM-raised OutOfMemoryError failed ({e:?}); throwing the \
                         preallocated one"
                    );
                }
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
    let immutable = preallocated_oome_immutable_enabled();
    if shared.mem.singleton_oom.read().is_none() {
        if let Ok(obj) = create_exception_object(
            shared,
            thread,
            "java/lang/OutOfMemoryError",
            Some("Java heap space"),
        ) {
            if immutable {
                make_preallocated_oome_immutable(shared, obj);
            }
            *shared.mem.singleton_oom.write() = Some(obj);
        }
    }
    // zs s9/la: HotSpot's traced pool, preallocated now (see
    // `zgc_traced_oome_pool_enabled`): each built as a VM-raised error is
    // (constructed, then `VM_OOME_CLEARED_FIELDS` cleared), stored (and so
    // rooted) before the next is built. Its boot trace is replaced when the
    // error is handed out (`pooled_traced_oome`).
    if zgc_traced_oome_pool_enabled(shared) {
        while shared.mem.preallocated_oome.read().traced_len()
            < HOTSPOT_PREALLOCATED_OOME_COUNT as usize
        {
            let Ok(obj) = create_exception_object(
                shared,
                thread,
                "java/lang/OutOfMemoryError",
                Some("Java heap space"),
            ) else {
                break;
            };
            let cleared: Vec<(&str, Value)> = VM_OOME_CLEARED_FIELDS
                .iter()
                .map(|name| (*name, Value::Object(None)))
                .collect();
            set_throwable_fields_by_name(shared, obj, &cleared);
            shared.mem.preallocated_oome.write().push_traced(obj);
        }
    }
    // gcg g1/o: the classes reading a caught OOME's trace needs, prepared
    // while the heap still has room (see `warm_oome_path_classes`).
    if oome_path_warm_enabled() {
        let _ = warm_oome_path_classes(shared, thread);
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
            if immutable {
                make_preallocated_oome_immutable(shared, obj);
            }
            shared.mem.preallocated_oome.write().set(kind, obj);
        }
    }
}

/// gcg g1/o (2026-10-01),
/// `gcf-f5o-a-vm-raised-oome-whose-construction-fails-escapes-every-catch`:
/// the classes [`warm_oome_path_classes`] loads, prepares and gives a mirror
/// at boot. `Throwable.getStackTrace()` -> `getOurStackTrace()` ->
/// `StackTraceElement.of(backtrace, depth)` is the first use of
/// `StackTraceElement` in most programs, and its preparation allocates the
/// class's two `ConstantValue` Strings (`NATIVE_METHOD`, `UNKNOWN_SOURCE`)
/// through `vm_util::prepare_class_shared`, which never collects. So the first
/// `e.getStackTrace()` after a program caught an `OutOfMemoryError` and
/// dropped its data threw `OutOfMemoryError: Java heap space (prepare_class
/// java/lang/StackTraceElement.NATIVE_METHOD ConstantValue String)` on all
/// three collectors (`GcfF7oOomeStageProbe`: `post-null-oome-at=getStackTrace`
/// with `big-alloc-after-gc=ok`). `Throwable.getSuppressed()`'s
/// `toArray(EMPTY_THROWABLE_ARRAY)` then needs the `Throwable[]` mirror, which
/// a native creates lazily with one non-collecting attempt (ZGC's door once the
/// first one is closed). HotSpot has neither door: `StackTraceElement` is a
/// `vmClasses` entry loaded at genesis, its constant Strings are created with
/// its mirror at load, and every loaded class (arrays included) has its mirror
/// from the start. `StackTraceElement` declares no `<clinit>`, so initializing
/// it here runs no Java.
///
/// gcg g1b/o: `java/lang/OutOfMemoryError`'s own mirror too (the class is
/// already loaded and initialized by the singleton's construction). vg1
/// `o1_jni_g1` died (rc 134) at `e.getClass()` on a caught OOME: G1's lazy
/// mirror allocation in a native is the INFALLIBLE one
/// (`vm_object::get_or_create_class_mirror` keeps G1 off the fallible arm),
/// so a full heap aborted the process with `FATAL: G1: out of heap space`.
const OOME_PATH_WARM_CLASSES: [&str; 4] = [
    "java/lang/StackTraceElement",
    "[Ljava/lang/StackTraceElement;",
    "[Ljava/lang/Throwable;",
    "java/lang/OutOfMemoryError",
];

/// gcg g1/o: `CRATONVM_GC_OOME_WARM_TRACE_CLASSES` (default ON;
/// `runtime_flag_default_on`, `=0` restores the lazy first use). Read once, at
/// boot, by [`ensure_singleton_oom`].
fn oome_path_warm_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_OOME_WARM_TRACE_CLASSES")
}

/// gcg g1/o: load, link and initialize each of [`OOME_PATH_WARM_CLASSES`]
/// (an array class is loaded only) and create its mirror, while the heap has
/// room. Best effort: a class that cannot be loaded or initialized is skipped
/// and keeps its lazy first use. Returns how many classes ended with a mirror.
fn warm_oome_path_classes(shared: &SharedVm, thread: &mut JvmThread) -> usize {
    let mut warmed = 0;
    for name in OOME_PATH_WARM_CLASSES {
        let Ok(class_id) = shared.load_class_concurrent(name) else {
            continue;
        };
        if !name.starts_with('[')
            && crate::vm::ensure_class_initialized_shared(shared, thread, class_id).is_err()
        {
            continue;
        }
        if crate::vm::try_get_or_create_class_mirror(shared, class_id).is_some() {
            warmed += 1;
        }
    }
    warmed
}

/// gcg g1/o: HotSpot's `PreallocatedOutOfMemoryErrorCount` (4): how many
/// VM-raised `OutOfMemoryError`s per VM get a stack trace of their own.
/// `Universe::gen_out_of_memory_error` hands out these pooled errors (each
/// with a fresh backtrace) and, once they are spent, the message's default --
/// no trace, no suppressed list, no cause -- for every later error.
const HOTSPOT_PREALLOCATED_OOME_COUNT: u32 = 4;

/// The budget arithmetic of [`hotspot_pool_spent_default`], pure over the
/// counter: takes one traced slot and answers `true` while any is left.
fn take_traced_oome_slot(used: &std::sync::atomic::AtomicU32) -> bool {
    take_traced_oome_slot_index(used).is_some()
}

/// [`take_traced_oome_slot`] answering WHICH slot it took (0-based), for the
/// traced pool (zs s9/la).
fn take_traced_oome_slot_index(used: &std::sync::atomic::AtomicU32) -> Option<u32> {
    use std::sync::atomic::Ordering;
    used.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        (n < HOTSPOT_PREALLOCATED_OOME_COUNT).then_some(n + 1)
    })
    .ok()
}

/// What [`hotspot_pool_answer`] decided for one VM-raised `OutOfMemoryError`.
enum OomePoolAnswer {
    /// HotSpot's traced pool is spent: the message's traceless default.
    Default(ObjectRef),
    /// One of HotSpot's traced slots (this call took it): a traced error.
    TracedSlot(u32),
    /// Not HotSpot's preallocated shape (switch off, or no default of its
    /// own): construct one.
    Construct,
}

/// [`hotspot_pool_spent_default`] with the traced slot it took, if any.
fn hotspot_pool_answer(shared: &SharedVm, message: &str) -> OomePoolAnswer {
    if !preallocated_oome_immutable_enabled() {
        return OomePoolAnswer::Construct;
    }
    let own_default = message == "Java heap space"
        || crate::vm::realms::heap_realm::OomeKind::for_message(message)
            .and_then(|kind| shared.mem.preallocated_oome.read().get(kind))
            .is_some();
    if !own_default {
        return OomePoolAnswer::Construct;
    }
    if let Some(index) = take_traced_oome_slot_index(&shared.mem.alloc_ladder.traced_oomes) {
        return OomePoolAnswer::TracedSlot(index);
    }
    match preallocated_oome_for(shared, message) {
        Some(default) => OomePoolAnswer::Default(default),
        None => OomePoolAnswer::Construct,
    }
}

/// zs s9/la: `CRATONVM_ZGC_OOME_TRACED_POOL` (default ON, `=0` kill): on the
/// VM's ZGC heap, with `CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE` on, the first
/// [`HOTSPOT_PREALLOCATED_OOME_COUNT`] VM-raised `Java heap space` errors are
/// preallocated at boot ([`ensure_singleton_oom`]) and handed out with a fresh
/// trace ([`pooled_traced_oome`]) instead of being constructed on the heap
/// that just refused an allocation. Generational and G1: never (unchanged).
///
/// Why: HotSpot's `Universe::gen_out_of_memory_error` takes these from
/// `_preallocated_out_of_memory_error_array`, so a refusal makes no garbage.
/// CratonVM built each one fresh (the error, its trace store) in the last holes
/// of the heap; once the program dropped it, the next collection put those
/// bytes back on the ZGC free list, and they served the allocations that came
/// next. That is how the L4 launcher probes (`Zs1NdLauncherOome`,
/// `Zs2Nd2LauncherArgs`) got their instance `main`'s receiver after a fill
/// HotSpot leaves with no room for it: HotSpot fails the receiver and prints
/// `Exception in thread "main" ` (27 bytes); CratonVM entered `main` and
/// printed the uncaught-handler line.
fn zgc_traced_oome_pool_enabled(shared: &SharedVm) -> bool {
    crate::vm::heap_is_zgc(shared)
        && preallocated_oome_immutable_enabled()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_OOME_TRACED_POOL")
}

/// zs s9/la: the `index`-th preallocated traced error with the current
/// thread's trace filled in place (the capture the direct throwable build
/// uses: `capture_throwable_stack_trace` + `finish_throwable_fill`, no Java
/// and no heap allocation for the trace, which lives in the VM's store).
/// `None` = not this message, the pool is off or that slot was never built:
/// the caller constructs one as before.
fn pooled_traced_oome(
    shared: &SharedVm,
    thread: &mut JvmThread,
    message: Option<&str>,
    index: u32,
) -> Option<ObjectRef> {
    use cratonvm_native_api::registry::{NativeExceptionAccess, NativeHeapAccess};
    if !zgc_traced_oome_pool_enabled(shared) {
        return None;
    }
    // zt t12/rt: only `Java heap space` takes a slot. zt t6/rv3's opt-in
    // `CRATONVM_ZGC_OOME_TRACED_POOL_ALL_KINDS` (another kind with a default
    // of its own took a slot too, with the default's message) was retired:
    // its unset arm was already SAME as HotSpot in both modes (t11 pr O1).
    if message != Some("Java heap space") {
        return None;
    }
    let index = index as usize;
    let obj = shared.mem.preallocated_oome.read().traced(index)?;
    let depth = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        ctx.capture_throwable_stack_trace(obj)
    };
    // The pool keeps the error rooted (and current) across the capture.
    let obj = shared.mem.preallocated_oome.read().traced(index)?;
    let depth = i32::try_from(depth).unwrap_or(i32::MAX);
    let filled = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        ctx.finish_throwable_fill(obj, depth)
    };
    let obj = shared.mem.preallocated_oome.read().traced(index)?;
    if filled.is_none() {
        set_throwable_fields_by_name(
            shared,
            obj,
            &[
                ("backtrace", Value::Object(Some(obj))),
                ("depth", Value::Int(depth)),
            ],
        );
    }
    Some(obj)
}

/// gcg g1/o, `gcf-f2o-preallocated-oome-defaults-carry-a-boot-stack-trace-and-mutable-state`
/// (why the `CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE` arm still differed from
/// HotSpot on Generational): under that switch, the VM-raised
/// `OutOfMemoryError` HotSpot would throw once its traced pool is spent -- the
/// message's preallocated default ([`preallocated_oome_for`]) -- instead of a
/// freshly constructed error with a trace. `None` = construct one as before:
/// the switch is off, one of the [`HOTSPOT_PREALLOCATED_OOME_COUNT`] traced
/// slots is left (this call takes it), or the message has no default of its
/// own (a VM-limit or `Metaspace` error without
/// `CRATONVM_GC_PREALLOCATED_OOME_KINDS` keeps its message by being built).
///
/// Generational fits a fresh error into the holes of a full heap where G1 and
/// ZGC cannot, so `GcfF7oOomeStageProbe`'s eight caught errors there were
/// partly fresh, traced and each with a suppressed list of its own
/// (`empty-trace-all=false`); HotSpot's are all the traceless default
/// (`empty-trace-all=true`, `suppressed-max=0`).
#[cfg(test)]
fn hotspot_pool_spent_default(shared: &SharedVm, message: &str) -> Option<ObjectRef> {
    match hotspot_pool_answer(shared, message) {
        OomePoolAnswer::Default(default) => Some(default),
        OomePoolAnswer::TracedSlot(_) | OomePoolAnswer::Construct => None,
    }
}

/// gcf f3/o (2026-10-01),
/// `gcf-f2o-preallocated-oome-defaults-carry-a-boot-stack-trace-and-mutable-state`:
/// `CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE` (default ON since gcg g1, vg1 `o1_*_pi_*`
/// SAME 12/12; `=0` restores the constructed shape; `runtime_flag_default_on`)
/// gives the preallocated `OutOfMemoryError`s ([`ensure_singleton_oom`]) the
/// shape HotSpot's `Universe` defaults have -- allocated without the
/// constructor, message only. Off, they keep the constructor's boot-thread
/// stack trace and a live suppressed list, shared by every throw.
fn preallocated_oome_immutable_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE")
}

/// The `Throwable` fields [`make_preallocated_oome_immutable`] clears, and
/// their HotSpot-default values: no trace (`backtrace == null` makes
/// `getOurStackTrace` answer the empty `UNASSIGNED_STACK`, `setStackTrace` a
/// no-op), no suppressed list (`addSuppressed` returns early), `cause == null`
/// (`getCause()` null; `initCause` throws, as on HotSpot's default).
const PREALLOCATED_OOME_CLEARED_FIELDS: [&str; 4] =
    ["backtrace", "stackTrace", "suppressedExceptions", "cause"];

/// Write HotSpot's preallocated-default shape onto `obj` by field name
/// (`detailMessage` kept; `depth` to `0`). A field this JDK does not declare is
/// skipped. Runs before `main`, once per preallocated object.
fn make_preallocated_oome_immutable(shared: &SharedVm, obj: ObjectRef) {
    let fields: Vec<(&str, Value)> = PREALLOCATED_OOME_CLEARED_FIELDS
        .iter()
        .map(|name| (*name, Value::Object(None)))
        .chain(std::iter::once(("depth", Value::Int(0))))
        .collect();
    set_throwable_fields_by_name(shared, obj, &fields);
}

/// gcf f3/o: the fields a FRESH VM-raised `OutOfMemoryError` clears under
/// `CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE` ([`create_vm_oome_object`]): it
/// keeps its stack trace (HotSpot's pooled copies get a fresh one) but, like
/// every `OutOfMemoryError` HotSpot itself raises (`gen_out_of_memory_error`
/// hands out instances allocated without the constructor), records no
/// suppressed exceptions and has a `null` cause.
const VM_OOME_CLEARED_FIELDS: [&str; 2] = ["suppressedExceptions", "cause"];

/// Write `fields` (name, value) onto `obj` by name through its class
/// hierarchy; a name this JDK does not declare is skipped.
fn set_throwable_fields_by_name(shared: &SharedVm, obj: ObjectRef, fields: &[(&str, Value)]) {
    let class_id = shared.mem.heap.class_id_of(obj);
    let indices: Vec<(usize, Value)> = {
        let cm = shared.classes.class_manager.read();
        fields
            .iter()
            .filter_map(|(name, value)| {
                crate::vm::vm_exec::resolve_field_index_in_hierarchy(
                    class_id,
                    name,
                    &cm.class_store,
                )
                .map(|i| (i, *value))
            })
            .collect()
    };
    for (i, value) in indices {
        shared.mem.heap.set_field(obj, i, value);
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
        hotspot_external_name(&internal_name)
    );
    raise_no_class_def_found(shared, thread, &message)
}

/// A class's name as HotSpot's `InstanceKlass::external_name()` (and
/// `Class.getName()`) spells it in a message: dotted, with a HIDDEN class's
/// `/0x<hex>` tail kept verbatim (`p.Foo/0x0000020001001400`, not
/// `p.Foo.0x0000020001001400`). Interpreter round i3 wave 79, lane L3
/// (`i71-L3-could-not-initialize-a-hidden-class-dots-its-0x-suffix-20261104.md`).
pub(crate) fn external_name_keeping_hidden_tail(internal_name: &str, hidden: bool) -> String {
    if hidden {
        if let Some((base, hex)) = internal_name.rsplit_once("/0x") {
            if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return format!("{}/0x{hex}", base.replace('/', "."));
            }
        }
    }
    internal_name.replace('/', ".")
}

/// Is `class_id` a hidden class (`Lookup.defineHiddenClass`, a lambda
/// proxy)? `None` when the store does not hold the id.
fn class_hidden_in_store(shared: &SharedVm, class_id: ClassId) -> Option<bool> {
    shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.hidden)
}

/// Why a class's initialization failed, as [`record_class_init_error`]
/// records it (`ClassRealm::class_init_errors`).
///
/// The message is text, and the trace is the original exception's frames as
/// built entries (names and bcis), not the compact `BacktraceFrame::Method`
/// form, so a row keeps no method metadata alive. Its one heap reference is
/// [`Self::saved_cause`] (round 21 wave 26), a strong global handle that the
/// unload sweep releases with the row (`memory::gc::unload_dead_class_metadata`,
/// JIT round 22 wave 1, lane review).
#[derive(Clone, Debug)]
pub struct RecordedClassInitError {
    /// `Exception <class>[: <message>] [in thread "<name>"]`.
    pub message: std::sync::Arc<str>,
    /// The original exception's stack trace, OUTERMOST-first, lines resolved;
    /// `None` when it had none in the VM trace store (a stackless throwable).
    pub trace: Option<std::sync::Arc<[crate::native::registry::StackTraceEntry]>>,
    /// The `ExceptionInInitializerError` the first "Could not initialize
    /// class" error was given as its cause, held by a strong global handle of
    /// `jni_global_refs` (HotSpot's `_initialization_error_table` keeps ONE
    /// such object per erroneous class as an `OopHandle`, and every later
    /// error carries that same object, JDK-8048190). `None` until the first
    /// raise built it, or with `CRATONVM_INIT_ERROR_SAVED_CAUSE=0`. JIT round
    /// 21 wave 26, lane reserr (`R21Fz10NcdfeSites1` row `User1 fresh boom
    /// ... sameCause=true`). Released when the row is replaced
    /// ([`record_class_init_error`]) or its class unloads (JIT round 22 wave 1,
    /// lane review: `memory::gc::unload_dead_class_metadata`, as HotSpot's
    /// `clean_initialization_error_table`); otherwise it lives as long as the
    /// VM (one small throwable per erroneous class).
    pub saved_cause: Option<crate::native::jni::JObject>,
}

/// `CRATONVM_INIT_ERROR_SAVED_CAUSE` -- correctness, default ON, `=0` restores
/// a fresh `ExceptionInInitializerError` cause per "Could not initialize
/// class" error ([`raise_could_not_initialize_class_for`]). JIT round 21 wave
/// 26, lane reserr. Read only when such an error is raised (cold).
pub(crate) fn init_error_saved_cause_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_INIT_ERROR_SAVED_CAUSE")
}

/// [`raise_could_not_initialize_class`] with HotSpot 21+'s cause
/// (JDK-8048190): when `class_id`'s failure was recorded by
/// [`record_class_init_error`], the `NoClassDefFoundError` carries an
/// `ExceptionInInitializerError` whose message names the original exception
/// and thread, `Exception java.lang.RuntimeException: boom [in thread "main"]`,
/// and whose stack trace is the original exception's.
///
/// HotSpot records ONE such error object and every later error carries it.
/// Since JIT round 21 wave 26 (lane reserr, `CRATONVM_INIT_ERROR_SAVED_CAUSE`)
/// so does this: the first raise builds it, with the recorded trace, and keeps
/// it under a strong global handle ([`RecordedClassInitError::saved_cause`]);
/// later raises hand out that object. With the switch off (or before the
/// first raise) a fresh one is built per throw, as before.
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
    // A hidden class keeps its `/0x<hex>` tail (HotSpot's `external_name()`).
    //
    // JIT round 21 wave 24, lane names (item 6 of
    // `r21-review-w23-findings-FIXED-20261020.md`): the store is asked once, and
    // only a class it HOLDS is judged by its flag. An id it does not hold (the
    // synthetic lambda-proxy ids, hidden on HotSpot) keeps the shape rule
    // below rather than being dotted whole as an ordinary class.
    let hidden = class_hidden_in_store(shared, class_id);
    let failed = if hidden == Some(true) {
        let message = format!(
            "Could not initialize class {}",
            external_name_keeping_hidden_tail(internal_name, true)
        );
        raise_no_class_def_found(shared, thread, &message)
    } else if hidden == Some(false)
        && hidden_name_text_by_class_enabled()
        && !internal_name.starts_with('[')
    {
        // An ordinary class is dotted whatever its name's shape
        // (`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`, wave 22, lane hidden-resid).
        let message = format!(
            "Could not initialize class {}",
            external_name_keeping_hidden_tail(internal_name, false)
        );
        raise_no_class_def_found(shared, thread, &message)
    } else {
        raise_could_not_initialize_class(shared, thread, internal_name)
    };
    let MethodCallFailed::ExceptionThrown(ncdfe) = failed else {
        return failed;
    };
    let Some(recorded) = recorded else {
        return MethodCallFailed::ExceptionThrown(ncdfe);
    };
    let keep_saved = init_error_saved_cause_enabled();
    if keep_saved {
        if let Some(handle) = recorded.saved_cause {
            let saved = shared.natives.jni_global_refs.lock().resolve(handle);
            if let Some(cause) = saved {
                // No allocation between the read and the store.
                set_cause_by_name(shared, ncdfe, cause);
                return MethodCallFailed::ExceptionThrown(ncdfe);
            }
        }
    }
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
        if keep_saved {
            // JIT round 22 wave 1, lane review: a raiser that lost the race
            // to save its cause hands out the WINNER's, so every error of the
            // class carries the one saved object, the first ones included.
            if let Some(winner) = save_init_error_cause(shared, class_id, cause) {
                set_cause_by_name(shared, ncdfe, winner);
            }
        }
    }
    MethodCallFailed::ExceptionThrown(ncdfe)
}

/// Keep `cause`, the `ExceptionInInitializerError` just given to the first
/// "Could not initialize class" error of `class_id`, as that class's saved
/// error ([`RecordedClassInitError::saved_cause`]). First wins: a racing
/// raiser's handle that lost, or a row that is gone, releases ours. Allocates
/// no heap object; the two locks are never held together.
///
/// Answers the object the row already saved when this raiser lost the race
/// (JIT round 22 wave 1, lane review: the loser's error then carries the
/// winner's cause, as on HotSpot, where the table holds one object from the
/// start), else `None`.
fn save_init_error_cause(
    shared: &SharedVm,
    class_id: ClassId,
    cause: ObjectRef,
) -> Option<ObjectRef> {
    let handle = shared.natives.jni_global_refs.lock().add(cause);
    let (kept, winner) = {
        let mut rows = shared.classes.class_init_errors.lock();
        match rows.get_mut(&class_id) {
            Some(row) if row.saved_cause.is_none() => {
                row.saved_cause = Some(handle);
                (true, None)
            }
            Some(row) => (false, row.saved_cause),
            None => (false, None),
        }
    };
    if kept {
        return None;
    }
    let mut refs = shared.natives.jni_global_refs.lock();
    let _ = refs.remove(handle);
    winner.and_then(|w| refs.resolve(w))
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
    if !backtrace_marks_our_capture(shared, throwable, heap.get_field(throwable, bt_idx)) {
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
        .map(|c| external_name_keeping_hidden_tail(&c.name, c.hidden))
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
    // A VM-raised NPE's JEP 358 text is not a detail message on HotSpot
    // (`NullPointerException.getExtendedNPEMessage` computes it; the field
    // stays null), so `create_initialization_error` names none for it
    // (`vm_helpful_npe_detail`; interpreter round i4 wave 98, lane L9).
    let message = string_field(
        exc,
        throwable_field_index_or_walk(shared, exc, "detailMessage"),
    )
    .filter(|m| !vm_helpful_npe_detail(shared, exc_class, m));
    // `JavaThread::name()`: the name of the JavaThread's `threadObj()`, which
    // for a MOUNTED virtual thread is its carrier, not the virtual thread
    // (`Thread.currentThread()`): HotSpot words an initialization failure in
    // a virtual thread `[in thread "ForkJoinPool-1-worker-1"]`. Interpreter
    // round i4 wave 98, lane L9 (`init_error_thread_obj`; probe
    // `tools/probes/interp/L9/L9W98InitErrorTexts.java`, row `vthread`).
    let thread_name = thread
        .java_thread_obj
        .map(|t| init_error_thread_obj(shared, t))
        .and_then(|t| string_field(t, instance_field_index_by_name(shared, t, "name")))
        .unwrap_or_else(|| thread.name.clone());
    let text = match message {
        Some(m) => format!("Exception {exc_name}: {m} [in thread \"{thread_name}\"]"),
        None => format!("Exception {exc_name} [in thread \"{thread_name}\"]"),
    };
    // Rust-side copies only (the trace store row is read, not moved).
    //
    // A throwable that captured nothing -- `writableStackTrace == false`, or
    // the "stackless exception" idiom, a `fillInStackTrace()` override that
    // returns `this` -- has no row and a null `backtrace`; HotSpot's
    // `getStackTrace()` answers it an EMPTY array, which the recorded error
    // then carries. Recorded as an empty trace, so the error does not keep the
    // construction-site trace of the `NoClassDefFoundError` that raises it
    // (interpreter round i4 wave 84, lane L2).
    let trace = shared
        .throwable_stack_trace_for(exc)
        .map(std::sync::Arc::<[crate::native::registry::StackTraceEntry]>::from)
        .or_else(|| {
            let slot = throwable_backtrace_slot(shared)?;
            // Widening: u32 -> usize.
            let in_bounds = slot < heap.get_header(exc).num_slots() as usize;
            let captured_nothing =
                in_bounds && matches!(heap.get_field(exc, slot), Value::Object(None));
            let empty: Vec<crate::native::registry::StackTraceEntry> = Vec::new();
            captured_nothing.then(|| std::sync::Arc::from(empty))
        });
    let replaced = shared.classes.class_init_errors.lock().insert(
        class_id,
        RecordedClassInitError {
            message: std::sync::Arc::from(text),
            trace,
            saved_cause: None,
        },
    );
    // A replaced row's saved error describes the old failure: release it
    // (not under the rows lock).
    if let Some(handle) = replaced.and_then(|row| row.saved_cause) {
        let _ = shared.natives.jni_global_refs.lock().remove(handle);
    }
}

/// Is `text`, the `detailMessage` of an instance of `class_id`, a JEP 358
/// message this VM stored there itself? HotSpot never stores one: an implicit
/// `NullPointerException` is built with a null `detailMessage` and its text is
/// computed by `getExtendedNPEMessage()` into the transient `extendedMessage`.
/// So the readers of the FIELD -- `java_lang_Throwable::message`, which
/// `create_initialization_error` uses for the `Exception <class>[: <message>]
/// [in thread ...]` text -- see none: an implicit NPE in a `<clinit>` is
/// recorded as `Exception java.lang.NullPointerException [in thread "main"]`.
/// CratonVM stores the text eagerly wherever the message is not deferred
/// (`--compatible` always), and natives that stand in for JDK bytecode raise
/// the JDK bytecode's JEP 358 text the same way. Recognised by the class (the
/// VM raises exactly `java.lang.NullPointerException`) and by the message
/// starting with one of HotSpot's `print_NPE_failed_action` phrases, which no
/// `NullPointerException(String)` the VM raises on a program's behalf starts
/// with. Interpreter round i4 wave 98, lane L9.
fn vm_helpful_npe_detail(shared: &SharedVm, class_id: ClassId, text: &str) -> bool {
    is_helpful_npe_action_text(text)
        && shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .is_some_and(|c| &*c.name == "java/lang/NullPointerException")
}

/// The thread object whose name [`record_class_init_error`] prints for the
/// thread `current` (the `java.lang.Thread` the JvmThread runs as): HotSpot's
/// `JavaThread::name()` reads `threadObj()`, the platform thread, so for a
/// `java.lang.VirtualThread` that is mounted (its `carrierThread` set, as the
/// JDK's `VirtualThread.mount()` sets it) the CARRIER answers. Anything else,
/// or a virtual thread with no carrier recorded, answers itself. No
/// allocation, no Java.
fn init_error_thread_obj(shared: &SharedVm, current: ObjectRef) -> ObjectRef {
    let heap = &shared.mem.heap;
    let is_virtual = shared
        .classes
        .class_manager
        .read()
        .get_class(heap.class_id_of(current))
        .is_some_and(|c| &*c.name == "java/lang/VirtualThread");
    if !is_virtual {
        return current;
    }
    let Some(slot) = instance_field_index_by_name(shared, current, "carrierThread") else {
        return current;
    };
    // Widening: u32 -> usize.
    if slot >= heap.get_header(current).num_slots() as usize {
        return current;
    }
    match heap.get_field(current, slot) {
        Value::Object(Some(carrier)) => carrier,
        _ => current,
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

pub(crate) fn linkage_throwable(error: &LinkageError) -> (&'static str, String) {
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
        // HotSpot's message is the bare class name in INTERNAL form:
        // `SystemDictionary::resolve_with_circularity_detection` /
        // `resolve_instance_class_or_null` throw
        // `THROW_MSG(ClassCircularityError, name->as_C_string())`, so a
        // packaged class reads `p/A`, not `p.A` (interpreter round i4 wave
        // 98, lane L5; `tools/probes/interp/L5/L5W98CircularityMessageName.java`).
        LinkageError::ClassCircularityError { class_name } => {
            ("java/lang/ClassCircularityError", class_name.clone())
        }
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
            // HotSpot's split-verifier text (header plus `Exception Details:`,
            // `cratonvm_classloading::verify_hotspot_text`; interpreter round
            // i5 wave 101, lane L7) already names the class and method in its
            // `Location:` line: it is the whole message, as on HotSpot.
            // libverify's text (below major 50: `(class: p/C, method: m
            // signature: ()V) <reason>`; interpreter round i5 wave 102, lane
            // L7) names them in its prefix: also whole.
            if message.contains(cratonvm_classloading::verify_hotspot_text::DETAILS_MARKER)
                || cratonvm_classloading::verify_old_text::is_old_verifier_message(message)
            {
                message.clone()
            } else {
                format!("{}.{}: {}", class_name, method_name, message)
            },
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
            LinkageError::class_format_error_text(class_name, message),
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
        // The definer's own diagnosis of `class_name` itself keeps its words:
        // a class file of another class (`p/A (wrong name: p/B)`,
        // `ClassManager::define_class_shared_with_options`), which a
        // load-time transformer's answer stages (interpreter round i1 wave
        // 48, lane L3; `tools/probes/interp/L3/L3W42TransformerBadBytes.java`,
        // row `load-wrong-name`: HotSpot's message, where only the class name
        // was kept). Any other message names the class alone, as before.
        MethodCallFailed::InternalError(VmError::Linkage(
            crate::error::LinkageError::NoClassDefFoundError {
                class_name: message,
            },
        )) => {
            let own_diagnosis = message
                .strip_prefix(class_name)
                .is_some_and(|rest| rest.starts_with(" (wrong name: "));
            if own_diagnosis {
                raise_no_class_def_found(shared, thread, &message)
            } else {
                raise_no_class_def_found(shared, thread, class_name)
            }
        }
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

    /// Interpreter round i3 wave 79, lane L3: a hidden class's `/0x<hex>`
    /// tail survives the dotting (HotSpot's `external_name()`); an ordinary
    /// class, or a hidden one without a hex tail, is dotted throughout.
    #[test]
    fn i79_external_name_keeps_a_hidden_classs_hex_tail() {
        assert_eq!(
            external_name_keeping_hidden_tail("p/q/Foo$Bad/0x0000020001001400", true),
            "p.q.Foo$Bad/0x0000020001001400"
        );
        assert_eq!(
            external_name_keeping_hidden_tail("p/q/Foo$Bad/0x1", false),
            "p.q.Foo$Bad.0x1"
        );
        assert_eq!(external_name_keeping_hidden_tail("p/Foo", true), "p.Foo");
        assert_eq!(
            external_name_keeping_hidden_tail("p/Foo/0xzz", true),
            "p.Foo.0xzz"
        );
        assert_eq!(
            external_name_keeping_hidden_tail("Foo/0x1f", true),
            "Foo/0x1f"
        );
    }

    /// Interpreter round i4 wave 84, lane L2: the `ArrayStoreException` funnel
    /// keeps a hidden element class's `/0x<hex>` tail, bare and as an array's
    /// bottom element, and leaves every ordinary name as before.
    #[test]
    fn i84_l2_array_store_message_keeps_a_hidden_classs_hex_tail() {
        assert_eq!(
            hotspot_external_name("p/Main$$Lambda/0x0000020001001400"),
            "p.Main$$Lambda/0x0000020001001400"
        );
        assert_eq!(
            hotspot_external_name("[Lp/Main$$Lambda/0x1a;"),
            "[Lp.Main$$Lambda/0x1a;"
        );
        assert_eq!(
            hotspot_external_name("java/lang/Integer"),
            "java.lang.Integer"
        );
        assert_eq!(
            hotspot_external_name("[Ljava/lang/Integer;"),
            "[Ljava.lang.Integer;"
        );
        assert_eq!(hotspot_external_name("[[I"), "[[I");
    }

    /// JIT round 21 wave 22, lane hidden-resid (item 7 of
    /// `r21-review-w15-native-findings-FIXED-20261020.md`,
    /// `CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`): with the class at hand its own
    /// hidden flag decides the `/0x<hex>` tail; an ordinary `p/0x1F` is
    /// dotted as `Klass::external_name()` dots it. `=0` and arrays keep the
    /// shape rule.
    #[test]
    fn a_held_class_is_named_by_its_hidden_flag_not_its_name_shape() {
        assert_eq!(hotspot_external_name_by_flag("p/0x1F", false, true), "p.0x1F");
        assert_eq!(hotspot_external_name_by_flag("p/0x1F", true, true), "p/0x1F");
        assert_eq!(
            hotspot_external_name_by_flag("p/Main$$Lambda/0x1a", true, true),
            "p.Main$$Lambda/0x1a"
        );
        assert_eq!(hotspot_external_name_by_flag("java/lang/Integer", false, true), "java.lang.Integer");
        // `=0`: the shape judgement, as before.
        assert_eq!(hotspot_external_name_by_flag("p/0x1F", false, false), "p/0x1F");
        // An array keeps the shape rule (its own flag is not its element's).
        assert_eq!(
            hotspot_external_name_by_flag("[Lp/Main$$Lambda/0x1a;", false, true),
            "[Lp.Main$$Lambda/0x1a;"
        );
    }

    /// JIT round 21 wave 24, lane names (item 5 of
    /// `r21-review-w23-findings-FIXED-20261020.md`): with the bottom class at hand
    /// its hidden flag decides an array's tail too. Failed before: the
    /// checkcast and `aastore` texts printed `[Lfz9/0x2A;` (shape) for an
    /// array of an ORDINARY class, where HotSpot prints `[Lfz9.0x2A;`.
    #[test]
    fn an_array_is_named_by_its_bottom_class_hidden_flag() {
        assert_eq!(hotspot_external_name_by_bottom_flag("[Lfz9/0x2A;", false), "[Lfz9.0x2A;");
        assert_eq!(hotspot_external_name_by_bottom_flag("[[Lfz9/0x2A;", true), "[[Lfz9/0x2A;");
        assert_eq!(
            hotspot_external_name_by_bottom_flag("[Lp/Main$$Lambda/0x1a;", true),
            "[Lp.Main$$Lambda/0x1a;"
        );
        assert_eq!(hotspot_external_name_by_bottom_flag("fz9/0x1F", false), "fz9.0x1F");
        assert_eq!(hotspot_external_name_by_bottom_flag("fz9/H/0x1F", true), "fz9.H/0x1F");
        assert_eq!(hotspot_external_name_by_bottom_flag("[[I", false), "[[I");
        // An unknown bottom keeps the site's own (shape) text.
        assert_eq!(
            external_name_by_bottom_class_or("[Lfz9/0x2A;", None, hotspot_external_name),
            hotspot_external_name("[Lfz9/0x2A;")
        );
        // The funnel's shape rewrite leaves both renderings unchanged.
        assert_eq!(hotspot_external_name("[Lfz9.H/0x2a;"), "[Lfz9.H/0x2a;");
        assert_eq!(hotspot_external_name("fz9.H/0x2a"), "fz9.H/0x2a");
    }

    /// JIT round 21 wave 25, lane hexname (item 5 of
    /// `r21-review-w23-findings-FIXED-20261020.md`): the compiled `checkcast`
    /// helper's own fallback (a bare `replace('/', ".")`) dotted a hidden
    /// bottom's tail (`[Lp.H.0x1a;`, HotSpot `[Lp.H/0x1a;`); by the bottom
    /// flag both kinds are right, and the funnel still finds the ordinary
    /// receiver's class by id (its dotted name spells the internal one), so
    /// the module/loader clause is unchanged.
    /// JIT round 21 wave 26, lane hexname (`CRATONVM_CAST_TEXT_BY_RESOLVED_CLASS`):
    /// a target id the store does not hold names nothing, so the site keeps
    /// its pool-name rendering; an absent operand id gives no array clause.
    #[test]
    fn an_unheld_cast_target_keeps_the_pool_name() {
        let vm = test_vm();
        let absent = ClassId::new(u32::MAX - 9);
        assert_eq!(resolved_hidden_cast_target_name(&vm.shared, absent, "p/Hd"), None);
        assert_eq!(
            cast_target_display(&vm.shared, Some(absent), "p/Hd", |n| n.replace('/', ".")),
            "p.Hd"
        );
        assert_eq!(cast_target_display(&vm.shared, None, "[Lp/Hd;", |n| n.replace('/', ".")), "[Lp.Hd;");
        assert!(klass_origin_by_id(&vm.shared, absent, "[Lp.Hd/0x1;", true).is_none());
        assert!(klass_origin_by_id(&vm.shared, absent, "[I", true).is_none());
    }

    #[test]
    fn the_compiled_cast_texts_are_named_by_the_bottom_class() {
        let bare = |d: &str| d.replace('/', ".");
        assert_eq!(bare("[Lp/H/0x1a;"), "[Lp.H.0x1a;");
        assert_eq!(hotspot_external_name_by_bottom_flag("[Lp/H/0x1a;", true), "[Lp.H/0x1a;");
        assert_eq!(hotspot_external_name_by_bottom_flag("[Lfz9/0x2A;", false), "[Lfz9.0x2A;");
        assert_eq!(hotspot_external_name_by_bottom_flag("p/H/0x1a", true), "p.H/0x1a");
        assert_eq!(external_name_by_bottom_class_or("p/H/0x1a", None, bare), "p.H.0x1a");
        assert!(internal_name_spells_display("fz9/0x2A", "fz9.0x2A"));
        assert_eq!(
            split_cast_operands("class fz9.0x2A cannot be cast to class java.lang.String"),
            Some(("fz9.0x2A", "java.lang.String"))
        );
    }

    /// Wave 24, lane names (item 6 of `r21-review-w23-findings-FIXED-20261020.md`):
    /// an id the store does not hold answers `None`, which keeps the shape
    /// rule in `raise_could_not_initialize_class_for` (it used to read as
    /// "ordinary" and dot `p/Main$$Lambda/0x1a` whole), and no bottom flag.
    #[test]
    fn an_id_the_store_does_not_hold_has_no_hidden_answer() {
        let vm = test_vm();
        let absent = ClassId::new(u32::MAX - 7);
        assert_eq!(class_hidden_in_store(&vm.shared, absent), None);
        assert_eq!(bottom_class_hidden(&vm.shared, absent, "p/Main$$Lambda/0x1a"), None);
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

    /// Round 15 wave 5 (lane throw1): the lock-free pair table behind
    /// `cast_refusal_forensics_admitted` admits each key once, probes past a
    /// taken slot, and admits nothing new once the cap is recorded. A local
    /// table, so the process-wide one is not touched.
    #[test]
    fn cast_forensics_table_admits_each_key_once_and_stops_at_the_cap() {
        let seen = Box::new(CastForensicsSeen::new());
        assert!(seen.first_sighting(7));
        assert!(!seen.first_sighting(7), "a repeat is not admitted");
        // Widening: the slot count fits u64.
        let same_slot = 7 + CAST_FORENSICS_SLOTS as u64;
        assert!(
            seen.first_sighting(same_slot),
            "a key on a taken slot probes on"
        );
        assert!(!seen.first_sighting(same_slot));
        for k in 0..(CAST_FORENSICS_PAIR_CAP as u64) {
            let _ = seen.first_sighting(1_000_000 + k);
        }
        assert!(
            !seen.first_sighting(u64::MAX - 3),
            "past the cap nothing new is admitted"
        );
        assert!(!seen.first_sighting(7));
        assert_ne!(cast_forensics_key(1, "a/B"), 0);
        assert_eq!(
            cast_forensics_key(5, "java/lang/Integer"),
            cast_forensics_key(5, "java.lang.Integer"),
            "the slashed and dotted spellings are one pair"
        );
        assert_ne!(
            cast_forensics_key(5, "java.lang.Integer"),
            cast_forensics_key(6, "java.lang.Integer")
        );
    }

    /// Round 15 wave 5 (lane throw1): a VM-built throwable whose constructor
    /// is plain bytecode reaches the ordinary tail on its first, by-name
    /// construction, which fills the callback memo for exactly
    /// `(class, "<init>", descriptor)`; the second construction is served from
    /// that memo and still RUNS the constructor's bytecode. The class's
    /// `<init>(String)` is `aconst_null; athrow`, so "the constructor ran" is
    /// observable: JNI `ThrowNew` answers the `NullPointerException` it threw.
    /// (The JDK throwables of the test VM are `--compatible` shadows, natives
    /// the memo never records, which is why a test class is used.)
    #[test]
    fn a_vm_built_throwable_constructor_is_served_from_the_callback_memo() {
        use crate::runtime::native_callee_memo;
        const NAME: &str = "cratonvm/test/Throw1CtorMemo";
        const DESC: &str = "(Ljava/lang/String;)V";
        if !throw1_switches().ctor_memo || !crate::runtime::env_cache::native_callback_memo() {
            // A kill switch is set in this process: nothing to prove.
            return;
        }
        let mut vm = test_vm();
        // Stripped VM: without a buildable NPE the constructor's `athrow`
        // cannot produce the exception the second half observes.
        let Ok(probe) = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/NullPointerException",
            None,
        ) else {
            return;
        };
        let npe_class = vm.shared.mem.heap.class_id_of(probe);
        let ctor = jni_test_ctor(DESC, 2, vec![0x01, 0xbf]);
        let class_id = add_jni_throw_new_test_class(&vm, NAME, vec![ctor], true);
        assert_eq!(
            native_callee_memo::lookup(&vm.shared, class_id, "<init>", DESC),
            None,
            "nothing is memoised before the first construction"
        );
        // Interpreter round i4 wave 96 (lane L7): the memo's default stamp
        // carries the PROCESS-wide `class_origin_epoch` (and a store-epoch
        // slot test VMs may share), which every concurrently running test
        // that defines or promotes a class moves (wave 95 added several:
        // the narrow class-memo promotion tests, the define-census scope).
        // A bump between the fill and the probe retires the entry on
        // purpose, so in the full lib run the first probe could miss
        // (measured: alone -> ok, full run -> None). A miss re-arms and
        // refills on the next by-name construction, so retry until a quiet
        // window; a fill that is really skipped never hits.
        let mut filled = false;
        for _ in 0..64 {
            let first = create_exception_object_for_jni_throw_new(
                &vm.shared,
                &mut vm.main_thread,
                class_id,
                NAME,
                Some("m"),
            )
            .expect("the constructor's exception is a result");
            assert_eq!(vm.shared.mem.heap.class_id_of(first), npe_class);
            let memo = native_callee_memo::lookup(&vm.shared, class_id, "<init>", DESC);
            if memo == Some(class_id) {
                filled = true;
                break;
            }
        }
        assert!(
            filled,
            "the by-name construction reached the ordinary tail and filled the memo"
        );
        assert_eq!(
            native_callee_memo::lookup(&vm.shared, class_id, "<init>", "()V"),
            None,
            "only the descriptor that ran is memoised"
        );
        let pins = vm.main_thread.native_pin_roots.len();
        let second = create_exception_object_for_jni_throw_new(
            &vm.shared,
            &mut vm.main_thread,
            class_id,
            NAME,
            Some("m"),
        )
        .expect("the constructor's exception is a result");
        assert_eq!(
            vm.shared.mem.heap.class_id_of(second),
            npe_class,
            "the memoised construction ran the constructor's bytecode"
        );
        assert_eq!(vm.main_thread.native_pin_roots.len(), pins, "pins released");
    }

    // -----------------------------------------------------------------------
    // `klass_origin`'s descriptor parse (W7-37 §B8.1 / §B11.1)
    // -----------------------------------------------------------------------

    /// The operand spellings are HotSpot's, taken verbatim from a Temurin
    /// 25.0.3.9 run of the `CastMsgs` probe (see
    /// `docs/feature-designs/jdk-only-mode.md`
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
            (1, (true, true, true, None)),        // java/lang/Throwable
            (2, (false, true, false, Some(1))),   // java/lang/RuntimeException
            (3, (false, true, true, Some(2))),    // java/lang/NullPointerException
            (4, (false, false, true, Some(2))),   // app Quiet extends RuntimeException
            (5, (false, false, false, Some(4))),  // app LoudQuiet extends Quiet
            (6, (false, false, false, Some(2))),  // app Plain extends RuntimeException
            (7, (false, false, false, Some(3))),  // app MyNpe extends NullPointerException
            (8, (false, false, false, Some(99))), // superclass not in the store
        ]);
        let walk = |start: u32| {
            first_application_fill_override(ClassId::new(start), |cid| {
                let &(is_throwable, built_in, declares_fill, sup) = classes.get(&cid.as_u32())?;
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
        assert!(
            !walk(1) && !walk(2) && !walk(6),
            "no override: the backstop stays"
        );
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
        let read = || RESUMED_COMPILED_FRAME_DEPTH.with(|d| d.get().0);
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

    /// Off (`-XX:-OmitStackTraceInFastThrow`, per VM), the door never
    /// answers, however hot the site: every throw is built in full.
    #[test]
    fn fast_throw_door_is_closed_when_the_flag_is_off() {
        let vm = Vm::new(VmConfig {
            omit_stack_trace_in_fast_throw: Some(false),
            ..VmConfig::default()
        });
        assert!(!omit_stack_trace_in_fast_throw(&vm.shared));
        for _ in 0..(FAST_THROW_HOT_THRESHOLD * 2) {
            assert!(
                try_fast_throw(&vm.shared, &vm.main_thread, FastThrowKind::Arithmetic, None)
                    .is_none()
            );
        }
    }

    /// nio21 wave 16, lane L5: absent `-XX:±OmitStackTraceInFastThrow`, the
    /// door follows the switch, which defaults ON (HotSpot's default); an
    /// explicit `-XX:+` opens it whatever the switch says.
    #[test]
    fn nio21_w16_l5_fast_throw_is_on_by_default_and_per_vm() {
        let default_vm = test_vm();
        assert_eq!(
            omit_stack_trace_in_fast_throw(&default_vm.shared),
            cratonvm_types::flags::runtime_flag_on("CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW")
        );
        let on = Vm::new(VmConfig {
            omit_stack_trace_in_fast_throw: Some(true),
            ..VmConfig::default()
        });
        let off = Vm::new(VmConfig {
            omit_stack_trace_in_fast_throw: Some(false),
            ..VmConfig::default()
        });
        assert!(omit_stack_trace_in_fast_throw(&on.shared));
        assert!(
            !omit_stack_trace_in_fast_throw(&off.shared),
            "a second VM keeps its own answer"
        );
        // The peek never fires for a thread with no site, nor before this VM's
        // instance of the kind exists.
        assert!(!fast_throw_will_fire(
            &on.shared,
            &on.main_thread,
            FastThrowKind::Arithmetic
        ));
        assert!(!fast_throw_will_fire(
            &off.shared,
            &off.main_thread,
            FastThrowKind::Arithmetic
        ));
    }

    /// nio21 wave 16, lane L5: the fast throw hands out ONE bare instance per
    /// kind per VM, as HotSpot's `Universe::_*_exception` -- no constructor
    /// (`cause` null, not `this`), no stored trace, and its `detailMessage`
    /// cleared on every hand-out.
    #[test]
    fn nio21_w16_l5_preallocated_fast_throw_instance_is_one_bare_object_per_kind() {
        let vm = test_vm();
        let Some(a) = preallocated_fast_throw_instance(&vm.shared, FastThrowKind::Arithmetic)
        else {
            // Stripped VM (no JDK on the test classpath), or a class that
            // cannot be instantiated without Java: the full build is used.
            return;
        };
        let b = preallocated_fast_throw_instance(&vm.shared, FastThrowKind::Arithmetic)
            .expect("built once, then reused");
        assert_eq!(a, b, "the same object every time, as on HotSpot");
        if let Some(n) = preallocated_fast_throw_instance(&vm.shared, FastThrowKind::NullPointer) {
            assert_ne!(n, a, "one instance per kind");
        }
        let cause = throwable_field_index_or_walk(&vm.shared, a, "cause")
            .expect("ArithmeticException inherits Throwable.cause");
        assert_eq!(
            vm.shared.mem.heap.get_field(a, cause),
            Value::Object(None),
            "no constructor ran: initCause must refuse, as on HotSpot"
        );
        assert!(vm.shared.throwable_stack_trace_for(a).is_none());
        let msg = throwable_field_index_or_walk(&vm.shared, a, "detailMessage")
            .expect("Throwable.detailMessage");
        // Anything written into the shared object's message (HotSpot's comment:
        // "Weblogic sometimes mutates the detail message") is gone at the next
        // hand-out.
        vm.shared.mem.heap.set_field(a, msg, Value::Object(Some(a)));
        let c = preallocated_fast_throw_instance(&vm.shared, FastThrowKind::Arithmetic)
            .expect("still built");
        assert_eq!(c, a);
        assert_eq!(vm.shared.mem.heap.get_field(a, msg), Value::Object(None));
    }

    /// L5W17-5 (JIT round 20 wave 11, lane pNat): the unlocked hand-out
    /// answers exactly what the locked `resolve` answers for the published
    /// handle, clears `detailMessage` the same way, and `=0` keeps the locked
    /// path; both arms hand out one object per kind.
    #[test]
    fn r20_pnat_unlocked_fast_throw_handout_matches_the_locked_resolve() {
        for arm in [None, Some("0")] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_FAST_THROW_UNLOCKED_HANDOUT", arm)],
                || {
                    let vm = test_vm();
                    let kind = FastThrowKind::Arithmetic;
                    let Some(a) = preallocated_fast_throw_instance(&vm.shared, kind) else {
                        return;
                    };
                    use std::sync::atomic::Ordering;
                    let cache = &vm.shared.threads.throwable_layout_cache;
                    assert_eq!(cache.fast_throw_unlocked_handout_enabled(), arm.is_none());
                    let handle = cache.fast_throw_handles[kind.slot()].load(Ordering::Acquire);
                    let locked = vm.shared.natives.jni_global_refs.lock().resolve(handle);
                    assert_eq!(locked, Some(a));
                    let msg = throwable_field_index_or_walk(&vm.shared, a, "detailMessage")
                        .expect("Throwable.detailMessage");
                    vm.shared.mem.heap.set_field(a, msg, Value::Object(Some(a)));
                    let b = preallocated_fast_throw_instance(&vm.shared, kind).expect("reused");
                    assert_eq!(b, a);
                    assert_eq!(vm.shared.mem.heap.get_field(a, msg), Value::Object(None));
                },
            );
        }
    }

    /// Fast-throw hotness page (2026-10-10): a compiled site is hot by the
    /// profile rule only when its body stands for C2 code AND an implicit
    /// exception of that kind is already recorded at that method and bci. The
    /// first C2 throw at an unrecorded site is full and records it, as C2's
    /// uncommon trap would; a single-pass (C1) body never throws fast by the
    /// rule. The count fallback is switched off so only the rule answers.
    #[test]
    fn fast_throw_profile_rule_needs_a_c2_body_and_a_recorded_trap() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_FAST_THROW_COUNT_FALLBACK", Some("0"))],
            || {
                let vm = Vm::new(VmConfig {
                    omit_stack_trace_in_fast_throw: Some(true),
                    ..VmConfig::default()
                });
                if preallocated_fast_throw_instance(&vm.shared, FastThrowKind::Arithmetic).is_none()
                {
                    // Stripped VM: no instance can be built, the door stays shut.
                    return;
                }
                let kind = FastThrowKind::Arithmetic;
                let label: Arc<str> = Arc::from("p/Probe.div:(II)I");
                let c1 = FastThrowSite {
                    label: Arc::clone(&label),
                    bci: 2,
                    c2_tier: false,
                };
                let c2 = FastThrowSite {
                    label: Arc::clone(&label),
                    bci: 2,
                    c2_tier: true,
                };
                let other_bci = FastThrowSite {
                    label: Arc::clone(&label),
                    bci: 9,
                    c2_tier: true,
                };
                let throw = |site: &FastThrowSite| {
                    try_fast_throw(&vm.shared, &vm.main_thread, kind, Some(site))
                };
                assert!(
                    throw(&c2).is_none(),
                    "an unrecorded site throws in full first"
                );
                assert!(
                    throw(&c2).is_some(),
                    "then the recorded site is hot in a C2 body"
                );
                assert!(
                    throw(&c1).is_none(),
                    "a single-pass body below the tier-4 count never is"
                );
                assert!(throw(&other_bci).is_none(), "the record is per bci");
                assert!(
                    try_fast_throw(
                        &vm.shared,
                        &vm.main_thread,
                        FastThrowKind::NullPointer,
                        Some(&c2)
                    )
                    .is_none(),
                    "and per kind"
                );
                assert!(
                    try_fast_throw(&vm.shared, &vm.main_thread, kind, None).is_none(),
                    "a site the rule cannot name is the count fallback's, switched off here"
                );
                let off = Vm::new(VmConfig {
                    omit_stack_trace_in_fast_throw: Some(false),
                    ..VmConfig::default()
                });
                off.shared
                    .threads
                    .throwable_layout_cache
                    .fast_throw_traps
                    .note(&label, 2, kind);
                assert!(
                    try_fast_throw(&off.shared, &off.main_thread, kind, Some(&c2)).is_none(),
                    "with the flag off nothing is ever fast"
                );
                assert!(
                    !vm.shared
                        .threads
                        .throwable_layout_cache
                        .fast_throw_traps
                        .has_trap_at(&Arc::from("p/Probe.div:(II)I"), 3, kind),
                    "the profile is per VM and per bci"
                );
            },
        );
    }

    /// The interpreter records a trap only at a bytecode that raises that
    /// kind implicitly (HotSpot's `note_trap` callers), so an exception a
    /// native raises while its caller stands at an unrelated bytecode is not
    /// mistaken for one.
    #[test]
    fn fast_throw_trap_opcodes_are_the_implicit_raisers() {
        use FastThrowKind as K;
        // iaload / aastore / getfield / invokevirtual / arraylength / athrow / monitorenter.
        for op in [0x2e, 0x53, 0xb4, 0xb6, 0xbe, 0xbf, 0xc2] {
            assert!(opcode_raises_implicitly(op, K::NullPointer), "{op:#x}");
        }
        // invokestatic and new never raise an implicit NPE.
        assert!(!opcode_raises_implicitly(0xb8, K::NullPointer));
        assert!(!opcode_raises_implicitly(0xbb, K::NullPointer));
        assert!(opcode_raises_implicitly(0x33, K::ArrayIndexOutOfBounds));
        assert!(!opcode_raises_implicitly(0xbe, K::ArrayIndexOutOfBounds));
        for op in [0x6c, 0x6d, 0x70, 0x71] {
            assert!(opcode_raises_implicitly(op, K::Arithmetic), "{op:#x}");
        }
        // fdiv / ddiv divide by zero without throwing.
        assert!(!opcode_raises_implicitly(0x6e, K::Arithmetic));
        assert!(opcode_raises_implicitly(0xc0, K::ClassCast));
        assert!(
            !opcode_raises_implicitly(0xc1, K::ClassCast),
            "instanceof never throws"
        );
        assert!(opcode_raises_implicitly(0x53, K::ArrayStore));
        assert!(
            !opcode_raises_implicitly(0x4f, K::ArrayStore),
            "iastore stores no reference"
        );
    }

    /// The trap profile is bounded: filling it clears it, and a site then has
    /// to throw again before it can be hot.
    #[test]
    fn fast_throw_trap_profile_is_bounded() {
        let profile = FastThrowTrapProfile::default();
        let label: Arc<str> = Arc::from("p/Probe.m:()V");
        profile.note(&label, 0, FastThrowKind::NullPointer);
        assert!(profile.has_trap_at(&label, 0, FastThrowKind::NullPointer));
        for bci in 1..FAST_THROW_TRAP_PROFILE_CAP as u32 {
            profile.note(&label, bci, FastThrowKind::NullPointer);
        }
        assert!(
            profile.has_trap_at(&label, 0, FastThrowKind::NullPointer),
            "at the cap, kept"
        );
        profile.note(&label, u32::MAX, FastThrowKind::NullPointer);
        assert!(
            !profile.has_trap_at(&label, 0, FastThrowKind::NullPointer),
            "past it, cleared"
        );
        assert!(profile.has_trap_at(&label, u32::MAX, FastThrowKind::NullPointer));
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
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
        // JIT round 21 wave 26, lane reserr (`CRATONVM_INIT_ERROR_SAVED_CAUSE`):
        // a later error is a NEW `NoClassDefFoundError` carrying the SAME saved
        // `ExceptionInInitializerError` (HotSpot's init-error table).
        let first_cause = heap.get_field(ncdfe, cause_idx);
        let MethodCallFailed::ExceptionThrown(again) = raise_could_not_initialize_class_for(
            &vm.shared,
            &mut vm.main_thread,
            failed_class,
            "p/Failed",
        ) else {
            panic!("the second raise builds its error");
        };
        let heap = &vm.shared.mem.heap;
        let cause = vm
            .shared
            .classes
            .class_init_errors
            .lock()
            .get(&failed_class)
            .and_then(|row| row.saved_cause)
            .and_then(|h| vm.shared.natives.jni_global_refs.lock().resolve(h))
            .expect("the first raise saved its cause");
        assert_ne!(again, ncdfe, "each raise is a new error");
        assert_eq!(first_cause, Value::Object(Some(cause)), "the saved cause is the first one");
        assert_eq!(
            heap.get_field(again, cause_idx),
            Value::Object(Some(cause)),
            "the later error carries the saved cause"
        );
        // JIT round 22 wave 1, lane review: a raiser that lost the race to
        // save its own cause gets the winner's object back (its own handle is
        // released), and a class with no row saves nothing.
        assert_eq!(
            save_init_error_cause(&vm.shared, failed_class, again),
            Some(cause),
            "the loser hands out the saved cause"
        );
        assert_eq!(
            vm.shared
                .classes
                .class_init_errors
                .lock()
                .get(&failed_class)
                .and_then(|row| row.saved_cause)
                .and_then(|h| vm.shared.natives.jni_global_refs.lock().resolve(h)),
            Some(cause),
            "the saved row is unchanged"
        );
        assert_eq!(save_init_error_cause(&vm.shared, ClassId::new(0x7fff_0001), again), None);
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

    /// The verifier's final-override refusal is HotSpot's
    /// `IncompatibleClassChangeError`, its text verbatim (no prefix).
    #[test]
    fn final_override_refusal_is_an_unprefixed_icce() {
        let hs = "class p.C overrides final method java.lang.Object.getClass()Ljava/lang/Class;";
        let (class, msg) =
            linkage_throwable(&LinkageError::IncompatibleClassChangeError { message: hs.into() });
        assert_eq!(class, "java/lang/IncompatibleClassChangeError");
        assert_eq!(msg, hs);
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
        resolver.indy.insert(7, "(I)Ljava/lang/String;".to_string());
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
            text(&helpful_npe::null_expr_at_depth(
                &code, invoke_bci, 0, &resolver
            )),
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

    /// The gated wrapper follows the VM's own
    /// `-XX:±ShowCodeDetailsInExceptionMessages` (interpreter round i4 wave
    /// 84, lane L2): no message when it is off, HotSpot's action text when it
    /// is on. The process-wide override and `CRATONVM_HELPFUL_NPE_OPCODES`
    /// outrank the configuration, so each arm is asserted only when the
    /// effective answer is the configuration's (other tests set the override).
    #[test]
    fn jit_npe_message_gated_follows_the_vms_flag() {
        for on in [false, true] {
            let mut config = crate::config::VmConfig::default();
            config.show_code_details_in_exception_messages = on;
            let message =
                helpful_npe::jit_npe_message_gated(&config, helpful_npe::jit_action::ALOAD_INT);
            if crate::runtime::env_cache::show_code_details(&config) {
                assert_eq!(message.as_deref(), Some("Cannot load from int array"));
            } else {
                assert_eq!(message, None);
            }
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
        let vm_oome = body_of(src, "fn create_vm_oome_object_with_array_retry(");
        assert!(vm_oome.contains("create_exception_object_no_collect("));
        // gcf f3/o, f4/o: the retry is an arm of `built`, then shaped
        // (`vm_oome_hotspot_shape`).
        let retry = vm_oome
            .find("create_exception_object(shared, thread, class_name, message)")
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

    /// gcf f3/o: the immutable shape (default ON since gcg g1; `=0` turns it
    /// off) clears exactly the four `Throwable` fields HotSpot's
    /// `allocate_instance` leaves null (the message is kept).
    #[test]
    fn gcf_f3o_preallocated_oome_immutable_shape_is_opt_in() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE", None)],
            || assert!(preallocated_oome_immutable_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE", Some("0"))],
            || assert!(!preallocated_oome_immutable_enabled()),
        );
        assert_eq!(
            PREALLOCATED_OOME_CLEARED_FIELDS,
            ["backtrace", "stackTrace", "suppressedExceptions", "cause"]
        );
        assert!(!PREALLOCATED_OOME_CLEARED_FIELDS.contains(&"detailMessage"));
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
        shared
            .mem
            .preallocated_oome
            .write()
            .set(OomeKind::ArraySize, own);
        assert_eq!(preallocated_oome_for(&shared, limit), Some(own));
        assert_eq!(
            preallocated_oome_for(&shared, "Java heap space"),
            Some(singleton)
        );
        assert_eq!(preallocated_oome_for(&shared, "Metaspace"), Some(singleton));
        // The fakes must not outlive the test inside a VM a collector could scan.
        *shared.mem.singleton_oom.write() = None;
        *shared.mem.preallocated_oome.write() = Default::default();
    }
}

#[cfg(test)]
mod gcg_g1o_oome_path_tests {
    //! gcg g1/o: the boot warm-up of the classes a caught OOME's trace needs,
    //! and HotSpot's traced pool under the immutable-shape switch.
    use super::*;
    use crate::config::VmConfig;
    use crate::vm::realms::heap_realm::OomeKind;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: non-null, 8-byte aligned, never dereferenced.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// Four traced errors per VM, then none, and the counter stops at four.
    #[test]
    fn gcg_g1o_the_traced_pool_holds_hotspots_four() {
        let used = AtomicU32::new(0);
        for _ in 0..HOTSPOT_PREALLOCATED_OOME_COUNT {
            assert!(take_traced_oome_slot(&used));
        }
        assert!(!take_traced_oome_slot(&used));
        assert!(!take_traced_oome_slot(&used));
        assert_eq!(
            used.load(Ordering::Relaxed),
            HOTSPOT_PREALLOCATED_OOME_COUNT
        );
    }

    /// Off, every VM-raised OOME is built (no slot is taken). On, the first
    /// four heap OOMEs are built and the fifth is the traceless default; a
    /// message with no default of its own is always built and takes no slot.
    #[test]
    fn gcg_g1o_a_spent_pool_hands_out_the_default_only_under_the_switch() {
        let shared = SharedVm::new(VmConfig::default());
        let singleton = fake(0x7000);
        *shared.mem.singleton_oom.write() = Some(singleton);
        let heap = "Java heap space";
        let limit = OomeKind::ArraySize.message();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE", Some("0"))],
            || {
                for _ in 0..8 {
                    assert_eq!(hotspot_pool_spent_default(&shared, heap), None);
                }
            },
        );
        assert_eq!(
            shared.mem.alloc_ladder.traced_oomes.load(Ordering::Relaxed),
            0,
            "off: no slot is taken"
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE", Some("1"))],
            || {
                assert_eq!(hotspot_pool_spent_default(&shared, limit), None);
                for _ in 0..HOTSPOT_PREALLOCATED_OOME_COUNT {
                    assert_eq!(hotspot_pool_spent_default(&shared, heap), None);
                }
                assert_eq!(hotspot_pool_spent_default(&shared, heap), Some(singleton));
                assert_eq!(hotspot_pool_spent_default(&shared, heap), Some(singleton));
                // Still built: its message has no preallocated default here.
                assert_eq!(hotspot_pool_spent_default(&shared, limit), None);
            },
        );
        // The fake must not outlive the test inside a VM a collector could scan.
        *shared.mem.singleton_oom.write() = None;
    }

    /// The warm-up is on by default, `=0` turns it off, it names the class
    /// whose preparation was the door, and `ensure_singleton_oom` runs it
    /// whether or not the per-message defaults are built.
    #[test]
    fn gcg_g1o_the_oome_path_classes_are_warmed_at_boot_by_default() {
        assert_eq!(OOME_PATH_WARM_CLASSES[0], "java/lang/StackTraceElement");
        assert!(OOME_PATH_WARM_CLASSES.contains(&"[Ljava/lang/Throwable;"));
        assert!(OOME_PATH_WARM_CLASSES.contains(&"java/lang/OutOfMemoryError"));
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_OOME_WARM_TRACE_CLASSES", None)],
            || assert!(oome_path_warm_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_OOME_WARM_TRACE_CLASSES", Some("0"))],
            || assert!(!oome_path_warm_enabled()),
        );
        let src = include_str!("exceptions.rs");
        let start = src
            .find("pub fn ensure_singleton_oom(")
            .expect("ensure_singleton_oom");
        let body = &src[start..];
        let warm = body
            .find("warm_oome_path_classes(shared, thread)")
            .expect("the warm-up call");
        let kinds = body
            .find("gc.preallocated_oome_kinds")
            .expect("the per-message defaults gate");
        assert!(
            warm < kinds,
            "the warm-up must not depend on the kinds switch"
        );
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

/// Round 15 wave 7 (lane throw2): TH1-2's allocation-free name check and
/// TH1-5's deferred-message mark.
#[cfg(test)]
mod throw2_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::vm::Vm;

    #[test]
    fn a_display_name_is_exactly_the_dotted_internal_name() {
        for (internal, display, want) in [
            ("java/lang/String", "java.lang.String", true),
            ("java/lang/String", "java.lang.Strin", false),
            ("java/lang/String", "java/lang/String", false),
            ("Main$Inner", "Main$Inner", true),
            ("a/b", "a.c", false),
            ("", "", true),
        ] {
            assert_eq!(
                internal_name_spells_display(internal, display),
                internal.replace('/', ".") == display,
                "{internal} {display}"
            );
            assert_eq!(internal_name_spells_display(internal, display), want);
        }
    }

    /// Only an NPE in the constructor's state (`extendedMessageState == 1`,
    /// no messages) takes the mark, only a marked one answers a deferred
    /// message, and once the JDK has stored an answer (state 2) the native
    /// answers as before.
    #[test]
    fn only_a_marked_npe_answers_a_deferred_message() {
        let mut vm = Vm::new(VmConfig::default());
        // The mark first: its allocation then moves nothing this test holds.
        if try_create_java_string(&vm.shared, DEFERRED_NPE_MESSAGE_MARK).is_none() {
            return;
        }
        let Ok(npe) = create_exception_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/NullPointerException",
            None,
        ) else {
            return;
        };
        let shared = &vm.shared;
        let Some((extended_slot, state_slot)) =
            npe_extended_message_slots(shared, shared.mem.heap.class_id_of(npe))
        else {
            // A class library without the JDK's fields: nothing to defer.
            return;
        };
        assert_eq!(deferred_npe_message(shared, npe), None);
        shared.mem.heap.set_field(npe, state_slot, Value::Int(1));
        shared
            .mem
            .heap
            .set_field(npe, extended_slot, Value::Object(None));
        assert!(mark_npe_message_deferred(shared, npe));
        assert!(deferred_npe_message(shared, npe).is_some());
        // Marked twice is refused: the slot is no longer empty.
        assert!(!mark_npe_message_deferred(shared, npe));
        shared
            .mem
            .heap
            .set_field(npe, extended_slot, Value::Object(None));
        shared.mem.heap.set_field(npe, state_slot, Value::Int(2));
        assert!(!mark_npe_message_deferred(shared, npe));
        assert_eq!(deferred_npe_message(shared, npe), None);
    }
}

#[cfg(test)]
mod gcg_g4oo_oome_construct_depth_tests {
    /// gcg g4/oo: the construction depth `create_vm_oome_object` raises is
    /// restored before a foreign panic is resumed, so a contained panic cannot
    /// leave the thread skipping the post-OOME debt for good.
    #[test]
    fn gcg_g4oo_the_construct_depth_is_restored_before_a_panic_resumes() {
        let src = include_str!("exceptions.rs");
        let body = src
            .split("fn create_vm_oome_object_with_array_retry(")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .expect("create_vm_oome_object body");
        let caught = body.find("std::panic::catch_unwind(").expect("the wrapper");
        let restore = body
            .find("thread.oome_construct_depth.saturating_sub(1)")
            .expect("the restore");
        let resume = body
            .find("std::panic::resume_unwind(")
            .expect("the resumed panic");
        assert!(caught < restore && restore < resume);
    }
}

#[cfg(test)]
mod nio21_w18_live_trap_snapshot_tests {
    use super::live_trap_snapshot_fits_stack;
    use crate::jit::conservative_roots::ActiveCompiledFrame;

    fn frame(depth: u32, bci: i32) -> ActiveCompiledFrame {
        ActiveCompiledFrame {
            inline_chain_shared: None,
            interp_depth: depth,
            chain_index: 0,
            label: std::sync::Arc::from("p/C.div0:(II)I"),
            owner_class_id: 1,
            cm_ptr: 0,
            bci,
            inline_chain: Vec::new(),
            chain_exact: false,
            trap_site_exact: false,
            chain_poisoned: false,
            compile_cp_stamp: None,
            fast_throw_c2_tier: false,
            frame_rbp: None,
        }
    }

    /// The `Nio21ImplicitCatch` division window: `hot`'s OSR continuation
    /// under the interpreted `call` (depth 2 of 3), the trapping `div0` above
    /// it (depth 3). The ordinary stand-in test refuses this shape; a live
    /// snapshot of it stands in.
    #[test]
    fn nio21_w18_a_live_snapshot_spanning_an_interpreter_frame_stands_in() {
        let snap = vec![frame(2, 40), frame(3, 4)];
        assert!(live_trap_snapshot_fits_stack(3, &snap));
        assert!(!crate::runtime::stackwalker::trap_snapshot_can_stand_in_for_walk(&[], &snap));
    }

    /// Empty, or a frame recorded above the stack as it is now: refused.
    #[test]
    fn nio21_w18_an_empty_or_too_deep_live_snapshot_is_refused() {
        assert!(!live_trap_snapshot_fits_stack(3, &[]));
        assert!(!live_trap_snapshot_fits_stack(
            3,
            &[frame(3, 4), frame(4, 1)]
        ));
        assert!(live_trap_snapshot_fits_stack(3, &[frame(3, 4)]));
    }

    /// The one door that builds with a live snapshot passes `true`, and the
    /// shared doors keep `false` (a source pin: the property is the caller's).
    #[test]
    fn nio21_w18_only_the_compiled_local_handler_builds_live() {
        let helpers = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = helpers
            .split("unsafe fn local_handler_enter_implicit(")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .expect("local_handler_enter_implicit body");
        assert!(body.contains("drained.npe_action,"));
        assert!(body.contains("        true,\n    ) {"));
        assert!(helpers
            .contains("materialize_implicit_signal_or_frames(vm, thread, signal, trap_frames, npe_action, false)"));
    }
}

#[cfg(test)]
mod nio21_w19_direct_implicit_build_tests {
    use super::{
        code_matches_shape, DELEGATE_INIT_EMPTY, DELEGATE_INIT_STRING, NPE_FILL_IN_STACK_TRACE,
        THROWABLE_FILL_IN_STACK_TRACE, THROWABLE_INIT_EMPTY, THROWABLE_INIT_STRING,
    };

    /// `javap -c` of JDK 25.0.3 `java.lang.Throwable(String)`, constant-pool
    /// indices as that build numbers them.
    const JDK25_THROWABLE_INIT_STRING: [u8; 48] = [
        0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x2a, 0xb5, 0x00, 0x07, 0x2a, 0xb2, 0x00, 0x0d, 0xb5, 0x00,
        0x11, 0x2a, 0xb2, 0x00, 0x14, 0xb5, 0x00, 0x18, 0x2a, 0xb6, 0x00, 0x1b, 0x57, 0x2a, 0x2b,
        0xb5, 0x00, 0x2d, 0xb2, 0x00, 0x1f, 0x99, 0x00, 0x0b, 0x2a, 0xb6, 0x00, 0x23, 0x2b, 0xb8,
        0x00, 0x27, 0xb1,
    ];
    const JDK25_THROWABLE_INIT_EMPTY: [u8; 43] = [
        0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x2a, 0xb5, 0x00, 0x07, 0x2a, 0xb2, 0x00, 0x0d, 0xb5, 0x00,
        0x11, 0x2a, 0xb2, 0x00, 0x14, 0xb5, 0x00, 0x18, 0x2a, 0xb6, 0x00, 0x1b, 0x57, 0xb2, 0x00,
        0x1f, 0x99, 0x00, 0x0b, 0x2a, 0xb6, 0x00, 0x23, 0x01, 0xb8, 0x00, 0x27, 0xb1,
    ];
    const JDK25_THROWABLE_FILL: [u8; 29] = [
        0x2a, 0xb4, 0x00, 0x11, 0xc7, 0x00, 0x0a, 0x2a, 0xb4, 0x00, 0xc6, 0xc6, 0x00, 0x10, 0x2a,
        0x03, 0xb6, 0x00, 0xca, 0x57, 0x2a, 0xb2, 0x00, 0x0d, 0xb5, 0x00, 0x11, 0x2a, 0xb0,
    ];
    const JDK25_NPE_FILL: [u8; 41] = [
        0x2a, 0xb4, 0x00, 0x0a, 0x9a, 0x00, 0x0b, 0x2a, 0x04, 0xb5, 0x00, 0x0a, 0xa7, 0x00, 0x18,
        0x2a, 0xb4, 0x00, 0x0a, 0x04, 0xa0, 0x00, 0x10, 0x2a, 0x2a, 0xb6, 0x00, 0x10, 0xb5, 0x00,
        0x14, 0x2a, 0x05, 0xb5, 0x00, 0x0a, 0x2a, 0xb7, 0x00, 0x18, 0xb0,
    ];

    #[test]
    fn nio21_w19_the_jdk25_chain_matches_its_shapes() {
        assert!(code_matches_shape(
            &JDK25_THROWABLE_INIT_STRING,
            THROWABLE_INIT_STRING
        ));
        assert!(code_matches_shape(
            &JDK25_THROWABLE_INIT_EMPTY,
            THROWABLE_INIT_EMPTY
        ));
        assert!(code_matches_shape(
            &JDK25_THROWABLE_FILL,
            THROWABLE_FILL_IN_STACK_TRACE
        ));
        assert!(code_matches_shape(&JDK25_NPE_FILL, NPE_FILL_IN_STACK_TRACE));
        assert!(code_matches_shape(
            &[0x2a, 0x2b, 0xb7, 0x00, 0x07, 0xb1],
            DELEGATE_INIT_STRING
        ));
        assert!(code_matches_shape(
            &[0x2a, 0xb7, 0x00, 0x01, 0xb1],
            DELEGATE_INIT_EMPTY
        ));
    }

    /// An instrumented or different constructor refuses: one inserted call, a
    /// moved branch, a different store, a wrong length.
    #[test]
    fn nio21_w19_any_other_bytecode_refuses() {
        let mut moved = JDK25_THROWABLE_INIT_STRING;
        moved[38] = 0x0c; // ifeq +12
        assert!(!code_matches_shape(&moved, THROWABLE_INIT_STRING));
        let mut other_store = JDK25_THROWABLE_INIT_STRING;
        other_store[30] = 0xb3; // putstatic instead of putfield
        assert!(!code_matches_shape(&other_store, THROWABLE_INIT_STRING));
        let mut instrumented = vec![0xb8, 0x00, 0x99];
        instrumented.extend_from_slice(&JDK25_THROWABLE_INIT_STRING);
        assert!(!code_matches_shape(&instrumented, THROWABLE_INIT_STRING));
        // A delegation that loads a constant message instead of its argument.
        assert!(!code_matches_shape(
            &[0x2a, 0x12, 0xb7, 0x00, 0x07, 0xb1],
            DELEGATE_INIT_STRING
        ));
        assert!(!code_matches_shape(
            &JDK25_THROWABLE_INIT_EMPTY,
            THROWABLE_INIT_STRING
        ));
    }

    /// The direct build is wired into the trap-frame door and (JIT round 23
    /// wave 1, lane L3) into `create_exception_object` behind its switch; the
    /// trap door's refusal arm runs the constructors without asking a second
    /// time (a source pin).
    #[test]
    fn nio21_w19_only_the_trap_frame_door_builds_directly() {
        let src = include_str!("exceptions.rs");
        let needle = format!(
            "match {}(shared, thread, class_name, message)",
            "try_build_implicit_throwable_directly"
        );
        let calls = src.matches(needle.as_str()).count();
        assert_eq!(calls, 1);
        let trap = src_fn("pub(crate) fn create_exception_object_with_trap_frames_live(");
        let args = "(shared, thread, class_name, message)";
        assert!(trap.contains(&format!("create_exception_object_constructed{args}")));
        assert!(!trap.contains(&format!("create_exception_object{args}")));
        let door = src_fn("pub fn create_exception_object(");
        let gate = door
            .find("direct_build_every_door_enabled()")
            .expect("the every-door switch");
        let ask = door
            .find("try_build_implicit_throwable_directly(shared, thread, class_name, message)")
            .expect("asks the direct build");
        let collect = door
            .find("collect_if_requested(shared, thread)")
            .expect("takes the collection request first");
        let fallback = door
            .find("create_exception_object_constructed(shared, thread, class_name, message)")
            .expect("the constructors on a refusal");
        assert!(gate < collect && collect < ask && ask < fallback);
    }

    /// nio21 wave 20 L1: the dispatch helper reaches the throw-new direct
    /// build exactly once, behind the `<init>` screen and after the
    /// null-receiver NPE (a null receiver must keep raising at the invoke).
    #[test]
    fn nio21_w20_the_dispatch_helper_builds_throw_new_after_the_null_receiver_check() {
        let helpers = crate::jit::helpers::HELPERS_SRC;
        let body = helpers
            .split("unsafe fn jit_invoke_dispatch_body(")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .expect("jit_invoke_dispatch_body");
        let hook = body
            .find("try_throwable_init_direct_at_dispatch(vm, info, args_slice)")
            .expect("the hook");
        let npe = body
            .find("jit_action::INVOKE_RECEIVER")
            .expect("the null-receiver NPE");
        assert!(npe < hook);
        assert_eq!(
            body.matches("try_throwable_init_direct_at_dispatch(")
                .count(),
            1
        );
        // The receiver must be exactly the named class.
        let direct = src_fn("pub(crate) fn jit_new_throwable_direct(");
        assert!(direct.contains("&*class.name != class_name"));
        assert!(direct.contains("ClassLoaderId::Bootstrap"));
    }

    /// nio21 wave 21 L1: every capture (the interpreted constructors' through
    /// `fillInStackTrace(0)`, the native shadows', the direct builds') pins its
    /// throwable, reads the pin back and captures again at a moved address,
    /// under the switch; the pin is released on every path.
    #[test]
    fn nio21_w21_every_capture_recaptures_a_moved_throwable() {
        let exec = include_str!("../vm/vm_exec.rs");
        let wrapper = exec
            .split(
                "    fn capture_throwable_stack_trace(&mut self, throwable: ObjectRef) -> usize {",
            )
            .nth(1)
            .and_then(|rest| rest.split(concat!("\n", "    }", "\n")).next())
            .expect("the capture wrapper");
        let pin = wrapper
            .find("native_pin_roots.push(throwable)")
            .expect("pins the throwable");
        // After the pin: the switch-off arm above it captures once too.
        let once = pin
            + wrapper[pin..]
                .find("capture_throwable_stack_trace_once(throwable)")
                .expect("captures once");
        let again = wrapper
            .find("capture_throwable_stack_trace_once(now)")
            .expect("captures again at the moved address");
        let release = wrapper
            .find("native_pin_roots.truncate(pin)")
            .expect("releases the pin");
        assert!(pin < once && once < again && again < release);
        assert!(wrapper.contains("throwable_recapture_if_moved_enabled()"));
        assert_eq!(
            exec.matches("fn capture_throwable_stack_trace_once(")
                .count(),
            1
        );
    }

    /// The body of `head` in `src`, up to its first column-0 closing brace.
    fn body_in<'a>(src: &'a str, head: &str) -> &'a str {
        src.split(head)
            .nth(1)
            .and_then(|rest| rest.split(concat!("\n", "}", "\n")).next())
            .expect("function body")
    }

    /// nio21 wave 21 L1: a handler-bearing lambda impl reaches the compiled
    /// site only under its switch, and both services that can see a throwable
    /// leave it route that throwable through the impl's table: the Rust
    /// direct arm BEFORE it restashes an explicit throwable as "not a deopt",
    /// and the callee-deopt service BEFORE its receiver-based table probe
    /// (which cannot see a proxy's impl), never at an owned-arguments site.
    #[test]
    fn nio21_w21_handler_bearing_lambda_impls_route_through_their_own_table() {
        let lambda = include_str!("interpreter/lambda.rs");
        let site = body_in(lambda, "pub(crate) fn lambda_jit_site(");
        assert!(site.contains("lambda_handler_impl_enabled()"));
        assert!(site.contains("build_lambda_jit_site(shared, proxy_class_id, allow_handlers)"));
        let helpers = crate::jit::helpers::HELPERS_SRC;
        let direct = body_in(helpers, "unsafe fn try_lambda_site_direct_call(");
        let routed = direct
            .find("lambda_route_thrown_through_impl(")
            .expect("the direct arm routes an explicit throwable");
        let restash = direct
            .find("if lambda_direct_sentinel_is_not_a_deopt(")
            .expect("the not-a-deopt restash");
        assert!(routed < restash);
        let service = body_in(
            helpers,
            "unsafe fn handle_compiled_callee_deopt_sentinel_inner(",
        );
        let hook = service
            .find("lambda_handler_site_for_service(vm, info, receiver_class_id)")
            .expect("the service hook");
        let probe = service
            .find("mic_callee_has_exception_table(vm, receiver_class_id, info)")
            .expect("the receiver-based probe");
        assert!(hook < probe);
        assert!(service.contains("if !args_owned_by_callee && !(escaped_redispatch"));
        let gate = body_in(helpers, "fn lambda_handler_site_for_service(");
        assert!(gate.contains("lambda_handler_impl_enabled()"));
        assert!(gate.contains("exception_table.is_empty()"));
    }

    fn src_fn(head: &str) -> String {
        include_str!("exceptions.rs")
            .split(head)
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .expect("function body")
            .to_string()
    }
}

#[cfg(test)]
mod xthrow_lean_build_tests {
    //! JIT round 20 wave 18, lane xThrow (THROW20-5,
    //! `CRATONVM_JIT_IMPLICIT_THROW_LEAN_BUILD`): the lean direct build makes
    //! the same message text and the same throwable as before.
    use super::*;
    use crate::config::VmConfig;
    use crate::vm::Vm;

    #[test]
    fn the_buffered_aioobe_message_is_the_formatted_one() {
        for (index, length) in [
            (5_i64, 3_i64),
            (-1, 0),
            (0, 0),
            (i64::from(i32::MIN), i64::from(i32::MAX)),
            (i64::MIN, i64::MIN),
            (i64::MAX, i64::MIN),
        ] {
            let mut buf: AioobeMessageBuf = [0; 96];
            let want = format!("Index {index} out of bounds for length {length}");
            assert_eq!(
                format_aioobe_message(&mut buf, index, length),
                Some(want.as_str()),
                "{index} {length}"
            );
        }
    }

    /// Three classes always; three more (JIT round 23 wave 1, lane L3) only
    /// under `CRATONVM_THROWABLE_DIRECT_IMPLICIT_BUILD_EVERY_DOOR`; every slot
    /// fits the per-thread row table.
    #[test]
    fn only_the_implicit_classes_have_a_class_slot() {
        for (name, off, on) in [
            ("java/lang/ArrayIndexOutOfBoundsException", Some(0), Some(0)),
            ("java/lang/ArithmeticException", Some(1), Some(1)),
            ("java/lang/NullPointerException", Some(2), Some(2)),
            ("java/lang/ClassCastException", None, Some(3)),
            ("java/lang/ArrayStoreException", None, Some(4)),
            ("java/lang/NegativeArraySizeException", None, Some(5)),
            ("java/lang/IndexOutOfBoundsException", None, None),
            ("java/lang/StringIndexOutOfBoundsException", None, None),
            ("java/lang/IllegalMonitorStateException", None, None),
        ] {
            assert_eq!(implicit_direct_class_slot_as(name, false), off, "{name}");
            assert_eq!(implicit_direct_class_slot_as(name, true), on, "{name}");
            if let Some(slot) = on {
                assert!(slot < IMPLICIT_DIRECT_CLASS_SLOTS);
            }
        }
    }

    /// JIT round 23 wave 1, lane L3: the operand tables name what the loaded
    /// JDK's bytes name. A method whose opcodes have the modelled shape must
    /// also pass the operand check, or the direct build would refuse every
    /// throw on this class library (a silent loss, not a wrong answer). Skipped
    /// where the class is not loaded or its `Code` is not decoded.
    #[test]
    fn the_loaded_jdk_chain_names_the_modelled_members() {
        let vm = Vm::new(VmConfig::default());
        let cm = vm.shared.classes.class_manager.read();
        let class_of = |name: &str| {
            cm.loaded_class_under_exact_key(name, cratonvm_types::ClassLoaderId::Bootstrap)
                .and_then(|id| cm.get_class(id))
        };
        if let Some(cls) = class_of("java/lang/Throwable") {
            let cp = &cls.constant_pool;
            for (name, descriptor, shape, refs) in [
                (
                    "<init>",
                    "(Ljava/lang/String;)V",
                    THROWABLE_INIT_STRING,
                    THROWABLE_INIT_STRING_REFS,
                ),
                (
                    "<init>",
                    "()V",
                    THROWABLE_INIT_EMPTY,
                    THROWABLE_INIT_EMPTY_REFS,
                ),
                (
                    "fillInStackTrace",
                    "()Ljava/lang/Throwable;",
                    THROWABLE_FILL_IN_STACK_TRACE,
                    THROWABLE_FILL_REFS,
                ),
            ] {
                let Some(m) = cls.find_method(name, descriptor) else {
                    continue;
                };
                if method_has_shape(m, shape) {
                    assert!(
                        method_has_shape_and_refs(cp, m, shape, refs),
                        "Throwable.{name}{descriptor}"
                    );
                }
            }
        }
        if let Some(cls) = class_of("java/lang/NullPointerException") {
            if let Some(m) = cls.find_method("fillInStackTrace", "()Ljava/lang/Throwable;") {
                if method_has_shape(m, NPE_FILL_IN_STACK_TRACE) {
                    assert!(method_has_shape_and_refs(
                        &cls.constant_pool,
                        m,
                        NPE_FILL_IN_STACK_TRACE,
                        NPE_FILL_REFS
                    ));
                }
            }
        }
        // Every delegating constructor of the six names its superclass's.
        for name in [
            "java/lang/ArrayIndexOutOfBoundsException",
            "java/lang/IndexOutOfBoundsException",
            "java/lang/ArithmeticException",
            "java/lang/NullPointerException",
            "java/lang/ClassCastException",
            "java/lang/ArrayStoreException",
            "java/lang/NegativeArraySizeException",
            "java/lang/RuntimeException",
            "java/lang/Exception",
        ] {
            let Some(cls) = class_of(name) else {
                continue;
            };
            let Some(super_name) = cls
                .superclass
                .and_then(|s| cm.get_class(s))
                .map(|s| s.name.clone())
            else {
                continue;
            };
            for (descriptor, shape, operand) in [
                ("(Ljava/lang/String;)V", DELEGATE_INIT_STRING, 3),
                ("()V", DELEGATE_INIT_EMPTY, 2),
            ] {
                let Some(code) = cls
                    .find_method("<init>", descriptor)
                    .filter(|m| method_has_shape(m, shape))
                    .and_then(|m| m.code())
                else {
                    continue;
                };
                assert_eq!(
                    cp_member_at(&cls.constant_pool, &code.code, operand),
                    Some((&*super_name, "<init>", descriptor)),
                    "{name}.<init>{descriptor}"
                );
            }
        }
    }

    /// The operand check refuses a pool entry of the wrong member, of the
    /// wrong kind, and an operand past the code (pure, on a hand-built pool).
    #[test]
    fn the_operand_check_refuses_another_member() {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry, CpUtf8};
        const CAUSE: &str = "Ljava/lang/Throwable;";
        let utf8 = |s: &str| ConstantPoolEntry::Utf8(CpUtf8::from_arc(std::sync::Arc::from(s)));
        // Indices 0..=6: the unused slot, the owner's name and class, the
        // member's name and type, the field reference.
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            utf8("java/lang/Throwable"),
            ConstantPoolEntry::ClassReference { name_index: 1 },
            utf8("cause"),
            utf8(CAUSE),
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            },
            ConstantPoolEntry::FieldReference {
                class_index: 2,
                name_and_type_index: 5,
            },
        ]);
        let code = [0x2a, 0xb5, 0x00, 0x06, 0xb5, 0x00, 0x05];
        let check = |r: CpRef| code_refs_match(&cp, &code, &[r]);
        let t = THROWABLE;
        assert!(check((2, t, "cause", CAUSE)));
        assert!(check((2, None, "cause", CAUSE)));
        // Another owner, another name, another descriptor.
        assert!(!check((2, Some("java/lang/Object"), "cause", CAUSE)));
        assert!(!check((2, t, "detailMessage", CAUSE)));
        assert!(!check((2, t, "cause", "I")));
        // A NameAndType where a member reference belongs.
        assert!(!check((5, t, "cause", CAUSE)));
        // Past the end.
        assert!(!check((6, t, "cause", CAUSE)));
    }

    /// JIT round 23 wave 1, lane L3: for each of the six implicit kinds, the
    /// direct build stores what the constructor chain stores -- the class, the
    /// message, `cause = this`, `stackTrace = UNASSIGNED_STACK`, the
    /// suppressed sentinel, the depth, and an NPE's `extendedMessageState`.
    /// A kind the class library does not let the door build is skipped (the
    /// door then refuses and the constructors run, which is the other arm).
    #[test]
    fn every_door_builds_what_the_constructors_build() {
        let mut vm = Vm::new(VmConfig::default());
        for (class, msg) in [
            (
                "java/lang/ArrayIndexOutOfBoundsException",
                Some("Index 3 out of bounds for length 2"),
            ),
            ("java/lang/ArithmeticException", Some("/ by zero")),
            ("java/lang/NullPointerException", None),
            (
                "java/lang/ClassCastException",
                Some("class java.lang.Integer cannot be cast to class java.lang.String"),
            ),
            ("java/lang/ArrayStoreException", Some("java.lang.Integer")),
            ("java/lang/NegativeArraySizeException", Some("-1")),
        ] {
            let Ok(constructed) =
                create_exception_object_constructed(&vm.shared, &mut vm.main_thread, class, msg)
            else {
                continue;
            };
            let pin = vm.main_thread.native_pin_roots.len();
            vm.main_thread.native_pin_roots.push(constructed);
            let direct =
                try_build_implicit_throwable_directly(&vm.shared, &mut vm.main_thread, class, msg);
            let constructed = vm.main_thread.native_pin_roots[pin];
            vm.main_thread.native_pin_roots.truncate(pin);
            let Some(direct) = direct else {
                continue;
            };
            let direct = direct.expect("a direct build never answers an error");
            let shared = &vm.shared;
            let heap = &shared.mem.heap;
            assert_ne!(direct, constructed, "{class}");
            assert_eq!(
                heap.class_id_of(direct),
                heap.class_id_of(constructed),
                "{class}"
            );
            let text = |exc: ObjectRef| {
                let slot = throwable_own_field_index(shared, "detailMessage")?;
                match heap.get_field(exc, slot) {
                    Value::Object(Some(s)) => crate::vm::read_java_string(heap, s),
                    _ => None,
                }
            };
            assert_eq!(text(direct), text(constructed), "{class}: detailMessage");
            assert_eq!(text(direct).as_deref(), msg, "{class}: detailMessage text");
            if let Some(slot) = throwable_own_field_index(shared, "cause") {
                assert_eq!(
                    heap.get_field(direct, slot),
                    Value::Object(Some(direct)),
                    "{class}"
                );
            }
            for name in ["stackTrace", "suppressedExceptions", "depth"] {
                if let Some(slot) = throwable_own_field_index(shared, name) {
                    assert_eq!(
                        heap.get_field(direct, slot),
                        heap.get_field(constructed, slot),
                        "{class}: {name}"
                    );
                }
            }
            if let Some((_, state)) = npe_extended_message_slots(shared, heap.class_id_of(direct)) {
                assert_eq!(
                    heap.get_field(direct, state),
                    heap.get_field(constructed, state),
                    "{class}: extendedMessageState"
                );
            }
        }
    }

    /// Two direct builds of the same class: the second answers its class from
    /// the per-thread row the first kept, and both carry the class and the
    /// message the constructor chain would store. Skipped where the class
    /// library does not let the door build at all.
    #[test]
    fn a_kept_class_row_builds_the_same_throwable() {
        const AIOOBE: &str = "java/lang/ArrayIndexOutOfBoundsException";
        let mut vm = Vm::new(VmConfig::default());
        let msg = "Index 5 out of bounds for length 3";
        let Some(Ok(first)) = try_build_implicit_throwable_directly(
            &vm.shared,
            &mut vm.main_thread,
            AIOOBE,
            Some(msg),
        ) else {
            return;
        };
        let class_id = vm.shared.mem.heap.class_id_of(first);
        let loaded = vm
            .shared
            .classes
            .class_manager
            .read()
            .loaded_class_under_exact_key(AIOOBE, cratonvm_types::ClassLoaderId::Bootstrap);
        assert_eq!(Some(class_id), loaded);
        if implicit_throw_lean_build_enabled() {
            assert_eq!(
                cached_implicit_direct_class(&vm.shared, 0).map(|(id, _)| id),
                Some(class_id)
            );
            // A slot nothing was kept in answers nothing.
            assert_eq!(cached_implicit_direct_class(&vm.shared, 1), None);
        }
        // Pinned across the second build, which captures (and may collect).
        let pin = vm.main_thread.native_pin_roots.len();
        vm.main_thread.native_pin_roots.push(first);
        let Some(Ok(second)) = try_build_implicit_throwable_directly(
            &vm.shared,
            &mut vm.main_thread,
            AIOOBE,
            Some(msg),
        ) else {
            panic!("a class that built once builds again");
        };
        let first = vm.main_thread.native_pin_roots[pin];
        vm.main_thread.native_pin_roots.truncate(pin);
        let shared = &vm.shared;
        assert_ne!(first, second);
        assert_eq!(shared.mem.heap.class_id_of(second), class_id);
        let Some(slot) = throwable_own_field_index(shared, "detailMessage") else {
            return;
        };
        for exc in [first, second] {
            let Value::Object(Some(text)) = shared.mem.heap.get_field(exc, slot) else {
                panic!("the message is stored");
            };
            assert_eq!(
                crate::vm::read_java_string(&shared.mem.heap, text).as_deref(),
                Some(msg)
            );
        }
    }
}

#[cfg(test)]
mod zs9la_traced_pool_tests {
    //! zs s9/la: HotSpot's preallocated traced pool on ZGC
    //! (`CRATONVM_ZGC_OOME_TRACED_POOL`). The first four VM-raised `Java heap
    //! space` errors ARE the four objects preallocated at boot, then the
    //! traceless singleton; nothing is constructed on the refusing heap.

    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::vm::Vm;

    fn zgc_vm() -> Option<Vm> {
        let vm = Vm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Zgc,
            max_heap_size: 64 << 20,
            ..VmConfig::default()
        });
        crate::vm::heap_is_zgc(&vm.shared).then_some(vm)
    }

    fn raise(vm: &mut Vm) -> ObjectRef {
        create_vm_oome_object(
            &vm.shared,
            &mut vm.main_thread,
            "java/lang/OutOfMemoryError",
            Some("Java heap space"),
        )
        .expect("an OutOfMemoryError")
    }

    #[test]
    fn the_first_four_heap_errors_are_the_boot_pool_then_the_default() {
        let Some(mut vm) = zgc_vm() else {
            return;
        };
        ensure_singleton_oom(&vm.shared, &mut vm.main_thread);
        let pool: Vec<Option<ObjectRef>> = (0..HOTSPOT_PREALLOCATED_OOME_COUNT as usize)
            .map(|i| vm.shared.mem.preallocated_oome.read().traced(i))
            .collect();
        if pool.iter().any(Option::is_none) {
            eprintln!("zs9la: the pool could not be built here (no JDK); skipped");
            return;
        }
        for (i, want) in pool.iter().enumerate() {
            let got = raise(&mut vm);
            assert_eq!(Some(got), *want, "slot {i}: the preallocated error");
        }
        let fifth = raise(&mut vm);
        assert_eq!(
            Some(fifth),
            *vm.shared.mem.singleton_oom.read(),
            "then the default"
        );
        // addr-keyed: test-local identity check, no allocation between collect and use.
        let distinct: std::collections::HashSet<usize> =
            pool.iter().flatten().map(|o| o.as_ptr() as usize).collect();
        assert_eq!(distinct.len(), pool.len(), "four distinct errors");
        eprintln!("zs9la: pool served {} errors, then the default", pool.len());
    }

    /// Kill control: `CRATONVM_ZGC_OOME_TRACED_POOL=0` builds no pool, and the
    /// first heap error is constructed (neither a pool object nor the default).
    #[test]
    fn the_kill_switch_constructs_as_before() {
        let _off = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_ZGC_OOME_TRACED_POOL",
                Some("0"),
            )]),
        );
        let Some(mut vm) = zgc_vm() else {
            return;
        };
        ensure_singleton_oom(&vm.shared, &mut vm.main_thread);
        assert_eq!(vm.shared.mem.preallocated_oome.read().traced_len(), 0);
        let Some(singleton) = *vm.shared.mem.singleton_oom.read() else {
            return;
        };
        let first = raise(&mut vm);
        assert_ne!(first, singleton, "a fresh, constructed error");
    }

    /// Generational: no pool (unchanged).
    #[test]
    fn other_collectors_build_no_pool() {
        let mut vm = Vm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        ensure_singleton_oom(&vm.shared, &mut vm.main_thread);
        assert_eq!(vm.shared.mem.preallocated_oome.read().traced_len(), 0);
    }
}

#[cfg(test)]
mod r22_throws_fast_throw_profile_tests {
    use super::{FastThrowKind, FastThrowTrapProfile};
    use std::sync::Arc;

    /// JIT round 22 wave 2 (lane throws, THROWS21-5): the label-keyed profile
    /// (`CRATONVM_FAST_THROW_PROFILE_BY_LABEL=1`) answers exactly what the set
    /// answers: per (label, bci, kind), a different `Arc` of the same text
    /// included, and a second `note` of a recorded site is a no-op.
    #[test]
    fn the_label_keyed_profile_answers_like_the_set() {
        for on in ["0", "1"] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_FAST_THROW_PROFILE_BY_LABEL", Some(on))],
                || {
                    let p = FastThrowTrapProfile::default();
                    let a: Arc<str> = Arc::from("p/C.m:()V");
                    let a2: Arc<str> = Arc::from("p/C.m:()V");
                    let b: Arc<str> = Arc::from("p/C.n:()V");
                    assert!(!p.has_trap_at(&a, 3, FastThrowKind::NullPointer));
                    p.note(&a, 3, FastThrowKind::NullPointer);
                    p.note(&a, 3, FastThrowKind::NullPointer);
                    assert!(p.has_trap_at(&a, 3, FastThrowKind::NullPointer));
                    assert!(p.has_trap_at(&a2, 3, FastThrowKind::NullPointer), "by text");
                    assert!(!p.has_trap_at(&a, 3, FastThrowKind::Arithmetic), "kind");
                    assert!(!p.has_trap_at(&a, 4, FastThrowKind::NullPointer), "bci");
                    assert!(!p.has_trap_at(&b, 3, FastThrowKind::NullPointer), "label");
                    p.note(&a, 4, FastThrowKind::ArrayIndexOutOfBounds);
                    assert!(p.has_trap_at(&a, 4, FastThrowKind::ArrayIndexOutOfBounds));
                    assert!(p.has_trap_at(&a, 3, FastThrowKind::NullPointer));
                    if on == "1" {
                        assert_eq!(p.by_label.read().1, 2, "two rows, the repeat not counted");
                    }
                },
            );
        }
    }
}
