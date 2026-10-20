// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Windowed `ScopedMemoryAccess` twins (round nio20 wave 3, lane L5), default
//! ON since nio20 wave 5 (kill switch `CRATONVM_FFM_WINDOWED_SCOPED_TWINS=0`,
//! [`crate::ffm_fast::windowed_scoped_twins_enabled`]).
//!
//! # What this closes
//!
//! A shared close handshakes (`panama::pe_close_handshake`) only with accesses
//! that hold an [`crate::ffm_fast::AccessWindow`], and then runs the session's
//! Java cleanup actions (a `FileChannel.map(.., arena)` unmap, a user
//! `MemorySegment.reinterpret(.., arena, cleanup)` free). The scalar plain /
//! unaligned and the four bulk `*Internal` bodies are windowed intrinsics in
//! `lib.rs`; every OTHER session-checked body -- the volatile / acquire /
//! release / opaque accessors, the CAS, exchange and `getAndOp` families, the
//! `float` / `double` / `boolean` plain accessors, and the mapped-memory
//! `isLoaded` / `load` / `unload` / `force` bodies -- is still a bytecode body
//! that may be past its `checkValidStateRaw()` when the cleanup action frees
//! what it is about to touch (`r20-shared-close-java-cleanup-actions-race-
//! unwindowed-scoped-bodies-20261007`).
//!
//! # Why the body is the method's own bytecode
//!
//! There are 221 such bodies (javap -p -s of JDK 25's `ScopedMemoryAccess`,
//! minus the 18 scalar and 4 bulk rows registered in `lib.rs`), each
//! `try { if (s != null) s.checkValidStateRaw(); [return] UNSAFE.<op>(..); }
//! finally { reachabilityFence(s); }`. Re-implementing each `Unsafe` operation
//! here would be 221 chances to diverge from the JDK. Instead each callback
//! opens the window and runs exactly that bytecode
//! (`invoke_special_bytecode_only`, which skips the native registry so it
//! cannot re-enter itself), so check order, exception types (the
//! `ScopedAccessError` the public wrapper translates), memory ordering and
//! return values are the bytecode's by construction. That is the review that
//! lets these rows be `NativeKind::Intrinsic` (AGENTS.md): the native adds a
//! window and nothing else.
//!
//! # The window spans Java code
//!
//! Unlike the Rust intrinsics, the window here is held while the interpreter
//! runs a few bytecodes. `InFlight::wait` already tolerates an accessor that is
//! suspended at a safepoint or allocating an exception (the closer waits in a
//! blocking region), and none of these bodies takes a lock or blocks. The one
//! shape that could stall a close is a body whose first execution must
//! initialise a class whose `<clinit>` the CLOSING thread is running; a close
//! from inside a static initialiser racing that first access is not a shape
//! any JDK caller produces.
//!
//! # Cost, and why it is default ON anyway
//!
//! One native frame and one interpreted body per access, and a registered
//! native is a `native-shadow` refusal for the optimizing tier's splice, so a
//! compiled `VarHandle` CAS over a segment or a direct buffer pays the round
//! trip on every call. The population is narrow: the JDK reaches these rows
//! only through `VarHandle` volatile / acquire / release / opaque / CAS /
//! `getAndOp` access modes over a `MemorySegment` or a direct `ByteBuffer`
//! view, `MappedMemorySegment` / `MappedByteBuffer` `load` / `isLoaded` /
//! `unload` / `force` (bulk), and the plain float / double / boolean accessors.
//! The four plain float / double rows (the per-element path of a native-order
//! direct `FloatBuffer` / `DoubleBuffer`) are NOT registered: a native there
//! measured ~1.8x slower than the compiled bytecode (see
//! `UNTWINNED_PLAIN_FP_ROWS`), so 217 of the 221 rows are twinned. Plain `get`/`put` of every integral width and every
//! bulk copy / fill / mismatch were already windowed Rust intrinsics. Against
//! that, the race these rows close is memory safety (a racing shared close
//! unmapping or freeing under the access). Price it with
//! `tools/probes/Nio20ScopedTwins.java` (bench rows, `=0` against unset).

