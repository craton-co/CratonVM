// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Interpreter intrinsic table — fast-path dispatch for hot JDK methods.
//!
//! This module provides two functions used by the interpreter inline cache:
//!
//! * [`lookup`] — called **once per call site**, at IC-fill time, to map a
//!   `(class, name, descriptor)` triple to an [`InterpIntrinsic`] tag.
//! * [`dispatch`] — the steady-state fast path: a plain `match` over the
//!   enum tag that calls the per-method handler directly, with no `RwLock`,
//!   no descriptor parse, and no `HashMap` probe.
//!
//! Every handler is byte-for-byte behavior-identical to the normal native
//! registry dispatch path (project rule: no synthetic stubs) — the handlers
//! delegate to the same `crate::lang_*` / `crate::util_*` native functions
//! the registry would have invoked.
//!
//! See `feature_roadmap_interpreter_intrinsic_table.md` and
//! `intrinsic_table_contract.md`.

pub mod integer;
pub mod long;
pub mod math;
pub mod object;
pub mod record;
pub mod string;
pub mod stringbuilder;
pub mod system;

use cratonvm_native_api::{InterpIntrinsic, NativeContext};
use cratonvm_types::{error::MethodCallResult, Value};

/// One-time resolution: static `(class, name, descriptor)` -> intrinsic kind.
///
/// Returns `None` for any method not in the intrinsic table — the caller then
/// falls back to the ordinary native/bytecode dispatch path. The match is
/// keyed on the **exact** descriptor so overloads resolve independently
/// (roadmap §3.4: no fuzzy descriptor matching).
///
/// A plain `match` is deliberately used instead of `phf` — resolution happens
/// once per call site, and direct branches beat hashing while keeping the
/// workspace dependency-free.
pub fn lookup(class: &str, name: &str, desc: &str) -> Option<InterpIntrinsic> {
    use InterpIntrinsic::*;
    Some(match (class, name, desc) {
        // java/lang/Object — virtual
        ("java/lang/Object", "getClass", "()Ljava/lang/Class;") => ObjectGetClass,
        ("java/lang/Object", "hashCode", "()I") => ObjectHashCode,

        // java/lang/String — virtual (String is final, no override risk)
        ("java/lang/String", "length", "()I") => StringLength,
        ("java/lang/String", "charAt", "(I)C") => StringCharAt,
        ("java/lang/String", "isEmpty", "()Z") => StringIsEmpty,

        // java/lang/System — static
        ("java/lang/System", "arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V") => {
            SystemArraycopy
        }

        // java/lang/StringBuilder — virtual
        ("java/lang/StringBuilder", "append", "(Ljava/lang/String;)Ljava/lang/StringBuilder;") => {
            StringBuilderAppendString
        }
        ("java/lang/StringBuilder", "append", "(I)Ljava/lang/StringBuilder;") => {
            StringBuilderAppendInt
        }
        ("java/lang/StringBuilder", "append", "(C)Ljava/lang/StringBuilder;") => {
            StringBuilderAppendChar
        }
        ("java/lang/StringBuilder", "append", "(J)Ljava/lang/StringBuilder;") => {
            StringBuilderAppendLong
        }
        ("java/lang/StringBuilder", "append", "(Z)Ljava/lang/StringBuilder;") => {
            StringBuilderAppendBool
        }
        ("java/lang/StringBuilder", "append", "(Ljava/lang/Object;)Ljava/lang/StringBuilder;") => {
            StringBuilderAppendObject
        }
        ("java/lang/StringBuilder", "toString", "()Ljava/lang/String;") => StringBuilderToString,
        ("java/lang/StringBuilder", "length", "()I") => StringBuilderLength,

        // java/lang/Integer
        ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;") => IntegerValueOf,
        ("java/lang/Integer", "intValue", "()I") => IntegerIntValue,
        ("java/lang/Integer", "parseInt", "(Ljava/lang/String;)I") => IntegerParseInt,

        // java/lang/Long
        ("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;") => LongValueOf,
        ("java/lang/Long", "longValue", "()J") => LongLongValue,
        ("java/lang/Long", "parseLong", "(Ljava/lang/String;)J") => LongParseLong,

        // java/lang/Thread — static, empty body, pure CPU hint
        ("java/lang/Thread", "onSpinWait", "()V") => ThreadOnSpinWait,
        ("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;") => ThreadCurrentThread,

        // java/lang/Math — static, pure arithmetic
        ("java/lang/Math", "abs", "(I)I") => MathAbsInt,
        ("java/lang/Math", "abs", "(J)J") => MathAbsLong,
        ("java/lang/Math", "abs", "(D)D") => MathAbsDouble,
        ("java/lang/Math", "min", "(II)I") => MathMinInt,
        ("java/lang/Math", "max", "(II)I") => MathMaxInt,
        ("java/lang/Math", "min", "(JJ)J") => MathMinLong,
        ("java/lang/Math", "max", "(JJ)J") => MathMaxLong,
        ("java/lang/Math", "sqrt", "(D)D") => MathSqrt,

        _ => return None,
    })
}

