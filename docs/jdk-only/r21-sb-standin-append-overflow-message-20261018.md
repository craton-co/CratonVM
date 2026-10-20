# The native StringBuilder stand-ins answer a different OutOfMemoryError when an append passes `int`

> **Moved (JIT round 21 orchestrator, 2026-10-19)** from `docs/known-issues/jit/`: the fix is a native classification decision (a `NativeKind` / `HotSpotAlignment` row), which belongs to a JDK-only round, not a JIT change. No Java-visible answer is known to differ on a real JDK boot.


Status: OPEN (not reachable on a real JDK in either mode; recorded, not changed)
Area: `native-builtins/src/lang_string.rs` (`sb_append_units`, `sb_reserve_units`, `sb_store_units`, `sb_oom`)
Found by: JIT round 21 wave 5, lane concat (review of the `--compatible` StringBuilder stand-ins, by reading)

## What

JDK 25's `AbstractStringBuilder.append(String)` grows through `ensureCapacityInternal(count + len)` ->
`newCapacity` -> `ArraysSupport.newLength`, whose `hugeLength` throws
`OutOfMemoryError("Required array length " + oldLength + " + " + minGrowth + " is too large")` when the length
leaves `int` (and `newCapacity` throws `"Required length exceeds implementation limit"` at exactly
`Integer.MAX_VALUE`). The native stand-ins compute the new capacity with saturating `usize` arithmetic
(`sb_grow_units`), build the whole content in a Rust `Vec<u16>`, and fail in the allocator: the native funnel's heap
`OutOfMemoryError` (or `"Requested array size exceeds VM limit"` from `sb_oom` on the constructor arm). They also
have no error channel (`sb_append_units` returns `ObjectRef`), and `sb_set_count(units.len() as i32)` would wrap past
`i32::MAX`.

## Why it is not a `--compatible` defect

On a real JDK neither mode runs these bodies: the registry rows are `Bridge` / `SyntheticStub`, and the interpreter
intrinsic table resolves its builder entries with kind `Bridge` (`vm/src/vm/vm_exec.rs`
`table_intrinsic_admitted` -> `resolve_dispatch`), so the concrete `AbstractStringBuilder` bytecode wins in both
modes and the overflow answer is the JDK's own. The compiled growth helper `jit_sb_grow_direct`
(`vm/src/jit/helpers.rs`) serves only sizes below `SOFT_MAX_ARRAY_LENGTH` and declines past it to that JDK body.
`R21ConcatRows` `fact sb-overflow` (run with `-Xmx4g`) pins the HotSpot text in both modes.

Reachable only on a boot without the JDK's class bytes (the synthetic String / builder classes), which has no HotSpot
counterpart.

## Reproducer

A no-JDK boot (synthetic runtime classes) running
`StringBuilder sb = new StringBuilder("a".repeat(1 << 30)); sb.append(sb.toString());` with a heap large enough for
both payloads: HotSpot (on its JDK) prints `Required array length 1073741840 + 1073741808 is too large`; the stand-in
raises a heap `OutOfMemoryError`.

## What would fix it

Give the native append arms an error channel (`MethodCallResult` through `sb_append_units` / `sb_store_units`) and
reproduce `ensureCapacityInternal` / `newCapacity` / `ArraysSupport.newLength` in 32-bit wrapping arithmetic before
building the content, raising the JDK's two messages. Worth doing only if a no-JDK configuration is meant to match a
JDK's answers.
