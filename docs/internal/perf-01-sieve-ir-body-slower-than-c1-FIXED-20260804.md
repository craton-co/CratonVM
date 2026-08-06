# PERF-01 — the optimizing tier's `sieve` body was 6.4x slower than the C1 body it replaced — FIXED 2026-08-04

**Landed 2026-08-03 with `cov-02`. Found 2026-08-04. Fixed the same day.**
The brief this closes is
[`../known-issues/c2/perf-01-sieve-ir-body-6x-slower-than-c1.md`](../feature-designs/c2/perf-01-sieve-ir-body-6x-slower-than-c1.md).

## What happened

`cov-02` taught `IrBuilder::build` to lower integral array access, which is
right and which its own closeout measured: refusals for its seven opcodes
79 → 0, optimizing-backend bodies 591 → 652 on the Spring workloads.

One of the methods it newly admitted was `CratonBench.sieve([ZI)I`. Before, the
builder refused it at `0x54 bastore` and it fell through to the **single-pass**
backend — which has three bulk-byte loop lowerings that replace a scalar
`boolean[]` element loop with a vectorised pre-header. After, the optimizing
tier produced a body for it, and that body is scalar.

| build | sieve, 5 interleaved pairs | reach |
|---|---|---|
| before (`cov-02` base) | 2,462 / 2,487 / 2,324 / 2,461 / 2,474 ms | 2 req, 1 admitted, **0 bodies** |
| after (`cov-02` arrays) | 15,949 / 15,805 / 15,662 / 15,922 / 15,823 ms | 2 req, 1 admitted, **1 body** |

**6.4x.** Checksums identical (`9592`) on both and matching HotSpot, so this
was throughput and never correctness. HotSpot JDK 25 C2 runs the phase in
2,412 ms — CratonVM was *faster than HotSpot* before the change and 6.5x
slower after it.

`sieve` was the only CratonBench phase whose C2 reach changed and the only one
whose time changed. That is the whole finding, and the per-phase reach record
added by `MEAS-02` the day before is what made it one step instead of a bisect.

## The fix

The optimizing tier's admission chain now declines a method whose loops the
single-pass backend would **vectorise**.

It is deliberately not a heuristic and not a bytecode pattern of its own. It
asks that backend's own detectors, through the function the emission path
itself calls:

* `escape_analysis::detect_bulk_byte_loops` — the one place the three
  detectors run. `x64/driver.rs` had three inlined copies of this loop; they
  are gone, and the driver destructures this instead.
* `escape_analysis::single_pass_has_bulk_byte_lowering` — what the admission
  chain asks. Same detectors, same loops, same `bypassable_headers` filter,
  same flag.

Two copies of one predicate in two files is how the frame reservation and the
stub spill drifted 32 registers apart, so there is one copy.

**Cost.** A raw-byte fast reject on `0x54` runs first, since all three
detectors require a `bastore`. Scanning bytes rather than decoding can only
produce a false *positive* — which costs the full detection that then says no
— and never a miss.

**Declared and bisectable.** `CRATONVM_JIT='-c1-vector-veto'` hands those
methods back to the IR tier, which reproduces the regression and lets a probe
that wants IR codegen for a sieve-shaped method get it (the term sits *after*
`CRATONVM_JIT_FORCE_C2` in the chain). Default **on**, because this repairs a
shipped regression rather than adding a behaviour.

The refusal names itself in the admission line, so the reach record explains it
without anyone reading this document:

```
[ir] admission CratonBench.sieve([ZI)I: the single-pass backend vectorises a
     byte-array loop in this method and the IR tier would emit a scalar one
```

## Verification

| check | result |
|---|---|
| `sieve` reach | 2 req / 1 admitted / 1 body → 2 req / **0 admitted** / 0 bodies |
| `sieve` speed, interleaved vs the pre-`cov-02` control and HotSpot 25 | fixed ≈ control ≈ HotSpot; the regressed binary stays 5–6x slower |
| blast radius (below) | **exactly one method** |
| `cargo test -p cratonvm-jit --lib` | 1880 passed, 0 failed |
| HotSpot-diffed regression suite | 22 passed, 0 failed |
| the new unit test | `single_pass_bulk_byte_veto_fires_on_the_cratonbench_sieve` |

### Blast radius, measured the only way worth measuring

The declared off-switch makes this exact rather than argued: **one binary, the
flag on and off**, all ten phases. Anything else — comparing against an older
binary, or against another tree — confounds the veto with everything else that
landed.

| phase | veto ON (default) | veto OFF |
|---|---|---|
| `cb:arithmetic` | 0 / 0 / 0 | 0 / 0 / 0 |
| `cb:fib` | 1 / 1 / 1 | 1 / 1 / 1 |
| **`cb:sieve`** | **2 / 0 / 0** | **2 / 1 / 1** |
| `cb:matrix` | 1 / 0 / 0 | 1 / 0 / 0 |
| `cb:hashmap` | 0 / 0 / 0 | 0 / 0 / 0 |
| `cb:stringregex` | 1 / 0 / 0 | 1 / 0 / 0 |
| `cb:bintrees` | 3 / 1 / 1 | 3 / 1 / 1 |
| `c2c:dispatch` | 16 / 9 / 7 | 16 / 9 / 7 |
| `c2c:bind` | 9 / 5 / 5 | 9 / 5 / 5 |
| `c2c:pipeline` | 14 / 7 / 5 | 14 / 7 / 5 |

One row differs. The veto costs exactly one IR body across both benchmark
suites — the one that was 6.4x slower than the code it displaced — and the
framework-shaped candidate, which is the workload that actually exercises the
optimizing tier, is untouched to the cell.

