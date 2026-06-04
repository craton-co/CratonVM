# BouncyCastle math-ec — EC arithmetic timeout (>360 s)

## Symptom
BC core suite `org.bouncycastle.math.ec.test.AllTests` (via junit 3 TestRunner),
heap 1g:
- CratonVM-CPU (before EC fix): **rc=124 TIMEOUT 360.3 s**.
- HotSpot: OK **14 tests** 56.2 s.
- TornadoVM: OK 14 tests 57.4 s.

(Note even HotSpot takes ~56 s here — this AllTests is genuinely heavy EC point
arithmetic; the bar is "finish under the 360 s wall", which CratonVM missed.)

Reproduce:
```
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --Xmx 1g \
  -cp "apps/_test-suites/bc-java/core/build/classes/java/main;...test;...resources/main;...resources/test;%TEMP%/junit-3.8.2.jar" \
  junit.textui.TestRunner org.bouncycastle.math.ec.test.AllTests
```

## Root cause (pre-fix)
EC point multiply / field arithmetic ran far too slowly. Background:
- The JIT MISCOMPILES BouncyCastle EC, so `org.bouncycastle.*` is JIT-banned →
  EC runs interpreted (see `reference_ec_nojit_perf`, `reference_bc_suite_harness_artifacts`).
- SunEC native field ops were already sped up (~1–16 s → 135 ms per op) but the
  BC EC path is its own Java implementation, not SunEC.

## Fix under test
The EC JIT fix being measured: `cdab8f7` fix/ec-native-mont-mult (native
Montgomery multiply), `bcfbb5c` coarse native EC scalar-multiply, `b19250f`
ec-native-fallthrough. These target the EC multiply hot path.

## After-fix result — STILL FAILING; root cause now nailed (2026-06-04)

Investigated on branch `fix/bc-math-ec-timeout` (worktree `C:/craton/CratonVM-bcmath`).

**The SunEC EC fix does NOT touch the BC path — confirmed.**
`native-builtins/src/sunec_point.rs` only registers
`sun/security/ec/ECOperations.multiply` (and is gated default-OFF behind
`CRATONVM_NATIVE_EC_MULTIPLY`). BC `math.ec` is an entirely separate **pure-Java**
implementation:
- generic `Fp` curves → `ECFieldElement.Fp` over `java.math.BigInteger`,
- "custom" `Fp` curves (SecP*) → `org.bouncycastle.math.raw.Nat*` int[] arithmetic,
- `F2m` (binary) curves → `ECFieldElement.F2m` / `LongArray` / `SecT*Field` long[]
  carryless arithmetic.

None of these call SunEC. The native mont-mult / scalar-multiply intrinsics are
never hit on the BC path.

**Why it's slow: BC `math.ec` is JIT-banned and runs interpreted.**
`vm/src/jit/skip_list.rs` blanket-bans `org/bouncycastle/` from the JIT because of
a real codegen miscompile (see the long comment there + `docs/bc-math-ec-jit-miscompile-investigation.md`).
So all EC arithmetic runs in the interpreter.

**Per-scalar-multiply profile (warm), CratonVM-interp vs HotSpot** (`ECBench.java`
in the worktree, 5–12 iters, 1g heap):

| curve | family | HotSpot | CratonVM interp | slowdown |
|---|---|---|---|---|
| prime256v1   | Fp generic (BigInteger) | 2.2 ms | 136 ms | 61× |
| secp256r1    | Fp custom (Nat int[])   | 0.8 ms | 82 ms  | 100× |
| sect233r1    | F2m generic (LongArray) | 1.4 ms | 295 ms | **215×** |
| sect233r1    | F2m custom (SecT233)    | 0.9 ms | 122 ms | 137× |
| sect571r1    | F2m generic (LongArray) | 7.5 ms | **1500–1700 ms** | **~225×** |

The **F2m-generic `LongArray`** path dominates the wall (sect571r1 ≈ 1.5 s **per
scalar multiply**, interpreted). `testSumOfMultipliesComplete` /
`testAddSubtractMultiplyTwiceEncoding` run full scalar multiplies over **every**
named curve × **every** coordinate system, so the suite is hopelessly over 360 s
interpreted. (A VM-internal stack-dump watchdog also aborts at 120 s by default;
disable with `--stack-dump-on-timeout=0` for real measurements.)

**Secondary finding — a hard, silent VM crash (`rc=1`) on interpreted BC EC.**
The suite does not only time out: it nondeterministically exits `rc=1` with **no
output on either stream** (beyond the BigInteger post-clinit WARN). Classification
(see `bc-math-ec-probes/RunEC.java`, a driver that runs `AllTests.suite()`, wraps
everything in `try (Throwable)`, and prints results/exceptions to **stderr**):

