# `BOBYQAOptimizerTest` times out because compiled numeric code is ~80x HotSpot — and 70% of it is one `getfield`

**Status: OPEN, root cause identified and sized 2026-08-18.** Not fixed here;
the fix belongs to
[`../jit/every-jit-getfield-takes-the-helper-because-the-guarded-inline-check-always-fails-20260817.md`](../jit/every-jit-getfield-takes-the-helper-because-the-guarded-inline-check-always-fails-20260817.md),
which this page now supplies the missing price and engagement counter for.

Re-verified 2026-08-18 on `dev` `c6299ca2a`: still HANG at the 90 s per-class
cap while HotSpot passes the class in 2 s, and it is now one of only two
non-PASSing classes in the whole 310-class `commons-math-legacy` sweep (the
suite run recorded in retired/commons-math-suite-run-RETIRED-20260818.md).

The process is not deadlocked and is not stuck in the interpreter — it is
running compiled code, correctly, about eighty times too slowly.

## This page replaces two wrong diagnoses, and both are worth reading first

**The first** was an OSR admission gap: `trsbox`/`bobyqb` are called once per
test, so their hot loops have no compile door but OSR, and OSR refused both for
a bare `athrow` (`RBC.6`). Every word true; not the cause. `RBC.6` was lifted
(plus two further gaps that refused OSR *entry* once the compile door opened —
fixed-suite-bugs/jit/bobyqa-hot-loop-refused-osr-because-of-a-bare-athrow-FIXED-20260817.md).
Both methods now compile and are entered, and **the wall time did not move**.

**The second was this page's own first draft**, which read a Java-frame sampler,
saw ~57% of samples in `ArrayRealVector.getEntry` / `Array2DRowRealMatrix.getEntry`
/ `setEntry`, and concluded the lever was **"inline the accessor"**. That is a
sum reported as a cause. The accessor is *where* the time is; it is not *what*
the time is spent on. Measured below: of the accessor's 13.2 ns, the call itself
is 3.4 and **the `getfield` inside it is 9.7**.

Both mistakes have the same shape — a true observation at the wrong altitude —
and both were settled the same way, by subtraction on a fast instrument rather
than by argument on a slow one.

## The instrument

A 45-second `optimize()` call over a commons-math checkout cannot be iterated
against. `probes/AccessorDispatchProbe.java` is the shape alone: commons-math's
own accessor bytecode (`aload_0; getfield data:[D; iload_1; daload; dreturn`,
exception table included) against arms that remove one ingredient each. Every
arm is called once, so OSR is its only compile door — the same door
`trsbox`/`bobyqb` use — and each reports the **minimum** of its timed rounds, so
a competing build on this shared host shows up as spread rather than as bias.

## Where the time actually goes

Windows, 32-core, ZGC (default), JIT on. Three reps agreed to ±0.3 ns; one
representative run:

| arm | ns/iter | what it adds to the row above |
|---|---:|---|
| `raw` | 2.21 | the array load inline — the floor |
| `emptyCall` | 5.56 | a call to `static double zero(){ return 0.0; }` |
| `virtualEmpty` | 6.26 | making that call virtual instead of static |
| `staticArrayGet` | 5.73 | passing args + doing the array load in the callee |
| `plainGetter` | 15.44 | reading the array out of `this` with a `getfield` |
| `tableGetter` | 18.12 | commons-math's exception table |
| `primFieldGet` | 14.93 | the same call reading a **primitive `int`** field |

as subtractions, on two hosts:

| term | Windows | Azure Linux |
|---|---:|---:|
| `callTax` — enter+leave a compiled callee | 3.35 | 6.68 |
| `virtualTax` — virtual vs static dispatch | 0.70 | 1.69 |
| `argsAndLoadTax` — args + array load in the callee | 0.17 | 0.47 |
| **`receiverFieldTax` — ONE `getfield`** | **9.71** | **13.23** |
| `tableTax` — the exception table | ~0 | −0.16 |
| `primFieldTax` — one `getfield` on a **primitive** | **8.67** | **13.19** |