/// Every `(class, name, descriptor)` [`lookup`] resolves, with the
/// staticness the interpreter has to agree with.
///
/// This table is the second copy of `lookup`'s arms on purpose: the tests
/// cross-check the three functions that must agree about it, and a single
/// table cannot disagree with itself. Adding an arm to `lookup` without adding
/// it here fails `the_table_is_complete`. Since j27 (CB17-6) the native
/// census (`--dump-native-registry`, `table_intrinsics`) publishes it beside
/// the registry rows: some of these bodies (the three `String` rows on a real
/// JDK) have no registry row at all.
pub const TABLE: &[(&str, &str, &str, bool)] = &[
    ("java/lang/Object", "getClass", "()Ljava/lang/Class;", false),
    ("java/lang/Object", "hashCode", "()I", false),
    ("java/lang/String", "length", "()I", false),
    ("java/lang/String", "charAt", "(I)C", false),
    ("java/lang/String", "isEmpty", "()Z", false),
    (
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        true,
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(Ljava/lang/String;)Ljava/lang/StringBuilder;",
        false,
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(I)Ljava/lang/StringBuilder;",
        false,
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(C)Ljava/lang/StringBuilder;",
        false,
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(J)Ljava/lang/StringBuilder;",
        false,
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(Z)Ljava/lang/StringBuilder;",
        false,
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(Ljava/lang/Object;)Ljava/lang/StringBuilder;",
        false,
    ),
    (
        "java/lang/StringBuilder",
        "toString",
        "()Ljava/lang/String;",
        false,
    ),
    ("java/lang/StringBuilder", "length", "()I", false),
    (
        "java/lang/Integer",
        "valueOf",
        "(I)Ljava/lang/Integer;",
        true,
    ),
    ("java/lang/Integer", "intValue", "()I", false),
    (
        "java/lang/Integer",
        "parseInt",
        "(Ljava/lang/String;)I",
        true,
    ),
    ("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;", true),
    ("java/lang/Long", "longValue", "()J", false),
    ("java/lang/Long", "parseLong", "(Ljava/lang/String;)J", true),
    ("java/lang/Thread", "onSpinWait", "()V", true),
    (
        "java/lang/Thread",
        "currentThread",
        "()Ljava/lang/Thread;",
        true,
    ),
    ("java/lang/Math", "abs", "(I)I", true),
    ("java/lang/Math", "abs", "(J)J", true),
    ("java/lang/Math", "abs", "(D)D", true),
    ("java/lang/Math", "min", "(II)I", true),
    ("java/lang/Math", "max", "(II)I", true),
    ("java/lang/Math", "min", "(JJ)J", true),
    ("java/lang/Math", "max", "(JJ)J", true),
    ("java/lang/Math", "sqrt", "(D)D", true),
];