- Short runs (`timeout 90`) survive — `rc=124`, just slow. The crash only appears
  after ~90–500 s of execution (more random curves/inputs exercised).
- A long run (`timeout 500`) exits `rc=1` with `RunEC` printing **neither** its
  `DONE …` summary **nor** a `THROWN …` line, and `System.out` is 0 bytes.

It is **not** a Java exception (RunEC's `catch (Throwable)` would print it),
**not** a `TestRunner` `System.exit(1)` on assertion failure (RunEC never calls
`System.exit`), **not** a Windows hardware fault (the auto-installed VEH crash
handler in `vm/src/runtime/crash_handler.rs` prints nothing), and **not** a Rust
panic (no panic message). It is a VM-level hard exit *below* the Java exception
machinery.

This `rc=1` + completely-empty-stdout+stderr signature *looks* like CratonVM's
known non-throwing heap-exhaustion symptom (documented for the bintrees GC
benchmark: too-small heap or stray-`cratonvm.exe` contention → `rc=1` empty, NOT a
catchable `OutOfMemoryError`) — but **heap exhaustion is ruled out**:

- Stray-process contention: ruled out (verified **zero** stray `cratonvm.exe`,
  killed first; machine has 64 GB).
- Heap size: `--Xmx 8 g` ran ~600 s before exiting `rc=1`, but `--Xmx 12 g`
  crashed in **90 s** — *more* heap crashed *sooner*. That is the opposite of an
  OOM (more heap delays exhaustion), so it is not heap.

This is a **genuine nondeterministic interpreter/native crash on some EC input**,
distinct from the throughput problem, that would block a green suite even with
unlimited time and a huge heap.

### Localized (deterministic driver `bc-math-ec-probes/ECCrash.java` / `ECMin.java`)
A driver that walks every named+custom curve × coordinate system with a
**fixed-seed `java.util.Random`** (printing+flushing the curve before each op)
pins the faulting site:

- **Curve/coord:** **custom binary-field (`SecT*`) curves at coordinate system 6
  (`LAMBDA_PROJECTIVE`)** — confirmed on `sect193r2`, `sect233r1` (crashes by
  i≈200), and `sect163r1` (`org.bouncycastle.math.ec.custom.sec.SecT*Curve`), so it
  is **general to the custom-`SecT` lambda-projective path**, not one curve. The
  **generic** `LongArray` `named/sect193r2 coord=6` (same curve, non-custom)
  succeeds, so it is specific to the **custom `SecT*Field`** implementation, not
  binary-field math in general.
- **Operation:** *without* GC stress, op-mode runs suggested `mul` alone survived
  1251 iters while `mul`-then-`twice` crashed — but that is an **allocation-rate
  artifact, not a real op distinction**: under `CRATONVM_DBG_GC_STRESS` even `mul`
  alone crashes at i=0 (it just allocates less, so natural GC fired later). The
  corruption is **general to the interpreted EC scalar-multiply path** under GC
  pressure; the higher-allocation ops (`multwice`) merely trip it sooner.
- **Mechanism:** hard VM exit `rc=1`. A `try { … } catch (Throwable)` around the op
  **never fires** (so not a Java exception/Error, incl. not a catchable
  `StackOverflowError`/`OutOfMemoryError`); the VEH handler prints nothing (not a
  Windows hardware fault); no Rust panic/`overflowed its stack` message.
- **Nondeterministic despite fixed input seed:** crashes at i≈371 / i≈473 across
  runs, and one run (`RUST_BACKTRACE=full`) survived to i≈1550 — input-fixed but
  layout/timing-dependent, the signature of **GC-driven memory corruption** (below).

### ROOT CAUSE: GC stale-reference corruption (a write-barrier / root-remap gap)
Running the repro under **`CRATONVM_DBG_GC_STRESS=1048576`** (force a young GC every
1 MiB) makes it **deterministic and immediate** — `ECMin sect233r1 6 multwice`
crashes at **i=0–13 every run**. With a flushing subscriber (`CRATONVM_DBG_EXIT=1`)
the real signature is exposed — a flood of:

```
WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped
  obj=0x1a9b06f8 index=0 num_slots=0 class_id=ClassId(0)
  class_name=java/lang/Object real_field_count=Some(0) value=Object(None)
```

i.e. references that **should** point to a `SecT*FieldElement`/`long[]` intermediate
now resolve to a **0-slot `java/lang/Object`** (the same recurring addresses, e.g.
`0x1a9b06f8`/`0xc5ff06f8`). That is a **stale reference**: a young GC moved/collected
the EC intermediate, but a root/field holding it was not remapped (or the object was
collected because a holding root was not scanned). The interpreter's `putfield` then
writes field 0 of what is now a bare `Object` → `gen_heap::set_field` drops the write
(index 0 ≥ num_slots 0) → the field element silently keeps its old/garbage value →
BC computes a wrong point and its own check throws **`IllegalStateException: Invalid
result`** (caught by the driver at i=13). When the stale slot instead lands somewhere
the dropped write isn't a clean miss, the same corruption is the **silent `rc=1`**.

So the secondary bug is a **GC bug**, not an EC bug: the heavily-allocating SecT
lambda-doubling holds references across many short-lived allocations, and under GC
pressure one of those references is not kept-alive/remapped across a young
collection. This is the **same family** as previously-fixed CratonVM corruptions —
a process-global cache that wasn't a GC root/remap target (the classloader-GC-root-
gap fix in `roots.rs`+`gc.rs`), and register-invisible roots under moving GC — i.e.
a missing root or a missing write-barrier on an old→young store.

