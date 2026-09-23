# H9-1 — a 3-D array allocation bail-lists its whole method, for the life of the process

**Status: ✅ RESOLVED 2026-09-22.** The x64 single-pass backend compiles
`multianewarray` at any arity up to `MAX_JIT_MULTIANEWARRAY_DIMS`.
`RomulusTest` went from **91 508 ms to 2 316-3 481 ms** against HotSpot's
586-593 ms — **138x to 4-6x** — and `RomulusEngine.skinny_128_384_plus_enc`,
which the OSR door used to refuse by name, now OSR-compiles.

**Found by:** profiling what was left of `org.bouncycastle.crypto.test.AllTests`
after the BigInteger `@IntrinsicCandidate` set landed (see
`docs/internal/comparison-handoff/bug-bc-crypto-regression-timeout.md`).

## The rule that was there

`jit/src/x64/bytecode_compat.rs`, the `multianewarray` (`0xc5`) arm of
`jit_scan`:

```rust
let ndims = code[pc + 3];
if ndims != 2 {
    return None;
}
```

A `jit_scan` `None` is **permanent and whole-method**. `try_compile_inner` and
the OSR door (`vm/src/runtime/interpreter/jit_bridge.rs`) both call
`mark_jit_bail_listed` on it, so the method is refused at *every* door for the
rest of the process — not just the one that asked. One `new byte[3][4][4]`
anywhere in a method was therefore enough to make that whole method
permanently interpreted, however hot it became.

The single-pass emitter agreed with the scanner and refused too
(`jit/src/x64/op_object.rs`, `singlepass-codegen/multianewarray-not-2d`), and
the optimizing pipeline refuses *any* `multianewarray` at all
(`jit/src/ir.rs`). So there was no tier that could take such a method.

## What it cost, measured

`org.bouncycastle.crypto.engines.RomulusEngine.skinny_128_384_plus_enc([B[B)V`
opens with

```java
byte[][] state = new byte[4][4];
byte[][][] keyCells = new byte[3][4][4];      // <- ndims = 3
byte[][][] keyCells_tmp = new byte[3][4][4];  // <- ndims = 3
```

and is then a 40-round permutation over those cells. It was the hottest method
in the BouncyCastle lightweight-crypto battery: 62.6% of all execution samples
in `RomulusTest`, which ran 138x HotSpot's wall while the battery around it ran
at 4.3x.

## The cause was the helper ABI, not the allocator

The runtime half was already dimension-agnostic:
`interpreter::multianewarray_alloc(vm, thread, holder_cid, cp_index, &dims)`
takes a **slice** of dimensions, and the packed `(holder_class_id, cp_index)`
site descriptor carries no arity either.

The blocker was the **helper-call ABI**. `ARG_REGS` is four wide on Windows x64
(`jit/src/x64/reg_encoding.rs`: RCX, RDX, R8, R9), and
`jit_multianewarray_2d(vm_ptr, site, dim1, dim2)` spent all four on two
dimensions. There is no stack-argument emission on the helper-call path, so a
third dimension had nowhere to go.

## What was done

**The dimensions go through memory, so arity costs no registers.** Helper ABI
v14 appends `multianewarray_n(vm_ptr, site, ndims, dims_ptr)`: `dims_ptr`
addresses `ndims` consecutive `i64` dimension counts, outermost first, which
the emitter has pinned into its own frame scratch words. `jit_multianewarray_2d`
stays wired — the helper table is append-only — and delegates to the same body,
so there is still exactly one allocation path and it is still
`interpreter::multianewarray_alloc`.

Three gates moved together, which the previous version of this page warned
they must:

* `jit_scan`'s `ndims != 2` → `ndims == 0 || ndims > MAX_JIT_MULTIANEWARRAY_DIMS`.
* `op_object.rs`'s `multianewarray-not-2d` → the same range check, plus a
  `spill_range_fits` probe for the dimension buffer
  (`singlepass-codegen/multianewarray-dims-spill-exhausted`).
* `ir.rs`'s blanket refusal of the optimizing tier is **unchanged and
  deliberate**: it refused 2-D too, and a method containing one still compiles
  on the single-pass backend. The jitc log shows exactly that handover —
  `[ir] ir_compatible refused: !scan.multianewarray_ops.is_empty()` followed by
  `OSR-compile ... entry_pc=226 entry=0x267ceb60000 len=62419`.

`MAX_JIT_MULTIANEWARRAY_DIMS` is 8. It is not a JVM limit (JVMS §4.4.1 allows
255) but a budget: the emitter writes the counts into that many frame scratch
words out of a finite spill reserve, and admitting a shape the emitter will
then refuse costs a **permanent** whole-method bail, so the scanner declines
the same shapes the emitter would.