/// Conservative class-agnostic prefilter for hot **virtual**-dispatch paths.
///
/// A `false` result is definitive **for an instance method**: no intrinsic
/// entry with that shape has this `(method_name, descriptor)` pair on any
/// class, so callers can skip the declaring-class lookup they would otherwise
/// need before calling [`lookup`]. A `true` result only means "maybe"; callers
/// must still resolve the actual declaring class and use [`lookup`] for the
/// final, sound decision.
///
/// **It is not a prefilter for STATIC call sites and must never be used as
/// one.** `Thread.onSpinWait ()V` and `Thread.currentThread
/// ()Ljava/lang/Thread;` both resolve through [`lookup`] and are deliberately
/// absent from the list below, so this function answers `false` for two live
/// intrinsics. That is sound today because the only caller is
/// `vm/src/runtime/interpreter/dispatch_virtual.rs`, and a static method never
/// reaches it — but the earlier wording ("no intrinsic entry has this pair on
/// any class") was simply false, and a future caller on the `invokestatic`
/// path would have lost both fast paths silently, with a green build. G9-1.
///
/// The invariant that IS true is pinned by `every_instance_entry_is_admitted`:
/// every non-static member of [`lookup`]'s table must be admitted here.
#[inline]
pub fn might_have_method_descriptor(name: &str, desc: &str) -> bool {
    matches!(
        (name, desc),
        ("getClass", "()Ljava/lang/Class;")
            | ("hashCode", "()I")
            | ("length", "()I")
            | ("charAt", "(I)C")
            | ("isEmpty", "()Z")
            | ("arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V")
            | ("append", "(Ljava/lang/String;)Ljava/lang/StringBuilder;")
            | ("append", "(I)Ljava/lang/StringBuilder;")
            | ("append", "(C)Ljava/lang/StringBuilder;")
            | ("append", "(J)Ljava/lang/StringBuilder;")
            | ("append", "(Z)Ljava/lang/StringBuilder;")
            | ("append", "(Ljava/lang/Object;)Ljava/lang/StringBuilder;")
            | ("toString", "()Ljava/lang/String;")
            | ("valueOf", "(I)Ljava/lang/Integer;")
            | ("intValue", "()I")
            | ("parseInt", "(Ljava/lang/String;)I")
            | ("valueOf", "(J)Ljava/lang/Long;")
            | ("longValue", "()J")
            | ("parseLong", "(Ljava/lang/String;)J")
            | ("abs", "(I)I")
            | ("abs", "(J)J")
            | ("abs", "(D)D")
            | ("min", "(II)I")
            | ("max", "(II)I")
            | ("min", "(JJ)J")
            | ("max", "(JJ)J")
            | ("sqrt", "(D)D")
            | ("equals", "(Ljava/lang/Object;)Z")
    )
}

/// `true` if `kind` names a *static* JDK method (`invokestatic` target with
/// no receiver), `false` for an instance method (`invokevirtual`/
/// `invokeinterface` target whose `args[0]` is the receiver).
///
/// The interpreter uses this to pick the correct argument-pop helper and to
/// avoid caching an instance intrinsic on an `invokespecial` site.
pub fn is_static(kind: InterpIntrinsic) -> bool {
    use InterpIntrinsic::*;
    matches!(
        kind,
        SystemArraycopy
            | ThreadOnSpinWait
            | ThreadCurrentThread
            | IntegerValueOf
            | IntegerParseInt
            | LongValueOf
            | LongParseLong
            | MathAbsInt
            | MathAbsLong
            | MathAbsDouble
            | MathMinInt
            | MathMaxInt
            | MathMinLong
            | MathMaxLong
            | MathSqrt
    )
}

/// `true` for the `java/lang/StringBuilder` members of the table — the
/// entries whose handlers are the `native_sb_*` bodies, i.e. the bodies of
/// registrations the `--jdk-only` registry REFUSES (every builder native is a
/// `SyntheticStub` there).
pub fn is_string_builder_entry(kind: InterpIntrinsic) -> bool {
    use InterpIntrinsic::*;
    matches!(
        kind,
        StringBuilderAppendString
            | StringBuilderAppendInt
            | StringBuilderAppendChar
            | StringBuilderAppendLong
            | StringBuilderAppendBool
            | StringBuilderAppendObject
            | StringBuilderToString
            | StringBuilderLength
    )
}