**Next step for whoever takes it (deterministic now):** repro is
`CRATONVM_DBG_GC_STRESS=1048576 CRATONVM_DBG_EXIT=1 cratonvm … ECMin sect233r1 6
multwice` → crashes at i≤13 with the `set_field … num_slots=0` warnings. Add a
backtrace at the `gen_heap::set_field` drop site (or at the young-GC evacuation that
fails to scan/remap the holder) to identify which root/field is stale, then fix that
root scan / write-barrier — mirroring the `roots.rs` + `gc.rs` cache remap fix used
for the classloader-GC-root-gap. Its own task; see the spawned chip.

**The JIT ban genuinely cannot be lifted — the miscompile is IN the leaf F2m
arithmetic.** Running `ECBench` with `CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/`
reproduces the miscompile deterministically as an
`ArrayIndexOutOfBoundsException` thrown out of
`org/bouncycastle/math/raw/Interleave.expand64To128` →
`SecT233Field.implSquare`/`squareN` on the first F2m curve. So a "selective
leaf-class JIT allow-list" would crash the same way; the ban must stay until the
JIT value-production miscompile is fixed. (This is a clean minimal repro for
whoever takes on the JIT bug.)

## Partial improvement landed: word-based BigInteger arithmetic
`native-builtins/src/lib.rs` `native_bi_{multiply,add,subtract}` were rewired from
the O(n²) decimal round-trip (`bi_*_str` via `bi_read`/`bi_alloc`) to the
word-limb `bigint::BigInt` path (`bi_read_int`/`bi_alloc_int`) — the read/write
boundary and `BigInt` ops were already present (see
`docs/biginteger-limb-rewrite-scope.md`) but only `bi_read_int` was wired.
Verified: `cargo test -p cratonvm-native-builtins bigint::` 8/8 pass (the
differential tests vs the decimal reference), and EC multiply is **byte-identical
to HotSpot** on prime256v1 / secp384r1 / brainpoolP256r1 + an add/sub/mul combo
(`ECVerify.java`). This speeds up the **Fp-generic (BigInteger)** curves and the
broader BigInteger-bound suites (RSA/DSA keygen, `PrimesTest`, bc-crypto) but does
**not** touch the F2m path that dominates bc-math-ec, so it does **not** by itself
bring this suite under 360 s.

## What an agent should try next
The only two ways to make bc-math-ec pass under 360 s:
1. **Fix the JIT value-production miscompile and lift the `org/bouncycastle/` ban.**
   This is the real fix (interpreted is 60–225× and unreachable). Minimal repro:
   `ECBench` (worktree) + `CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/` →
   AIOOBE in `Interleave.expand64To128`. See `docs/bc-math-ec-jit-miscompile-investigation.md`.
2. **Native intrinsics for the hot interpreted field ops**, SunEC-style but for BC:
   - `org/bouncycastle/math/ec/LongArray` carryless multiply + reduction (biggest
     win — F2m generic, the dominant cost; this is a PCLMULQDQ-shaped kernel),
   - `org/bouncycastle/math/raw/Nat*` mul/square (Fp custom),
   - finish wiring the remaining BigInteger ops to `bigint::BigInt` (Fp generic).
   This is a large multi-family surface (`LongArray` alone is ~2150 LOC) and 360 s
   is not guaranteed even then, so option 1 is preferred if the JIT bug is tractable.
