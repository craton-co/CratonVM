// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Interpreter intrinsic identity.
//!
//! `InterpIntrinsic` is the enum tag for a hot JDK method that the interpreter
//! can dispatch through a direct fast path instead of the general native
//! registry (`RwLock` + descriptor parse + `FxHashMap` probe on every call).
//!
//! The tag is resolved **once**, at inline-cache fill time, by
//! `cratonvm_native_builtins::intrinsics::lookup`, and stored in the
//! `CachedInvokeTarget::Intrinsic` cache entry. Steady-state dispatch is a
//! plain `match` over this enum — see
//! `cratonvm_native_builtins::intrinsics::dispatch`.
//!
//! It lives in `cratonvm-native-api` (rather than `vm` or `native-builtins`)
//! so `classloading` (the cache entry), `native-builtins` (the handlers), and
//! `vm` (the interpreter integration) can all name the same type.
//!
//! See `feature_roadmap_interpreter_intrinsic_table.md` and
//! `intrinsic_table_contract.md`.

/// One variant per supported interpreter intrinsic. Each maps to exactly one
/// `(class, name, descriptor)` triple and one handler function — the mapping
/// is fixed in `cratonvm_native_builtins::intrinsics::{lookup, dispatch}`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum InterpIntrinsic {
    // java/lang/Object
    ObjectGetClass,
    ObjectHashCode,
    // java/lang/String
    StringLength,
    StringCharAt,
    StringIsEmpty,
    // java/lang/System
    SystemArraycopy,
    // java/lang/StringBuilder
    StringBuilderAppendString,
    StringBuilderAppendInt,
    StringBuilderAppendChar,
    StringBuilderAppendLong,
    StringBuilderAppendBool,
    StringBuilderAppendObject,
    StringBuilderToString,
    StringBuilderLength,
    // java/lang/Integer
    IntegerValueOf,
    IntegerIntValue,
    IntegerParseInt,
    // java/lang/Long
    LongValueOf,
    LongLongValue,
    LongParseLong,
    // java/lang/Thread
    //
    // `onSpinWait` is unlike every other entry here: it is not *dispatched* at
    // all. The JDK declares it `@IntrinsicCandidate public static void
    // onSpinWait() {}` — an EMPTY body that HotSpot lowers to a single x86
    // PAUSE — so the interpreter answers the call site inline (a `spin_loop()`
    // hint and a pc advance) instead of paying `safe_native_call`. The tag
    // exists so the inline cache can recognise the site once and take that
    // path; `dispatch`/`callback_for` still handle it for any caller that goes
    // through the generic route.
    ThreadOnSpinWait,
    /// `Thread.currentThread()` — the same shape: answered inline, not
    /// dispatched.
    ///
    /// Once a thread's `java_thread_obj` mirror exists it is a single field
    /// read, it is already a GC root, and returning it cannot allocate,
    /// collect or throw — so entering `safe_native_call` buys nothing and
    /// costs ~400 ns (measured: `probes/NativeShapeProbe.java`). Worth
    /// singling out because the JDK calls it constantly: **twice per
    /// uncontended `ReentrantLock.lock()`/`unlock()` pair** (censused with
    /// `--dump-native-registry`, `probes/LockNativeCensusProbe.java`), plus
    /// every AQS ownership check and thread-local lookup.
    ///
    /// The interpreter falls through to the ordinary path when the mirror has
    /// not been created yet — that first call must run the allocating slow
    /// path in `current_thread_object`.
    ThreadCurrentThread,
    // java/lang/Math
    MathAbsInt,
    MathAbsLong,
    MathAbsDouble,
    MathMinInt,
    MathMaxInt,
    MathMinLong,
    MathMaxLong,
    MathSqrt,
    // Records (JEP 395) — the javac-generated `hashCode`/`equals` bodies.
    //
    // Unlike every other variant these are NOT keyed on a fixed
    // `(class, name, descriptor)` triple: they apply to any record class whose
    // body is the generated `invokedynamic ObjectMethods.bootstrap` shape, so
    // `lookup` never returns them. The interpreter installs them directly from
    // its own record check at inline-cache fill time (one call site per record
    // class, and the entry carries the same receiver-class guard as every
    // other virtual intrinsic). Without this the bodies stay interpreter-only
    // forever — the x64 backend lowers `invokedynamic` to an unconditional
    // deopt — which makes every `HashMap`/`HashSet` operation keyed by a
    // record three orders of magnitude slower than HotSpot.
    RecordHashCode,
    RecordEquals,
}