/// `CRATONVM_STRICT_SB_BYTECODE` (JIT round 16, lane strings, STR16-5;
/// default OFF until a startup A/B). Under `--jdk-only`:
///
/// * the interpreter's inline-cache fill does not serve a `StringBuilder`
///   entry of this table ([`strict_policy_declines`]) — the table is keyed on
///   the triple alone, so it resurrected the `native_sb_*` bodies whose
///   registrations the strict policy refused, and a cached interpreted
///   `sb.append(obj)` lost its `StringBuilder.append` frame; and
/// * `String.valueOf(Object)`'s `Intrinsic` registration is left out
///   (`lang_math::register_wrapper_natives`), so the JDK body — and its
///   `String.valueOf` stack frame — runs.
///
/// Since wave 9 (lane sbtrace) the lost frames no longer depend on this
/// switch: `native_sb_append_object` and `native_string_value_of_object` run
/// the JDK bodies for a `toString()` upcall in both modes
/// (`CRATONVM_TOSTRING_UPCALL_FRAMES`). What this switch still decides is the
/// policy: whether a refused builder body runs at all.
///
/// Read at registration and at the interpreter's two table doors, both of
/// which decide once per call site (see [`strict_policy_declines`]).
pub fn jdk_only_sb_bytecode_enabled() -> bool {
    // JIT round 17 triage: default ON (`=0` kill switch). Spring Boot sample
    // startup, 5 interleaved reps: 19014 -> 19873 ms (within the host spread).
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_STRICT_SB_BYTECODE")
}

/// Whether the interpreter must NOT serve intrinsic `kind` at a call site:
/// `--jdk-only` (`jdk_only`) and either a `StringBuilder` entry with the
/// `CRATONVM_STRICT_SB_BYTECODE` switch armed, or (round 17 wave 3, lane
/// compatbridge) one of the [`is_reviewed_string_entry`] rows with the opt-in
/// `CRATONVM_STRICT_STRING_TABLE_BYTECODE` armed. The flags are read last, so
/// a `--compatible` caller and every other kind pay nothing for them.
///
/// Every door the table has asks THIS predicate, and only at link time
/// (JIT round 17 wave 1, lane strings, which added the second):
///
/// * the inline-cache fill (`dispatch_virtual.rs`
///   `populate_virtual_invoke_cache`): a declined kind falls through to the
///   ordinary fill, which caches the JDK body;
/// * the vtable fast path's shadowing gate
///   (`execute_invokevirtual_vtable_fast`): a declined kind is no intrinsic,
///   so that path dispatches the JDK body itself and caches it, instead of
///   ceding the site to the slow path once per miss.
///
/// Neither runs per call: both are reached only while the site's inline cache
/// misses, and the entry either fills caches the answer. The table is not
/// consulted anywhere else that serves a call (the `invokestatic` fills only
/// admit static kinds, and no builder kind is static; the JIT never calls
/// these handlers -- its builder intrinsics are inline code over the JDK
/// fields, and its native doors only see registry rows, which the strict
/// registry has already refused).
pub fn strict_policy_declines(kind: InterpIntrinsic, jdk_only: bool) -> bool {
    if !jdk_only {
        return false;
    }
    (is_string_builder_entry(kind) && jdk_only_sb_bytecode_enabled())
        || (is_reviewed_string_entry(kind) && jdk_only_string_table_bytecode_enabled())
}