### The ordering the emitter has to get right

The spill comes **first**, before the dimension buffer — the opposite of every
other helper call site in that file. `emit_pre_safepoint_spill` flushes
register-resident operand-stack entries, and a flush RESERVES spill words from
the cursor; with the dimensions already popped the cursor has rewound over
their frame homes, so a flush can hand one of those homes to another stack
entry and overwrite a dimension before it is read. Spilling while the
dimensions are still **on** the model stack makes that unrepresentable, because
the cursor is then above every one of them.

## A second defect, found by the fixture and not by the lowering

Negative dimensions are checked **outermost first**, and all of them are
checked before anything is allocated. MEASURED on HotSpot 25.0.3+9:

```
new byte[-1][4][-2]   ->  NegativeArraySizeException: -1
new byte[2][0][-1]    ->  NegativeArraySizeException: -1
```

— the second one throws even though a zero outer length means no inner array
would ever be allocated, i.e. the check is a full pre-pass and not something
the allocation walk discovers on its way down.

This VM's **interpreter** checked them in POP order, i.e. innermost first, and
stopped at the first one it met going inward: it answered `-2` and "no throw"
for those two cases. Its JIT helper already answered outermost-first, so the
two tiers disagreed with each other as well as with HotSpot. Fixed in
`vm/src/runtime/interpreter/opcodes.rs` to match; the JIT's order is unchanged.

## Verification

`regression-suite/src/RJitMultiArrayDims.java`, a new CORE vector registered in
`run.sh`, built on `RJitMultiArrayClass`'s two-tier design: 28 shapes each read
cold (interpreted), at the iteration its answer changes if it ever does, and
hot (compiled). **84 checks, 0 fails, 0 tier splits**, on HotSpot 25.0.3+9, on
CratonVM, and on CratonVM `--nojit`. Every expected value was measured on
HotSpot rather than predicted.

Rust side: the `jit_scan` unit test now pins that 3-D is admitted with the site
recorded, that `ndims == 0` is still refused **and names `@pc=0 op=0xc5`**, and
that an over-budget arity is refused; `jit/src/tests.rs` pins that the 2-D
helper delegates and does not allocate for itself. 3384 jit lib tests and the
whole `jit-api` ABI ledger pass.

### The walls

| | HotSpot 25.0.3+9 | CratonVM `--jdk-only` | ratio |
|---|---|---|---|
| `RomulusTest`, before | 660 ms | 91 508 ms | 138x |
| `RomulusTest`, after | 586-593 ms (3 reps: 896 / 593 / 586) | **2 316-3 481 ms** (3 reps: 2 316 / 3 186 / 3 481) | **4-6x** |
| `RomulusTest`, `--nojit` control | — | 286 200 ms | — |

Both "after" rows were taken while this machine was also running a release
build, which is why the CratonVM reps rise across the run and why the range and
not a single number is quoted. The "before" row is the previous session's
measurement on this branch's own base; it is not a same-session A/B, and the
causal evidence is the jitc log below rather than the wall alone.

The `--nojit` row is the point of the fix rather than a curiosity: 286 s is
what this test costs with nothing compiled, 91.5 s is what it cost with
everything compiled **except** the one method the scanner refused, and 2-3 s is
what it costs now. The method was 62.6% of the samples, and the refusal was
worth roughly 89 seconds of a 91-second test.

Direct evidence that it is this method and not the neighbourhood, from
`CRATONVM_DBG=jitc` on the same command line that used to print
`osr-DENY (jit_scan refused: <unrecorded> @pc=0 op=0x00)`:

```
[cratonvm-jitc] bg-compile  RomulusEngine.skinny_128_384_plus_enc([B[B)V tier=C2 optimized=true osr_bci=226
[cratonvm-jitc] OSR-compile RomulusEngine.skinny_128_384_plus_enc([B[B)V entry_pc=226 entry=0x267ceb60000 len=62419
[cratonvm-jitc] OSR-reuse   RomulusEngine.skinny_128_384_plus_enc([B[B)V entry_pc=226 ...
```

## What this does NOT fix

`RomulusTest` at 4.0x is in line with the battery around it, not ahead of it,
and `CipherStreamTest` (79 s, 80x) is now the largest single item in that
suite. Nothing here touches the optimizing tier, which still refuses every
`multianewarray`: giving `Op::NewArray` a multi-dimensional form and a
resolver-fed info map is a separate, larger change, and the survey that
declined it (2 events against `anewarray`'s 138) has not been re-run since the
arity gate lifted.