use cratonvm_native_api::{NativeCallback, NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

const SCOPED_MEMORY_ACCESS: &str = "jdk/internal/misc/ScopedMemoryAccess";

/// Run `ScopedMemoryAccess.<method><descriptor>`'s own bytecode on `args`
/// (`args[0]` the `ScopedMemoryAccess` receiver) inside one access window.
fn run_twin_in_window(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> MethodCallResult {
    let _window = crate::ffm_fast::AccessWindow::open();
    ctx.invoke_special_bytecode_only(SCOPED_MEMORY_ACCESS, method, descriptor, args)
}

macro_rules! windowed_twins {
    ($( $f:ident = $m:literal, $d:literal; )*) => {
        $(
            #[allow(non_snake_case)]
            fn $f(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                run_twin_in_window(ctx, args, $m, $d)
            }
        )*

        /// Every windowed twin: `(method, descriptor, callback)`.
        static WINDOWED_TWINS: &[(&str, &str, NativeCallback)] = &[
            $( ($m, $d, $f as NativeCallback), )*
        ];
    };
}

windowed_twins! {
    w_isLoadedInternal = "isLoadedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/access/foreign/MappedMemoryUtilsProxy;JZJ)Z";
    w_loadInternal = "loadInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/access/foreign/MappedMemoryUtilsProxy;JZJ)V";
    w_unloadInternal = "unloadInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/access/foreign/MappedMemoryUtilsProxy;JZJ)V";
    w_forceInternal = "forceInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/access/foreign/MappedMemoryUtilsProxy;Ljava/io/FileDescriptor;JZJJ)V";
    w_getBooleanInternal = "getBooleanInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)Z";
    w_putBooleanInternal = "putBooleanInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)V";
    w_getBooleanVolatileInternal = "getBooleanVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)Z";
    w_putBooleanVolatileInternal = "putBooleanVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)V";
    w_getBooleanAcquireInternal = "getBooleanAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)Z";
    w_putBooleanReleaseInternal = "putBooleanReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)V";
    w_getBooleanOpaqueInternal = "getBooleanOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)Z";
    w_putBooleanOpaqueInternal = "putBooleanOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)V";
    w_compareAndSetBooleanInternal = "compareAndSetBooleanInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZZ)Z";
    w_compareAndExchangeBooleanInternal = "compareAndExchangeBooleanInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZZ)Z";
    w_compareAndExchangeBooleanAcquireInternal = "compareAndExchangeBooleanAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZZ)Z";
    w_compareAndExchangeBooleanReleaseInternal = "compareAndExchangeBooleanReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZZ)Z";
    w_weakCompareAndSetBooleanPlainInternal = "weakCompareAndSetBooleanPlainInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZZ)Z";
    w_weakCompareAndSetBooleanInternal = "weakCompareAndSetBooleanInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZZ)Z";
    w_weakCompareAndSetBooleanAcquireInternal = "weakCompareAndSetBooleanAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZZ)Z";
    w_weakCompareAndSetBooleanReleaseInternal = "weakCompareAndSetBooleanReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZZ)Z";
    w_getAndSetBooleanInternal = "getAndSetBooleanInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)Z";
    w_getAndSetBooleanAcquireInternal = "getAndSetBooleanAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)Z";
    w_getAndSetBooleanReleaseInternal = "getAndSetBooleanReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)Z";
    w_getAndBitwiseOrBooleanInternal = "getAndBitwiseOrBooleanInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)Z";
    w_getAndBitwiseOrBooleanAcquireInternal = "getAndBitwiseOrBooleanAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)Z";
    w_getAndBitwiseOrBooleanReleaseInternal = "getAndBitwiseOrBooleanReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)Z";
    w_getAndBitwiseAndBooleanInternal = "getAndBitwiseAndBooleanInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)Z";
    w_getAndBitwiseAndBooleanAcquireInternal = "getAndBitwiseAndBooleanAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)Z";
    w_getAndBitwiseAndBooleanReleaseInternal = "getAndBitwiseAndBooleanReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)Z";
    w_getAndBitwiseXorBooleanInternal = "getAndBitwiseXorBooleanInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)Z";
    w_getAndBitwiseXorBooleanAcquireInternal = "getAndBitwiseXorBooleanAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)Z";
    w_getAndBitwiseXorBooleanReleaseInternal = "getAndBitwiseXorBooleanReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)Z";
    w_getByteVolatileInternal = "getByteVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)B";
    w_putByteVolatileInternal = "putByteVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)V";
    w_getByteAcquireInternal = "getByteAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)B";
    w_putByteReleaseInternal = "putByteReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)V";
    w_getByteOpaqueInternal = "getByteOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)B";
    w_putByteOpaqueInternal = "putByteOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)V";
    w_compareAndSetByteInternal = "compareAndSetByteInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JBB)Z";
    w_compareAndExchangeByteInternal = "compareAndExchangeByteInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JBB)B";
    w_compareAndExchangeByteAcquireInternal = "compareAndExchangeByteAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JBB)B";
    w_compareAndExchangeByteReleaseInternal = "compareAndExchangeByteReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JBB)B";
    w_weakCompareAndSetBytePlainInternal = "weakCompareAndSetBytePlainInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JBB)Z";
    w_weakCompareAndSetByteInternal = "weakCompareAndSetByteInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JBB)Z";
    w_weakCompareAndSetByteAcquireInternal = "weakCompareAndSetByteAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JBB)Z";
    w_weakCompareAndSetByteReleaseInternal = "weakCompareAndSetByteReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JBB)Z";
    w_getAndSetByteInternal = "getAndSetByteInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndSetByteAcquireInternal = "getAndSetByteAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndSetByteReleaseInternal = "getAndSetByteReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndAddByteInternal = "getAndAddByteInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndAddByteAcquireInternal = "getAndAddByteAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndAddByteReleaseInternal = "getAndAddByteReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndBitwiseOrByteInternal = "getAndBitwiseOrByteInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndBitwiseOrByteAcquireInternal = "getAndBitwiseOrByteAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndBitwiseOrByteReleaseInternal = "getAndBitwiseOrByteReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndBitwiseAndByteInternal = "getAndBitwiseAndByteInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndBitwiseAndByteAcquireInternal = "getAndBitwiseAndByteAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndBitwiseAndByteReleaseInternal = "getAndBitwiseAndByteReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndBitwiseXorByteInternal = "getAndBitwiseXorByteInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndBitwiseXorByteAcquireInternal = "getAndBitwiseXorByteAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getAndBitwiseXorByteReleaseInternal = "getAndBitwiseXorByteReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)B";
    w_getShortVolatileInternal = "getShortVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)S";
    w_putShortVolatileInternal = "putShortVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)V";
    w_getShortAcquireInternal = "getShortAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)S";
    w_putShortReleaseInternal = "putShortReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)V";
    w_getShortOpaqueInternal = "getShortOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)S";
    w_putShortOpaqueInternal = "putShortOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)V";
    w_compareAndSetShortInternal = "compareAndSetShortInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSS)Z";
    w_compareAndExchangeShortInternal = "compareAndExchangeShortInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSS)S";
    w_compareAndExchangeShortAcquireInternal = "compareAndExchangeShortAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSS)S";
    w_compareAndExchangeShortReleaseInternal = "compareAndExchangeShortReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSS)S";
    w_weakCompareAndSetShortPlainInternal = "weakCompareAndSetShortPlainInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSS)Z";
    w_weakCompareAndSetShortInternal = "weakCompareAndSetShortInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSS)Z";
    w_weakCompareAndSetShortAcquireInternal = "weakCompareAndSetShortAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSS)Z";
    w_weakCompareAndSetShortReleaseInternal = "weakCompareAndSetShortReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSS)Z";
    w_getAndSetShortInternal = "getAndSetShortInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndSetShortAcquireInternal = "getAndSetShortAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndSetShortReleaseInternal = "getAndSetShortReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndAddShortInternal = "getAndAddShortInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndAddShortAcquireInternal = "getAndAddShortAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndAddShortReleaseInternal = "getAndAddShortReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndBitwiseOrShortInternal = "getAndBitwiseOrShortInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndBitwiseOrShortAcquireInternal = "getAndBitwiseOrShortAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndBitwiseOrShortReleaseInternal = "getAndBitwiseOrShortReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndBitwiseAndShortInternal = "getAndBitwiseAndShortInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndBitwiseAndShortAcquireInternal = "getAndBitwiseAndShortAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndBitwiseAndShortReleaseInternal = "getAndBitwiseAndShortReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndBitwiseXorShortInternal = "getAndBitwiseXorShortInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndBitwiseXorShortAcquireInternal = "getAndBitwiseXorShortAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getAndBitwiseXorShortReleaseInternal = "getAndBitwiseXorShortReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)S";
    w_getCharVolatileInternal = "getCharVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)C";
    w_putCharVolatileInternal = "putCharVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)V";
    w_getCharAcquireInternal = "getCharAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)C";
    w_putCharReleaseInternal = "putCharReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)V";
    w_getCharOpaqueInternal = "getCharOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)C";
    w_putCharOpaqueInternal = "putCharOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)V";
    w_compareAndSetCharInternal = "compareAndSetCharInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCC)Z";
    w_compareAndExchangeCharInternal = "compareAndExchangeCharInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCC)C";
    w_compareAndExchangeCharAcquireInternal = "compareAndExchangeCharAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCC)C";
    w_compareAndExchangeCharReleaseInternal = "compareAndExchangeCharReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCC)C";
    w_weakCompareAndSetCharPlainInternal = "weakCompareAndSetCharPlainInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCC)Z";
    w_weakCompareAndSetCharInternal = "weakCompareAndSetCharInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCC)Z";
    w_weakCompareAndSetCharAcquireInternal = "weakCompareAndSetCharAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCC)Z";
    w_weakCompareAndSetCharReleaseInternal = "weakCompareAndSetCharReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCC)Z";
    w_getAndSetCharInternal = "getAndSetCharInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndSetCharAcquireInternal = "getAndSetCharAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndSetCharReleaseInternal = "getAndSetCharReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndAddCharInternal = "getAndAddCharInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndAddCharAcquireInternal = "getAndAddCharAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndAddCharReleaseInternal = "getAndAddCharReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndBitwiseOrCharInternal = "getAndBitwiseOrCharInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndBitwiseOrCharAcquireInternal = "getAndBitwiseOrCharAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndBitwiseOrCharReleaseInternal = "getAndBitwiseOrCharReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndBitwiseAndCharInternal = "getAndBitwiseAndCharInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndBitwiseAndCharAcquireInternal = "getAndBitwiseAndCharAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndBitwiseAndCharReleaseInternal = "getAndBitwiseAndCharReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndBitwiseXorCharInternal = "getAndBitwiseXorCharInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndBitwiseXorCharAcquireInternal = "getAndBitwiseXorCharAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getAndBitwiseXorCharReleaseInternal = "getAndBitwiseXorCharReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)C";
    w_getIntVolatileInternal = "getIntVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)I";
    w_putIntVolatileInternal = "putIntVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)V";
    w_getIntAcquireInternal = "getIntAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)I";
    w_putIntReleaseInternal = "putIntReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)V";
    w_getIntOpaqueInternal = "getIntOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)I";
    w_putIntOpaqueInternal = "putIntOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)V";
    w_compareAndSetIntInternal = "compareAndSetIntInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JII)Z";
    w_compareAndExchangeIntInternal = "compareAndExchangeIntInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JII)I";
    w_compareAndExchangeIntAcquireInternal = "compareAndExchangeIntAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JII)I";
    w_compareAndExchangeIntReleaseInternal = "compareAndExchangeIntReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JII)I";
    w_weakCompareAndSetIntPlainInternal = "weakCompareAndSetIntPlainInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JII)Z";
    w_weakCompareAndSetIntInternal = "weakCompareAndSetIntInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JII)Z";
    w_weakCompareAndSetIntAcquireInternal = "weakCompareAndSetIntAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JII)Z";
    w_weakCompareAndSetIntReleaseInternal = "weakCompareAndSetIntReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JII)Z";
    w_getAndSetIntInternal = "getAndSetIntInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndSetIntAcquireInternal = "getAndSetIntAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndSetIntReleaseInternal = "getAndSetIntReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndAddIntInternal = "getAndAddIntInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndAddIntAcquireInternal = "getAndAddIntAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndAddIntReleaseInternal = "getAndAddIntReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndBitwiseOrIntInternal = "getAndBitwiseOrIntInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndBitwiseOrIntAcquireInternal = "getAndBitwiseOrIntAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndBitwiseOrIntReleaseInternal = "getAndBitwiseOrIntReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndBitwiseAndIntInternal = "getAndBitwiseAndIntInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndBitwiseAndIntAcquireInternal = "getAndBitwiseAndIntAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndBitwiseAndIntReleaseInternal = "getAndBitwiseAndIntReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndBitwiseXorIntInternal = "getAndBitwiseXorIntInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndBitwiseXorIntAcquireInternal = "getAndBitwiseXorIntAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getAndBitwiseXorIntReleaseInternal = "getAndBitwiseXorIntReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I";
    w_getLongVolatileInternal = "getLongVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)J";
    w_putLongVolatileInternal = "putLongVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)V";
    w_getLongAcquireInternal = "getLongAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)J";
    w_putLongReleaseInternal = "putLongReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)V";
    w_getLongOpaqueInternal = "getLongOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)J";
    w_putLongOpaqueInternal = "putLongOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)V";
    w_compareAndSetLongInternal = "compareAndSetLongInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJJ)Z";
    w_compareAndExchangeLongInternal = "compareAndExchangeLongInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJJ)J";
    w_compareAndExchangeLongAcquireInternal = "compareAndExchangeLongAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJJ)J";
    w_compareAndExchangeLongReleaseInternal = "compareAndExchangeLongReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJJ)J";
    w_weakCompareAndSetLongPlainInternal = "weakCompareAndSetLongPlainInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJJ)Z";
    w_weakCompareAndSetLongInternal = "weakCompareAndSetLongInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJJ)Z";
    w_weakCompareAndSetLongAcquireInternal = "weakCompareAndSetLongAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJJ)Z";
    w_weakCompareAndSetLongReleaseInternal = "weakCompareAndSetLongReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJJ)Z";
    w_getAndSetLongInternal = "getAndSetLongInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndSetLongAcquireInternal = "getAndSetLongAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndSetLongReleaseInternal = "getAndSetLongReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndAddLongInternal = "getAndAddLongInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndAddLongAcquireInternal = "getAndAddLongAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndAddLongReleaseInternal = "getAndAddLongReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndBitwiseOrLongInternal = "getAndBitwiseOrLongInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndBitwiseOrLongAcquireInternal = "getAndBitwiseOrLongAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndBitwiseOrLongReleaseInternal = "getAndBitwiseOrLongReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndBitwiseAndLongInternal = "getAndBitwiseAndLongInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndBitwiseAndLongAcquireInternal = "getAndBitwiseAndLongAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndBitwiseAndLongReleaseInternal = "getAndBitwiseAndLongReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndBitwiseXorLongInternal = "getAndBitwiseXorLongInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndBitwiseXorLongAcquireInternal = "getAndBitwiseXorLongAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getAndBitwiseXorLongReleaseInternal = "getAndBitwiseXorLongReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)J";
    w_getFloatInternal = "getFloatInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)F";
    w_putFloatInternal = "putFloatInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JF)V";
    w_getFloatVolatileInternal = "getFloatVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)F";
    w_putFloatVolatileInternal = "putFloatVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JF)V";
    w_getFloatAcquireInternal = "getFloatAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)F";
    w_putFloatReleaseInternal = "putFloatReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JF)V";
    w_getFloatOpaqueInternal = "getFloatOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)F";
    w_putFloatOpaqueInternal = "putFloatOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JF)V";
    w_compareAndSetFloatInternal = "compareAndSetFloatInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JFF)Z";
    w_compareAndExchangeFloatInternal = "compareAndExchangeFloatInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JFF)F";
    w_compareAndExchangeFloatAcquireInternal = "compareAndExchangeFloatAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JFF)F";
    w_compareAndExchangeFloatReleaseInternal = "compareAndExchangeFloatReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JFF)F";
    w_weakCompareAndSetFloatPlainInternal = "weakCompareAndSetFloatPlainInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JFF)Z";
    w_weakCompareAndSetFloatInternal = "weakCompareAndSetFloatInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JFF)Z";
    w_weakCompareAndSetFloatAcquireInternal = "weakCompareAndSetFloatAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JFF)Z";
    w_weakCompareAndSetFloatReleaseInternal = "weakCompareAndSetFloatReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JFF)Z";
    w_getAndSetFloatInternal = "getAndSetFloatInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JF)F";
    w_getAndSetFloatAcquireInternal = "getAndSetFloatAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JF)F";
    w_getAndSetFloatReleaseInternal = "getAndSetFloatReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JF)F";
    w_getAndAddFloatInternal = "getAndAddFloatInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JF)F";
    w_getAndAddFloatAcquireInternal = "getAndAddFloatAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JF)F";
    w_getAndAddFloatReleaseInternal = "getAndAddFloatReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JF)F";
    w_getDoubleInternal = "getDoubleInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)D";
    w_putDoubleInternal = "putDoubleInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JD)V";
    w_getDoubleVolatileInternal = "getDoubleVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)D";
    w_putDoubleVolatileInternal = "putDoubleVolatileInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JD)V";
    w_getDoubleAcquireInternal = "getDoubleAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)D";
    w_putDoubleReleaseInternal = "putDoubleReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JD)V";
    w_getDoubleOpaqueInternal = "getDoubleOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)D";
    w_putDoubleOpaqueInternal = "putDoubleOpaqueInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JD)V";
    w_compareAndSetDoubleInternal = "compareAndSetDoubleInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JDD)Z";
    w_compareAndExchangeDoubleInternal = "compareAndExchangeDoubleInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JDD)D";
    w_compareAndExchangeDoubleAcquireInternal = "compareAndExchangeDoubleAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JDD)D";
    w_compareAndExchangeDoubleReleaseInternal = "compareAndExchangeDoubleReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JDD)D";
    w_weakCompareAndSetDoublePlainInternal = "weakCompareAndSetDoublePlainInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JDD)Z";
    w_weakCompareAndSetDoubleInternal = "weakCompareAndSetDoubleInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JDD)Z";
    w_weakCompareAndSetDoubleAcquireInternal = "weakCompareAndSetDoubleAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JDD)Z";
    w_weakCompareAndSetDoubleReleaseInternal = "weakCompareAndSetDoubleReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JDD)Z";
    w_getAndSetDoubleInternal = "getAndSetDoubleInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JD)D";
    w_getAndSetDoubleAcquireInternal = "getAndSetDoubleAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JD)D";
    w_getAndSetDoubleReleaseInternal = "getAndSetDoubleReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JD)D";
    w_getAndAddDoubleInternal = "getAndAddDoubleInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JD)D";
    w_getAndAddDoubleAcquireInternal = "getAndAddDoubleAcquireInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JD)D";
    w_getAndAddDoubleReleaseInternal = "getAndAddDoubleReleaseInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JD)D";
}

// ---------------------------------------------------------------------------
// The four plain float / double rows stay UNtwinned (nio20 w5 fix-back, L5)
// ---------------------------------------------------------------------------
//
// `getFloatInternal` / `putFloatInternal` / `getDoubleInternal` /
// `putDoubleInternal` are what a native-order `DirectFloatBufferU` /
// `DirectDoubleBufferU` reaches on every `get` / `put` (JDK 25
// `Direct-X-Buffer.java.template`). Any registered native for these triples --
// the bytecode twin, or the raw-address scalar form wave 5 first tried --
// shadows the bytecode body: the optimizing tier no longer splices it
// (`native-shadow`), and the per-element access becomes a native crossing
// through the generic funnel (plus, for the scalar form, an argument `Vec`
// per call and a verdict publication). Measured on w5b (`Nio20ScopedTwins`,
// ns/op, twins on / off): fbuf 1189 / 666, dbuf 1528 / 981. Unlike the
// integral scalar rows, these four have no compiled fast path that would
// absorb the crossing, so they are NOT registered: the compiled JDK body runs
// as before. The cost is that a plain float / double access over a shared
// segment still races a Java cleanup action of a concurrent close
// (`r20-shared-close-java-cleanup-actions-race-unwindowed-scoped-bodies`,
// item 1b); closing it needs a compiled access window around the spliced body
// (proposal NIO20W3-L5-1), not a native.

/// The twin rows deliberately left unregistered (see above).
const UNTWINNED_PLAIN_FP_ROWS: [&str; 4] = [
    "getFloatInternal",
    "putFloatInternal",
    "getDoubleInternal",
    "putDoubleInternal",
];

/// Register every twin but [`UNTWINNED_PLAIN_FP_ROWS`] as a reviewed
/// `Intrinsic` (see the module docs). The kind is stated at each registration,
/// never inherited.
pub(crate) fn register_windowed_scoped_twins(registry: &mut NativeMethodRegistry) {
    for &(method, descriptor, callback) in WINDOWED_TWINS {
        if UNTWINNED_PLAIN_FP_ROWS.contains(&method) {
            continue;
        }
        registry.register_with_kind(
            SCOPED_MEMORY_ACCESS,
            method,
            descriptor,
            callback,
            NativeKind::Intrinsic,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row is a session-checked `*Internal` instance body, none of the
    /// rows `lib.rs` registers itself is here, and no name repeats.
    #[test]
    fn twins_are_session_checked_internal_bodies_not_registered_elsewhere() {
        assert_eq!(WINDOWED_TWINS.len(), 221);
        let mut seen = std::collections::HashSet::new();
        for &(method, descriptor, _) in WINDOWED_TWINS {
            assert!(method.ends_with("Internal"), "{method}");
            assert!(
                descriptor.starts_with("(Ljdk/internal/foreign/MemorySessionImpl;"),
                "{method}{descriptor}"
            );
            assert!(seen.insert(method), "duplicate {method}");
            for owned in [
                "copyMemoryInternal",
                "copySwapMemoryInternal",
                "setMemoryInternal",
                "vectorizedMismatchInternal",
                "getByteInternal",
                "putIntUnalignedInternal",
                "getLongInternal",
            ] {
                assert_ne!(method, owned, "registered by lib.rs");
            }
        }
    }

    /// Exactly the four plain float / double rows are left unregistered; every
    /// other row is a windowed twin.
    #[test]
    fn only_the_plain_float_double_rows_stay_untwinned() {
        let mut skipped: Vec<&str> = WINDOWED_TWINS
            .iter()
            .map(|&(m, _, _)| m)
            .filter(|m| UNTWINNED_PLAIN_FP_ROWS.contains(m))
            .collect();
        skipped.sort_unstable();
        assert_eq!(
            skipped,
            [
                "getDoubleInternal",
                "getFloatInternal",
                "putDoubleInternal",
                "putFloatInternal"
            ]
        );
    }
}