The unit test asserts the veto fires on the exact javac bytecode of
`CratonBench.sieve`, that it agrees with what the emission path found, and that
a loopless method is *not* vetoed. Without the middle assertion the test would
still pass if the veto started refusing everything.

## Known limitation, left alone on purpose

The admission chain sees the method's **original** bytecode; the driver
re-binds `code` to the rewritten buffer when the bytecode loop transform
applies. Under `CRATONVM_JIT='bytecode-loop-xform'` the two can disagree — a
rewrite that destroys the vectorisable shape leaves a method vetoed here and
un-vectorised there; one that creates it leaves the IR tier holding a method
the other backend would have done better.

The transform is opt-in and off by default, so no shipped configuration is
affected, and both failure modes are worse codegen rather than wrong codegen.
Re-derive it if that transform ever becomes default-on.

## The general form, taken on the same day

Fixing one method was a special case. The problem behind it is:

> **The optimizing tier installs its body whenever it *can*, and nothing checks
> that the body it installs is faster than the one the single-pass backend
> would have installed.**

The set of things that backend can do and the IR tier cannot is finite and
knowable. It is now enumerated, in `jit/src/x64/single_pass_only.rs`, and the
admission chain consults the enumeration rather than the one-off predicate this
fix started as. Seven classes, each consumed by a single-pass emitter at a loop
header, each with no counterpart in `ir_optimize`/`ir_lower`:

| class | what the single-pass backend emits |
|---|---|
| `BulkZeroByteFill` | `REP STOSB` over a `byte[]`/`boolean[]` clear |
| `BulkSetByteStride` | a bulk `a[iv] = 1; iv += step` store |
| `ByteSieve` | the whole sieve nest in registers |
| `SimdIntArraySum` | AVX2 `int[]` reduction |
| `SimdFpArraySum` | AVX2 floating-point reduction |
| `SimdArrayElementWise` | AVX2 `out[i] = a[i] OP b[i]` |
| `MatrixDot` | the matrix dot-product nest |

The verdict now names which one it protected, so a reader of a results
directory sees the reason and not just the refusal.

### Three things the audit turned up

**1. The IR tier has no vectoriser at all.** Every SIMD family above is a
lowering it cannot match. `cov-02` hitting one of them was not bad luck — there
were four more of exactly that shape waiting for the next lane.

**2. Loop unswitching was in the first draft of the list and is not in it
now.** It is detected and an emitter consumes it, which is what put it there.
Reading what that emitter *emits* is what took it out:
`emit_loop_unswitch_preheader`'s own contract says the sequence "is
*additive* — it reads `invariant_local` and sets flags but never writes back to
any local … Removing the emission yields identical final state." It does not
duplicate the body or hoist the branch, so there is no advantage to protect.
Vetoing on it would have declined IR bodies for every loop with an invariant
branch — a common shape — in exchange for nothing. **Do not add a class because
a detector and an emitter exist. Read what the emitter emits.**

**3. `ir_optimize`'s unroll and LICM are default-ON**, and the comments beside
them saying "Default-OFF … while it soaks" were stale; they are corrected in
the same commit. Those two are precisely why the single-pass native unroller
and the `aaload`/FP hoists are *not* on the veto list, so a reader who believed
the comments would have added two more classes that cost a large population of
IR bodies for nothing.

### Blast radius of the widened veto

Same binary, flag on and off, all ten phases — the seven-class registry still
costs **exactly one** IR body, the same one:

| phase | veto ON | veto OFF |
|---|---|---|
| **`cb:sieve`** | **2 / 0 / 0** | **2 / 1 / 1** |
| *all nine others* | *identical* | *identical* |

`c2c:dispatch` first appeared to differ (7 bodies against 8). It does not: the
veto's verdict string never appears in that phase's log at all, and four
repeats put ON above OFF twice, below once, equal once — that phase's request
count swings 15–18 run to run on its own. **A one-body difference on a phase
that noisy is not a result**, which is why the check that settled it was the
verdict string and not the count.

## What this still does NOT fix

The enumeration catches an advantage somebody has written down. It cannot catch
one nobody has.

Two things narrow that and neither closes it. Every `match` in the module is
exhaustive, so a new class cannot be half-added — it will not compile until it
is classified, and `all_variants_are_registered` fails until `ALL` carries it.
And the audit that produced the seven is reproducible: read every list the
single-pass emitters consult at a loop header, ask whether `ir_optimize` or
`ir_lower` has a counterpart, then read what the emitter actually emits. But
nothing forces the next person to run it.

**The mechanism that would close it is a backend-parity harness**: compile a
corpus with both backends and flag any method whose single-pass body contains
VEX-prefixed or `REP`-string bytes that its IR body does not. That is
capability-*agnostic* — it would catch a vectorising specialisation nobody
registered, because it reads emitted bytes rather than asking a detector.

It is not built here, and the reason is structural rather than effort: the two
backends cannot currently be driven independently over the same method. The
single-pass call site sits about 2,000 lines below the IR one inside
`try_compile_inner`, behind local state built in between, so "compile it both
ways" means either restructuring that function or duplicating a 19-argument
call. That is a real increment, and it is written up as one because a parity
check that only *looks* general is worse than one whose limits are on the
label.

Until it exists, the `ir_reach_<phase>` record in every gate run is the
tripwire, and the gate now runs (`MEAS-02` fixed the `javac` defect that had
stopped it): `sieve`'s baseline is 2,700 ms with a 5% budget and the regressed
build measured 15,680 ms, so the gate would have failed it outright on the day
it landed.