**HotSpot reads every one of those taxes at |x| ≤ 0.06 ns.** It inlines all of
it; the whole table is the cost of not inlining, and its largest term by far is
a single field read. Two hosts, two operating systems, the same ordering.

## What that rules out, cheaply, and what it leaves

Each of these was a live hypothesis with a page behind it. Each died to one
measurement:

* **The callee's exception table is not the problem.** `tableTax` reads ~0. The
  MIC/PIC bar that once cost 11.4x per call
  (`CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH`) is default-ON and doing its job.
* **The compiled call does not go out to Rust.** `CRATONVM_DBG=mic-prof` over a
  run making 40 M+ accessor calls reports `mic_calls=3` and `disp_calls=195`.
  The inline cache serves essentially all of them from machine code.
* **It is not reference-vs-primitive.** `primFieldTax` is the falsifying arm: a
  primitive `int` field costs the **same** ~8.7 ns as the `double[]` reference.
  The getfield page's plan (3) — a per-field-KIND gate letting primitive fields
  load inline while reference fields keep a barrier — would therefore not fix
  this.
* **Publishing the arena bounds does not help either.** Generational, which
  *does* publish `JIT_REGION_BOUNDS` where ZGC never does, measured **worse**:
  `callTax` 15.2/16.4/15.8 against ZGC's 13.3/12.8/13.1. That is the getfield
  page's plan (2), measured.
* Neither `CRATONVM_JIT_GETFIELD_HELPER=1` (force the helper, delete the guard)
  nor `CRATONVM_JIT_INLINE_GETFIELD=1` (raw inline) moves the number by more
  than 0.7 ns. The guard is pure overhead; both flags choose between two
  expensive options.

## Why, in the instruction stream

`CRATONVM_DBG_JIT_DISASM=1` on `double getEntryNoTable(int){ return data[index]; }`
— **579 bytes** of machine code, of which the field read is:

```
 a9: mov  rax,[rbp-48h]           ; receiver
 ad: test rax,rax / je            ; null check
 b6: and  rcx,7   / jne           ; alignment check
 c3: mov  rdx,<&JIT_REGION_BOUNDS>
 cd: cmp  rax,[rdx]     / jb      ; region 0 base
 da: cmp  rax,[rdx+8]   / jb      ;          end
 e7: cmp  rax,[rdx+10h] / jb      ; region 1
 f4: cmp  rax,[rdx+18h] / jb
101: cmp  rax,[rdx+20h] / jb      ; region 2
10e: cmp  rax,[rdx+28h] / jae
11b: test byte [rax+0Fh],4        ; GC_FLAG_COMPACT
128: mov  rax,[rax+10h]           ; <- the load, finally
134: (slow path) call jit_getfield ; then compare RAX against the deopt sentinel
```

`perf` says which side of that branch runs. Azure Linux, `perf record -F 999`,
DSO split **66.1% JIT-compiled code / 31.2% VM binary** — and of that 31.2%:

| symbol | share of run |
|---|---:|
| `cratonvm_vm::jit::helpers::jit_getfield` | 9.89% |
| `VmHeap::is_object_address` | 6.41% |
| `ZObjectStarts::contains` | 4.40% |
| **the getfield helper chain** | **20.7%** |

**Two-thirds of all VM-binary time in this run is one helper**, and the profile
agrees with the subtraction it was run to check.

## The engagement counter, which is the number the fix needs

The getfield page's step 1 is "instrument before fixing … a fix priced on
anything but that counter is a guess", because three signals (gates default-on,
35 sites emitted, codegen arm unit-tested) all report **emission** and none asks
whether the inline branch ever *runs*.

`jit_getfield` now increments `GETFIELD_HELPER_CALLS`, printed as
`[GETFIELD_CENSUS] helper_calls=…` on the existing `CRATONVM_DBG=mic-prof` dump.
Reaching that function at all means the guard fell through, so the count *is*
the miss count — and this probe gives it an exact denominator, because four arms
do `rounds × per` field reads and nothing else:

| rounds | field reads | `helper_calls` | miss rate |
|---:|---:|---:|---:|
| 3 | 24 000 000 | 23 993 196 | 99.972% |
| 5 | 40 000 000 | 39 993 236 | 99.983% |
| 8 | 64 000 000 | 63 993 131 | 99.989% |

The shortfall is **constant at ~6 800, not proportional** — which says more than
the percentage does: the inline branch works for a few thousand reads during
startup and then never again for the rest of the process. In steady state the
fast path is taken **zero** times.

## Follow-up 2026-08-18: what is left is ONE reference-field read under ZGC

Two fixes landed on `dev` while this page was being written
(`a6f0ecf75`, `8787edbbe`, both on the getfield page's own branch). Re-measured
on Azure Linux, quiet host (load 1.3), same probe, 3 ZGC reps + 2 Generational:

| term | ZGC (default) | Generational |
|---|---:|---:|
| `callTax` | 2.85 | 2.46 |
| `virtualTax` | 0.31 | 0.71 |
| `argsAndLoadTax` | 0.6 | 0.9 |
| **`receiverFieldTax` — a REFERENCE field** | **25.2** | **0.95** |
| `primFieldTax` — a PRIMITIVE field | 1.51 | 1.51 |
| **`totalGetterTax`** | **28.7** | **4.3** |

Three things follow, and they finish this page's line of enquiry:

* **Primitive field reads are fixed**, on both collectors: 13.19 → **1.51** ns,
  an 8.7x improvement, and `[GETFIELD_CENSUS] helper_calls` drops from 4 arms'
  worth of reads to 3 — `primFieldGet` no longer calls the helper **at all**.
* **A reference field costs 25.2 ns on ZGC and 0.95 ns on Generational — 26x.**
  The whole residual accessor tax is that one operation, and one collector
  already does it in a nanosecond.
* **The single-pass arm was never the problem.** The getfield page's open item 1
  asks why `stack_oop_marks_exact` is false at these sites. It is not false:
  with a per-clause emission diagnostic on
  `receiver_is_trusted_oop`'s three conjuncts, **zero** sites report a refusal,
  and the blocking site prints `getfield pc=1 off=0 ref=true`. The single-pass
  arm takes its shortcut; the misses are all the **IR/C2** arm, whose own
  shortcut is primitives-only *by design*, because an inline load of a
  reference field under ZGC hands on a `Z_COLORED_TAG | colour | offset` word
  that has had no load barrier.

So the remaining lever is not a guard bug and not an admission gap. It is the
ZGC JIT load barrier (`feature-designs/zgc-jit-load-barrier.md`), or the
read-side bounds table the getfield page proposes as its own item 2. Until one
of those exists, **`BOBYQAOptimizer`'s accessors — `ArrayRealVector.data` and
`Array2DRowRealMatrix.data` are both reference fields — cannot inline on the
default collector**, and that is the whole of what is left of this page.

One observation was offered here with its caveat — that the reference-field path
appeared to have got *slower* (13.23 → 25.2) even as the primitive path got 8.7x
faster — and it has since been bisected. **It was real, and it was a
diagnostic.** `1794c8e81` put an uncached `runtime_var_os("CRATONVM_DBG_COMPACT_INLINE")`
inline in `jit_getfield`: a string-keyed flag lookup on the hottest helper in
the VM, 3.4x on every reference-field read, fixed by caching it in a `OnceLock`
like the line above it. See the CORRECTION section of the getfield page.

So the numbers in the table above are inflated on the ZGC row only (a primitive
field inlines and never enters the helper). Re-measured with the gate cached:
**ZGC reference 12.5-15.3 ns, Generational reference 2.1-3.4 ns** — the residual
is ~5x, not 26x. Every conclusion above holds in direction; the magnitude is
five times smaller than published.

## What would fix it

The lever is a single field read costing ~9 ns where HotSpot pays a load, taken
essentially every time. In order:

1. **Make the guarded inline `getfield` actually take its inline branch.** That
   is the whole of the gap and it has its own page. What this page adds is the
   price (9.7 ns/read, 20.7% of the profile, 99.98% miss) and two of that page's
   three candidate fixes measured and refused. The counter narrows the third:
   the miss rate is 99.98% under the default, 99.98% under
   `CRATONVM_JIT_INLINE_GETFIELD=1` (which removes the region compares from the
   emitted code entirely) and 99.98% under Generational. The fall-through
   survives removing the region test AND switching to the collector that
   publishes it, so the discriminator is downstream of both — the compact-layout
   tag test `test byte [rax+0Fh],4`, or the site's own admission.
2. **Then the call itself**: `callTax` 3.4 ns (Windows) / 6.7 (Linux) to enter
   and leave a compiled callee — a 160-byte frame, three argument spills, two
   zeroing stores, two `gs:`-relative TLS publishes and a safepoint poll, for a
   method whose body is one load. A leaf accessor needs none of it. Note the
   ordering: this is the item the first draft called #1, and it is worth a third
   of what it thought.
3. The residual arithmetic. The sibling
   [`bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`](bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md)
   has the only `perf` profile of that: flat, largest symbol 5.06%, **~2% in the
   arithmetic itself**, the rest name/metadata resolution, heap address
   validation and native dispatch plumbing. Two commons-math workloads, two
   arithmetic surfaces, the same answer — per-operation VM plumbing spread thin.

## Reproduction

```bash
javac -d . probes/AccessorDispatchProbe.java
java     -cp . AccessorDispatchProbe 5 2000000          # the oracle: every tax ~0
cratonvm --java-home <jdk> -cp . AccessorDispatchProbe 5 2000000

# the engagement counter, against an exact denominator of rounds x per x 4 arms
CRATONVM_DBG=mic-prof cratonvm … AccessorDispatchProbe 5 2000000 2>&1 \
  | grep GETFIELD_CENSUS
# the guard in the instruction stream
CRATONVM_DBG_JIT_DISASM=1 cratonvm … AccessorDispatchProbe 2 200000 2>&1 \
  | grep -A 40 'getEntryNoTable'
# the collector arm that publishes JIT_REGION_BOUNDS, and does not help
cratonvm --XX:UseGc Generational … AccessorDispatchProbe 5 2000000
```

The workload itself, for the wall clock:

```bash
CP="<commons-math test classpath — /data/cm-legacy-classpath.txt on the Azure Linux box>"
javac -nowarn -cp "$CP" -d . probes/BobyqaOne.java
java     -cp ".;$CP" BobyqaOne 12 1                                    # 0.5 s
cratonvm --java-home <jdk> --Xmx 1g -c ".;$CP" BobyqaOne 12 1          # ~45 s
cratonvm --java-home <jdk> --nojit --Xmx 1g -c ".;$CP" BobyqaOne 12 1  # ~52 s
```

The JIT buys ~15%; `CRATONVM_DBG=jit-method-stats` reports
`hot_but_stuck_in_interpreter=0`, so no admission gate can close this. HotSpot
finishes the full class in 1 806 ms (17 of 18 tests, 1 skipped).

## Related

* [`../jit/every-jit-getfield-takes-the-helper-because-the-guarded-inline-check-always-fails-20260817.md`](../jit/every-jit-getfield-takes-the-helper-because-the-guarded-inline-check-always-fails-20260817.md)
  — owns the fix. This page supplies its step-1 counter, the per-read price, and
  the measured refusal of two of its three candidate fixes.
* fixed-suite-bugs/jit/bobyqa-hot-loop-refused-osr-because-of-a-bare-athrow-FIXED-20260817.md
  — the OSR gate this workload was first blamed on. Real gate, really fixed,
  worth ~29x on the shape it governs, worth nothing here.
* [`bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`](bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md)
  — the other commons-math throughput page, and the one with an actual `perf`
  profile. Its conclusion — flat, ~2% arithmetic, the rest VM plumbing — is the
  independent corroboration this page's sampler could only gesture at.
* retired/commons-math-suite-run-RETIRED-20260818.md — the suite run both were
  found from, now closed.
