# Feature Roadmap — JIT-Inlined Intrinsics (beyond `java.lang.Math`)

Status: **Proposed** · Owner: _unassigned_ · Target: post-0.2.0

## 1. Goal

Extend the JIT's call-site intrinsic mechanism — today implemented only for
`java/lang/Math` — to cover the remaining hot, leaf, HotSpot-class intrinsics so
that the JIT emits inline machine code instead of a native-registry call:

- `System.arraycopy` → inlined bulk copy (`REP MOVSB/MOVSQ`, or SIMD for known
  element widths) with a single fused bounds/null check.
- `java.lang.String` / `java.lang.StringLatin1` — `length`, `charAt`,
  `hashCode`, `compareTo`, `indexOf`, `equals`.
- `java.util.Arrays` — `fill`, `equals`, `sort` (leaf scalar paths).
- `java.lang.Integer` / `java.lang.Long` bit ops — `bitCount`, `numberOf*Zeros`,
  `reverse`, `reverseBytes`, `highestOneBit`, `lowestOneBit`, `compare`.
- `java.util.zip.CRC32` / `CRC32C` — `update` (hardware `CRC32` instruction).

## 2. Current state (baseline)

- The intrinsic dispatch already exists and works — but **only for `Math`**.
  - Sentinels: `jit/src/lib.rs:1246-1274` (`MATH_SQRT_INTRINSIC`, …,
    `MATH_MAX_LONG_INTRINSIC`). They are `usize::MAX - N` values that can never
    be valid code pointers.
  - Matcher: `try_resolve_intrinsic` in `jit/src/lib.rs:2807` keys on
    `(class_name, method_name, descriptor)`.
  - Codegen: `jit/src/x64.rs` `callee_entry` arms (`x64.rs:11688+`) emit
    `SQRTSD` / `ROUNDSD` / `CMOV` / FMA inline — no `CALL`, no safepoint.