/// `true` for the three `java/lang/String` members of the table
/// (`length()I`, `charAt(I)C`, `isEmpty()Z`).
///
/// # Classification: reviewed interpreter intrinsics (JIT round 17 wave 3, lane compatbridge)
///
/// These rows have NO registry row on a real JDK, in either mode:
/// `NativeMethodRegistry::register` drops every `Bridge` on `java/lang/String`
/// except `intern()` when `drop_real_layout_synthetic` is set, which
/// `vm_init`'s real-JDK arms always set. (The round-17 page that called them
/// "`Bridge`s in front of JDK bytecode" read the ambient
/// `set_category(Bridge)` at `register_essential_natives_with_shims`; the
/// registration it found never survives.) So the table is the only door that
/// serves these bodies, and it serves them as the contract's §1.4 reviewed
/// exception, recorded in `docs/feature-designs/native-dispatch-architecture.md`
/// ("String rows: classification"):
///
/// * value-identical to the JDK 25 bodies on the compact `byte[]` + `coder`
///   layout: `length` is `value.length >> coder`, `isEmpty` is
///   `value.length == 0`, `charAt` indexes the code unit;
/// * the one exceptional path, an out-of-range `charAt`, raises through the
///   real `String.charAt` body (`CRATONVM_BOUNDS_RAISE_BYTECODE`), so the
///   exception type, message and frames are HotSpot's;
/// * faster where the table applies (cached INTERPRETED call sites only; the
///   JIT never calls these handlers): one crossing instead of two to five
///   interpreted frames (`length` -> `coder`; `charAt` -> `isLatin1` ->
///   `StringLatin1.charAt` -> `checkIndex`).
///
/// They stay ON in both modes. `CRATONVM_STRICT_STRING_TABLE_BYTECODE`
/// (default OFF) declines them under `--jdk-only` so the orchestrator can A/B
/// the reading (`--nojit` `R17StringsSb` / `R17CompatStrings`).
pub fn is_reviewed_string_entry(kind: InterpIntrinsic) -> bool {
    use InterpIntrinsic::*;
    matches!(kind, StringLength | StringCharAt | StringIsEmpty)
}

/// `CRATONVM_STRICT_STRING_TABLE_BYTECODE` (JIT round 17 wave 3, lane
/// compatbridge; default OFF, a performance A/B): under `--jdk-only` the
/// interpreter's two table doors do not serve the [`is_reviewed_string_entry`]
/// rows, so a cached interpreted `String.length` / `charAt` / `isEmpty` site
/// runs (and caches) the JDK body. Read at the doors' link time only, through
/// [`strict_policy_declines`].
pub fn jdk_only_string_table_bytecode_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_STRICT_STRING_TABLE_BYTECODE")
}

/// Steady-state dispatch — no lock, no hashmap, no descriptor parse.
///
/// `args` follows the native-registry convention: for an INSTANCE method it
/// is `[receiver, param0, ...]`; for a STATIC method it is `[param0, ...]`.
pub fn dispatch(
    kind: InterpIntrinsic,
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use InterpIntrinsic::*;
    match kind {
        // Object
        ObjectGetClass => object::intrinsic_object_get_class(ctx, args),
        ObjectHashCode => object::intrinsic_object_hash_code(ctx, args),
        // String
        StringLength => string::intrinsic_string_length(ctx, args),
        StringCharAt => string::intrinsic_string_char_at(ctx, args),
        StringIsEmpty => string::intrinsic_string_is_empty(ctx, args),
        // System
        SystemArraycopy => system::intrinsic_system_arraycopy(ctx, args),
        // Thread
        ThreadOnSpinWait => {
            // Same body as the registry native: a CPU pause hint, nothing
            // observable. The interpreter's invokestatic fast path answers
            // this without reaching here at all; this arm covers any caller
            // that still routes through the generic dispatch.
            std::hint::spin_loop();
            Ok(None)
        }
        ThreadCurrentThread => {
            // Same answer as the registry native. The interpreter's
            // invokestatic fast path serves this without reaching here once
            // the thread's mirror exists; this arm covers the first call (which
            // must run `current_thread_object`'s allocating slow path) and any
            // caller still on the generic route.
            Ok(Some(Value::Object(Some(ctx.current_thread_object()))))
        }
        // StringBuilder
        StringBuilderAppendString => stringbuilder::intrinsic_sb_append_string(ctx, args),
        StringBuilderAppendInt => stringbuilder::intrinsic_sb_append_int(ctx, args),
        StringBuilderAppendChar => stringbuilder::intrinsic_sb_append_char(ctx, args),
        StringBuilderAppendLong => stringbuilder::intrinsic_sb_append_long(ctx, args),
        StringBuilderAppendBool => stringbuilder::intrinsic_sb_append_bool(ctx, args),
        StringBuilderAppendObject => stringbuilder::intrinsic_sb_append_object(ctx, args),
        StringBuilderToString => stringbuilder::intrinsic_sb_to_string(ctx, args),
        StringBuilderLength => stringbuilder::intrinsic_sb_length(ctx, args),
        // Integer
        IntegerValueOf => integer::intrinsic_integer_value_of(ctx, args),
        IntegerIntValue => integer::intrinsic_integer_int_value(ctx, args),
        IntegerParseInt => integer::intrinsic_integer_parse_int(ctx, args),
        // Long
        LongValueOf => long::intrinsic_long_value_of(ctx, args),
        LongLongValue => long::intrinsic_long_long_value(ctx, args),
        LongParseLong => long::intrinsic_long_parse_long(ctx, args),
        // Math
        MathAbsInt => math::intrinsic_math_abs_int(ctx, args),
        MathAbsLong => math::intrinsic_math_abs_long(ctx, args),
        MathAbsDouble => math::intrinsic_math_abs_double(ctx, args),
        MathMinInt => math::intrinsic_math_min_int(ctx, args),
        MathMaxInt => math::intrinsic_math_max_int(ctx, args),
        MathMinLong => math::intrinsic_math_min_long(ctx, args),
        MathMaxLong => math::intrinsic_math_max_long(ctx, args),
        MathSqrt => math::intrinsic_math_sqrt(ctx, args),
        // Records
        RecordHashCode => record::intrinsic_record_hash_code(ctx, args),
        RecordEquals => record::intrinsic_record_equals(ctx, args),
    }
}

/// Return a directly-callable [`NativeCallback`](cratonvm_native_api::NativeCallback)
/// (a plain `fn` pointer) that dispatches `kind`.
///
/// The interpreter inline cache stores a `NativeCallback` so an intrinsic
/// entry is dispatched through the exact same `safe_native_call` machinery
/// (arg pinning, panic catch, JNI exception drain) as a `Native` entry. A
/// `NativeCallback` cannot close over `kind`, so we hand out one zero-cost
/// trampoline `fn` per intrinsic; each forwards to [`dispatch`]. This keeps
/// the steady-state path going through the real handler code in the group
/// submodules — the differential tests exercise exactly this path.
pub fn callback_for(kind: InterpIntrinsic) -> cratonvm_native_api::NativeCallback {
    use InterpIntrinsic::*;
    macro_rules! tramp {
        ($k:expr) => {{
            fn cb(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                dispatch($k, ctx, args)
            }
            cb as cratonvm_native_api::NativeCallback
        }};
    }
    match kind {
        ObjectGetClass => tramp!(ObjectGetClass),
        ObjectHashCode => tramp!(ObjectHashCode),
        StringLength => tramp!(StringLength),
        StringCharAt => tramp!(StringCharAt),
        StringIsEmpty => tramp!(StringIsEmpty),
        SystemArraycopy => tramp!(SystemArraycopy),
        ThreadOnSpinWait => tramp!(ThreadOnSpinWait),
        ThreadCurrentThread => tramp!(ThreadCurrentThread),
        StringBuilderAppendString => tramp!(StringBuilderAppendString),
        StringBuilderAppendInt => tramp!(StringBuilderAppendInt),
        StringBuilderAppendChar => tramp!(StringBuilderAppendChar),
        StringBuilderAppendLong => tramp!(StringBuilderAppendLong),
        StringBuilderAppendBool => tramp!(StringBuilderAppendBool),
        StringBuilderAppendObject => tramp!(StringBuilderAppendObject),
        StringBuilderToString => tramp!(StringBuilderToString),
        StringBuilderLength => tramp!(StringBuilderLength),
        IntegerValueOf => tramp!(IntegerValueOf),
        IntegerIntValue => tramp!(IntegerIntValue),
        IntegerParseInt => tramp!(IntegerParseInt),
        LongValueOf => tramp!(LongValueOf),
        LongLongValue => tramp!(LongLongValue),
        LongParseLong => tramp!(LongParseLong),
        MathAbsInt => tramp!(MathAbsInt),
        MathAbsLong => tramp!(MathAbsLong),
        MathAbsDouble => tramp!(MathAbsDouble),
        MathMinInt => tramp!(MathMinInt),
        MathMaxInt => tramp!(MathMaxInt),
        MathMinLong => tramp!(MathMinLong),
        MathMaxLong => tramp!(MathMaxLong),
        MathSqrt => tramp!(MathSqrt),
        RecordHashCode => tramp!(RecordHashCode),
        RecordEquals => tramp!(RecordEquals),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn lookup_resolves_known_methods() {
        assert_eq!(
            lookup("java/lang/String", "length", "()I"),
            Some(InterpIntrinsic::StringLength)
        );
        assert_eq!(
            lookup(
                "java/lang/System",
                "arraycopy",
                "(Ljava/lang/Object;ILjava/lang/Object;II)V"
            ),
            Some(InterpIntrinsic::SystemArraycopy)
        );
        assert_eq!(
            lookup("java/lang/Math", "max", "(JJ)J"),
            Some(InterpIntrinsic::MathMaxLong)
        );
    }

    #[test]
    fn lookup_is_descriptor_exact() {
        // Wrong descriptor for an otherwise-known (class,name) must miss.
        assert_eq!(lookup("java/lang/String", "length", "()J"), None);
        // append overloads must resolve independently.
        assert_eq!(
            lookup(
                "java/lang/StringBuilder",
                "append",
                "(I)Ljava/lang/StringBuilder;"
            ),
            Some(InterpIntrinsic::StringBuilderAppendInt)
        );
        assert_eq!(
            lookup(
                "java/lang/StringBuilder",
                "append",
                "(C)Ljava/lang/StringBuilder;"
            ),
            Some(InterpIntrinsic::StringBuilderAppendChar)
        );
    }

    #[test]
    fn lookup_misses_unknown() {
        assert_eq!(
            lookup(
                "java/lang/String",
                "concat",
                "(Ljava/lang/String;)Ljava/lang/String;"
            ),
            None
        );
        assert_eq!(lookup("com/example/Foo", "bar", "()V"), None);
    }

    #[test]
    fn signature_prefilter_is_conservative() {
        assert!(might_have_method_descriptor("length", "()I"));
        assert!(might_have_method_descriptor("max", "(JJ)J"));
        assert!(might_have_method_descriptor(
            "append",
            "(Ljava/lang/Object;)Ljava/lang/StringBuilder;"
        ));
        assert!(!might_have_method_descriptor(
            "concat",
            "(Ljava/lang/String;)Ljava/lang/String;"
        ));
        assert!(!might_have_method_descriptor("length", "()J"));
    }

    // `TABLE` (the module's, through `use super::*`) is the second copy of
    // `lookup`'s arms the tests below cross-check; since j27 CB17-6 it is also
    // what the native census publishes.

    /// `is_static` and [`lookup`] must agree for every entry: the interpreter
    /// picks its argument-pop helper from `is_static`, so a wrong answer reads
    /// the receiver as `param0` or drops it.
    #[test]
    fn staticness_agrees_with_the_table() {
        for &(class, name, desc, expect_static) in TABLE {
            let kind = lookup(class, name, desc)
                .unwrap_or_else(|| panic!("{class} {name} {desc} must resolve"));
            assert_eq!(
                is_static(kind),
                expect_static,
                "{class} {name} {desc} staticness"
            );
        }
    }

    /// The prefilter's real invariant, G9-1: it may drop a STATIC entry (see
    /// its doc), but it must never drop an INSTANCE one — the virtual dispatch
    /// path consults it before `lookup` and a `false` there is final.
    #[test]
    fn every_instance_entry_is_admitted() {
        for &(class, name, desc, is_stat) in TABLE {
            if is_stat {
                continue;
            }
            assert!(
                might_have_method_descriptor(name, desc),
                "{class} {name} {desc} resolves through lookup() but the virtual \
                 prefilter rejects it, so the fast path is unreachable"
            );
        }
    }

    /// The two Thread entries really are the whole of the prefilter's blind
    /// spot. If this starts failing, a new static intrinsic was added and the
    /// doc on `might_have_method_descriptor` needs re-reading, not deleting.
    #[test]
    fn the_prefilter_blind_spot_is_exactly_the_two_thread_statics() {
        let blind: Vec<&str> = TABLE
            .iter()
            .filter(|(_, name, desc, _)| !might_have_method_descriptor(name, desc))
            .map(|(_, name, _, _)| *name)
            .collect();
        assert_eq!(blind, vec!["onSpinWait", "currentThread"]);
    }

    /// Guards the table above against drifting behind `lookup`.
    ///
    /// `dispatch` has one arm per `InterpIntrinsic`; the two record kinds are
    /// not in `lookup` at all (they are produced per-class by
    /// `vm::runtime::interpreter::dispatch_static::record_object_intrinsic`),
    /// so the expected count is the enum's size minus those two.
    #[test]
    fn the_table_is_complete() {
        assert_eq!(TABLE.len(), 30, "lookup() arm count");
        assert_eq!(
            lookup("java/lang/Thread", "onSpinWait", "()V"),
            Some(InterpIntrinsic::ThreadOnSpinWait)
        );
        assert_eq!(lookup("java/lang/Record", "hashCode", "()I"), None);
        assert_eq!(
            lookup("java/lang/Object", "equals", "(Ljava/lang/Object;)Z"),
            None
        );
    }

    /// STR16-5: the strict-policy filter names exactly the `StringBuilder`
    /// rows of the table (whose `native_sb_*` bodies the `--jdk-only`
    /// registry refuses), and never declines anything under `--compatible`.
    #[test]
    fn strict_policy_filter_is_exactly_the_string_builder_rows() {
        for &(class, name, desc, _) in TABLE {
            let kind = lookup(class, name, desc).expect("table row resolves");
            assert_eq!(
                is_string_builder_entry(kind),
                class == "java/lang/StringBuilder",
                "{class}.{name}{desc}"
            );
            assert!(
                !strict_policy_declines(kind, false),
                "--compatible never declines {class}.{name}{desc}"
            );
            if !is_string_builder_entry(kind) && !is_reviewed_string_entry(kind) {
                assert!(
                    !strict_policy_declines(kind, true),
                    "only builder and String rows are declined: {class}.{name}{desc}"
                );
            }
        }
        // With the switch armed in this environment, every builder row is
        // declined under `--jdk-only`; unarmed, none is.
        let armed = jdk_only_sb_bytecode_enabled();
        for &(class, name, desc, _) in TABLE {
            let kind = lookup(class, name, desc).expect("table row resolves");
            if is_string_builder_entry(kind) {
                assert_eq!(
                    strict_policy_declines(kind, true),
                    armed,
                    "{class}.{name}{desc}"
                );
            }
        }
    }

    /// Round 17 wave 3 (lane compatbridge): the reviewed `String` rows are
    /// exactly the three `java/lang/String` members of the table, never
    /// declined under `--compatible`, and declined under `--jdk-only` exactly
    /// when `CRATONVM_STRICT_STRING_TABLE_BYTECODE` is armed.
    #[test]
    fn reviewed_string_rows_are_exactly_the_string_members() {
        let armed = jdk_only_string_table_bytecode_enabled();
        for &(class, name, desc, _) in TABLE {
            let kind = lookup(class, name, desc).expect("table row resolves");
            assert_eq!(
                is_reviewed_string_entry(kind),
                class == "java/lang/String",
                "{class}.{name}{desc}"
            );
            if is_reviewed_string_entry(kind) {
                assert!(!strict_policy_declines(kind, false), "{class}.{name}{desc}");
                assert_eq!(
                    strict_policy_declines(kind, true),
                    armed,
                    "{class}.{name}{desc}"
                );
            }
        }
    }
}