- `System.arraycopy` is currently a **native-registry call** — see the test
  comment at `jit/src/lib.rs:4288` ("routed through the native registry as an
  interpreter-level intrinsic"). The JIT accepts the invoke site but does not
  inline it.
- No sentinels or matcher arms exist for `String`, `StringLatin1`, `Arrays`,
  `Integer`/`Long`, or `CRC32`.
- Stale comment to clean up: `jit/src/x64.rs:4529` claims `Math.min/max` are
  "NOT yet registered as JIT intrinsics" — they now are (sentinels 1271-1274,
  matcher 2824-2827). Delete the stale comment as part of Phase 0.

## 3. Design

### 3.1 Sentinel namespace

The `usize::MAX - N` sentinel scheme does not scale cleanly past ~30 entries and
is easy to mis-number. Introduce an explicit enum instead:

```rust
#[repr(usize)]
pub enum JitIntrinsic { MathSqrt, MathFloor, /* … */ ArraycopyByte, StringLength, /* … */ }
```

Map the enum to the existing `JitDirectCall.entry` sentinel space via a helper
(`JitIntrinsic::as_entry()` → `usize::MAX - (variant as usize)`), so the
`callee_entry` dispatch in `x64.rs` stays a single comparison. Migrate the 14
existing `MATH_*_INTRINSIC` consts onto the enum (keep the old `pub const`
names as deprecated aliases for one release).

### 3.2 Matcher

Replace the per-class `match` ladder in `try_resolve_intrinsic` with a single
`static` perfect-hash / `phf` map keyed on `(class, name, descriptor)` → 
`(JitIntrinsic, arg_slots, return_kind)`. Gate width-dependent intrinsics on CPU
features (`x64::has_sse41()`, a new `has_sse42()` for `CRC32`, `has_popcnt()`).

### 3.3 Codegen per family

| Family | Inline strategy |
|---|---|
| `System.arraycopy` | Null-check src/dst, fused bounds check, then `REP MOVSB` (or `MOVSQ` for 8-byte elements). For reference arrays keep the GC store-barrier — fall back to the native call if a card-marking barrier is required. |
| `String.length` / `charAt` | Direct field load of the `value` array length / element (respect compact-strings: `coder` field selects Latin-1 vs UTF-16). |
| `String.hashCode` / `equals` / `compareTo` / `indexOf` | Tight Rust-equivalent loops emitted inline; SIMD `PCMPEQB` for `equals`/`indexOf` where profitable. |
| `Arrays.fill` / `equals` | `REP STOS` / SIMD compare. |
| `Integer`/`Long` bit ops | Single `POPCNT` / `LZCNT` / `TZCNT` / `BSWAP` / `BSR` / `BSF` instruction. |
| `CRC32.update` | Hardware `CRC32` instruction over the buffer. |

### 3.4 Correctness guards

- `arraycopy` on reference arrays must preserve store-type checks
  (`ArrayStoreException`) and the GC write barrier — when either is needed and
  cannot be inlined cheaply, **bail to the native call** (return `None` from the
  matcher). Never trade correctness for inlining.
- All intrinsics must produce results bit-identical to the existing
  `native-builtins` implementations — those become the differential oracle.

## 4. Phases

- **Phase 0 — Refactor (no behavior change).** Introduce `JitIntrinsic` enum +
  `phf` matcher, migrate the 14 `Math` intrinsics, delete the stale
  `x64.rs:4529` comment. Acceptance: existing `test_compile_math_*` tests pass
  unchanged.
- **Phase 1 — Bit ops.** `Integer`/`Long` — lowest risk (pure leaf, single
  instruction, no memory). Establishes the differential-test harness.
- **Phase 2 — `System.arraycopy`.** Highest payoff, highest risk (GC barrier,
  exceptions). Primitive arrays first; reference arrays bail to native unless
  the barrier-free fast path applies.
- **Phase 3 — `String` / `StringLatin1`.** Requires modelling the compact-string
  `coder` field; `length`/`charAt`/`hashCode` first, then `equals`/`compareTo`/
  `indexOf`.
- **Phase 4 — `Arrays.fill`/`equals`, `CRC32`.** SIMD + hardware `CRC32`.

## 5. Files to touch

- `jit/src/lib.rs` — `JitIntrinsic` enum, sentinel mapping, `try_resolve_intrinsic` matcher.
- `jit/src/x64.rs` — new `callee_entry` codegen arms; CPU-feature detection (`has_sse42`, `has_popcnt`).
- `jit-api/src/lib.rs` — only if a new `JitRuntimeHelpers` slot is needed (e.g. a slow-path arraycopy helper); follow the `math_fma_*` precedent.
- `jit/tests/` — new differential tests (see §6).

## 6. Testing

- **Differential tests:** for each intrinsic, JIT-compile a method and assert the
  result equals the `native-builtins` implementation over a randomized input
  matrix (edge values, NaN, overlap for `arraycopy`, empty/huge arrays).
- **Negative tests:** reference-array `arraycopy` with a store-type violation
  still throws `ArrayStoreException`; out-of-bounds still throws
  `ArrayIndexOutOfBoundsException`.
- **CPU-feature fallback:** force-disable `popcnt`/`sse42` and confirm the
  matcher falls back to the native call.

## 7. Risks

- `arraycopy` reference-array correctness (GC barrier / `ArrayStoreException`) —
  mitigated by bail-to-native.
- Compact-string `coder` assumptions could break if the JDK String layout the
  VM loads differs — pin to the layout `native-builtins/src/lang_string.rs` uses.
- Sentinel-space exhaustion — mitigated by the `JitIntrinsic` enum in Phase 0.

## 8. Acceptance criteria

- All targeted methods inline with no `CALL` in the generated code (verified by
  disassembly in tests).
- Differential tests pass; no JCK regressions.
- Measurable speedup on an intrinsic-heavy benchmark (target: ≥1.5× on a
  `arraycopy`/`String`-bound microbenchmark vs the native-call path).
