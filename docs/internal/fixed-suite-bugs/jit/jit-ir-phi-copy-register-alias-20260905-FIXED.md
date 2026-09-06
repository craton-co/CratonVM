# The optimizing tier's phi copies were a parallel assignment in memory and a sequence in registers — `Duration.toNanos()` returned `seconds * 1e9 + seconds` — FIXED

**Status:** FIXED 2026-09-05, `jit/src/ir_lower.rs` (`emit_phi_copies` /
`emit_copy_op`). Regression test
`vm/tests/jit_ir_phi_copy_register_alias.rs` with fixture
`vm/tests/jit_ir_phi_copy_register_alias_fixtures/PhiCopyRegisterAliasProbe.java`.

Found while retiring the hibernate-reactive MySQL checkpoint-timeout page
(`../hibernate-reactive/mysql-beforeeach-checkpoint-timeout-was-a-30s-budget-20260905-CLOSED.md`):
it was the
one genuine correctness defect in that page's 24-class FAIL set, and the only
class that failed in ISOLATION on a quiet host.

## Symptom, outermost first

`org.hibernate.reactive.types.BasicTypesAndCallbacksForAllDBsTest`, MySQL,
one class per JVM, no load, no timeout:

```
testLocalDateTimeType : java.lang.ArithmeticException: / by zero
        at java.time.LocalTime.truncatedTo(LocalTime.java:991)
        at java.time.LocalDateTime.truncatedTo(LocalDateTime.java:1120)
        at ...BasicTypesAndCallbacksForAllDBsTest.testLocalDateTimeType(:301)
testInstant           : the same, wrapped in a CompletionException
```

Line 301 is `LocalDateTime.now().truncatedTo(ChronoUnit.MILLIS)`.
`LocalTime.truncatedTo` divides by `unit.getDuration().toNanos()`, so the
divisor was zero.

A four-line probe took it the rest of the way, in two seconds:

```
ChronoUnit.MILLIS.getDuration().toNanos()   -> 0  (want 1000000)
        198,356 wrong answers in 200,000 calls, first wrong at call 1,644
--nojit : 0 wrong.   HotSpot : 0 wrong.   cold (first ~1600 calls) : correct.
```

The `Duration` object was fine throughout — `getSeconds()` answered 0,
`getNano()` answered 1000000, `toString()` answered `PT0.001S`. Only
`toNanos()` (and `toMillis()`, which shares its body's shape) was wrong, and
the wrong values were not noise:

```
Duration.ofSeconds(2, 5).toNanos()  -> 2000000002   (want 2000000005)
Duration.ofSeconds(1, 0).toNanos()  -> 1000000001   (want 1000000000)
Duration.ofMillis(1).toNanos()      -> 0            (want    1000000)
```

Every one of them is `seconds * 1e9 + seconds`. The addend was reading
`seconds` where the method asks for `nanos`.

## Two things that were NOT it

* **Not the natives.** `Duration.toNanos` IS registered
  (`native_dur_to_nanos`), but `Duration.ofSeconds(Long.MAX_VALUE/2).toNanos()`
  threw `ArithmeticException: long overflow` both before and after the switch —
  that is `Math.multiplyExact` from the JDK classfile body, not the i128
  native, so the real bytecode was running from call 0 and the native was not
  in the picture. (This also rules out the 2026-09-05 `final`-devirt native
  screen, which is in this base: `CRATONVM_JIT_FINAL_DEVIRT=0` and
  `CRATONVM_JIT_FINAL_DEVIRT_NATIVE_SCREEN=0` both still reproduce.)
* **Not `Math.addExact` / `multiplyExact`, and not `getfield`.** A user class
  reproducing the same call shape — `Math.multiplyExact(long,int)` then
  `Math.addExact(long, i2l int)`, out of two fields — is clean at 30,000
  iterations. So is the same shape with the branch removed, and so is a
  version with two `long` fields.

## What it takes to reproduce

Three ingredients, and all three are required
(`PhiCopyRegisterAliasProbe.java` keeps each control):

1. two `long` locals, one of them seeded from an **`int`** field through `i2l`;
2. a **conditional block that reassigns BOTH**, so the merge carries two phis;
3. both read after the merge.

Drop any one — use a `long` field for the second, reassign only one local,
remove the branch — and it is correct. That is `java.time.Duration.toNanos()`
bytecode-for-bytecode:

```
 0: getfield seconds:J ; lstore_1        // local 1 = seconds
 5: getfield nanos:I   ; i2l ; lstore_3  // local 3 = (long) nanos
11: lload_1 ; lconst_0 ; lcmp ; ifge 27
17:   lload_1 ; lconst_1 ; ladd  ; lstore_1
21:   lload_3 ; ldc2_w 1e9 ; lsub ; lstore_3
27: lload_1 ; ldc2_w 1e9 ; multiplyExact ; lload_3 ; addExact ; lreturn
```

## Root cause, in the disassembly

`CRATONVM_DBG_JIT_DISASM=<Class>.<method>` on the minimised shape (it is
`PhiCopyRegisterAliasProbe.shape` in the checked-in fixture). The
single-pass artifact is correct. The optimizing-tier recompile has this in the
**not-taken** arm's merge block, where `r13` holds `seconds`, `rbx` holds
`(long) nanos`, and `r12` is phi_nanos's register:

```
368: mov rax,r13    ; read phi_seconds's source
36b: mov [rbp-0B0h],rax
372: mov rbx,rax    ; PUBLISH phi_seconds -- whose register is RBX
375: mov rax,rbx    ; read phi_nanos's SOURCE -- whose register is ALSO RBX
378: mov r12,rax    ; PUBLISH phi_nanos == seconds
```

The taken arm is correct, because there both sources come from frame words the
branch had just written.

`emit_phi_copies` sequentialises the copies with `resolve_parallel_copy`, which
is correct and has been since the swap-shaped-loop fix — but it works over
**frame words**. The register fast path added on 2026-09-04
(`ir_phi_copy_regs_enabled`, `ir_phi_residency_enabled`) then reads a source
from its assigned GPR and publishes a destination into the phi's assigned GPR,
and **two distinct frame words can share a register**: the allocator sees a
phi's live range as beginning after the merge and a dying source's as ending at
it, so they do not interfere by its reckoning. They do interfere across the
copy sequence, and the word-level order says nothing about it.

The safety argument in `emit_copy_op` names the gap exactly, and is wrong only
in its last clause:

> a register is updated at exactly the instruction that writes the word
> (below), so a register and its home word go stale at the same point, and an
> order that protects one protects the other.

They go stale at the same point *for the same value*. This publish wrote a
DIFFERENT value's register.

Both kill switches confirm the owner — each, on its own, takes both the probe
and `Duration.toNanos` to zero wrong answers on the same binary:

```
BASE                              shape first-bad=571   Duration.toNanos first-bad=342
CRATONVM_JIT_IR_PHI_COPY_REGS=0   shape clean           Duration.toNanos clean
CRATONVM_JIT_IR_PHI_RESIDENCY=0   shape clean           Duration.toNanos clean
CRATONVM_JIT_IR_SKIP_REPUBLISH=0  shape first-bad=6000  (delays it; does not fix it)
```

## The fix

Per edge, and conservative: a phi whose publish register is **also** some
source's register on that edge does not publish inline. Its home store is kept
(so there is a word to read back) and the trailing residency loop — which
already exists, for the FP half and for self-copies the resolver drops —
publishes it from that word, after every source on the edge has been read.
Every other phi keeps the reg-to-reg publish it had.

### The other place this could have been fixed

`regalloc.rs` already models the outgoing-edge position — "one position past
the block's last instruction: the outgoing edge, where `emit_phi_copies` reads
this block's phi arguments" — so a source's interval reaches the edge. What it
does not do is extend the PHI's interval back to that same position, which is
why a phi and a source read on the same edge can be given one register. Doing
that in the allocator is the root fix and would make the screen above
unnecessary; it also lengthens every phi's live range at every incoming edge,
which changes pressure and allocation across the whole tier. That is a
measurement project, not a correctness fix, and it is not what a wrong-answer
bug should wait for.

**DONE 2026-09-06, opt-in: `CRATONVM_JIT_IR_PHI_EDGE_INTERFERE=1`.** The loop
in `build_live_model` that attributes a phi's k-th value input to the k-th
predecessor's outgoing-edge position already extends the SOURCE's interval to
that position; it never extended the PHI's. One `lo[phi] = lo[phi].min(at)`
there is the whole change. Only `lo` moves, deliberately: the phi is not USED
at that position, so pushing a use would distort the spill heuristics, and
setting `phi_out_bits` would make the phi live-OUT of a block that does not
define it, which the backward dataflow would then propagate live-IN through
every predecessor -- turning a one-position extension into a whole-CFG one.

Engagement is the point and it is measured, one binary, the fixture above:

| counter | interfere off | interfere on |
|---|---:|---:|
| `phi_copy_publish_deferred` | **3** | **0** |
| `peak_live` | 152 | 151 |
| `spilled` | 43 | 45 |
| `splits` | 34 | 37 |
| `scan_reloads` | 8 | 11 |
| compile refusals / bailouts | 0 | 0 |
| probe verdict | OK | OK |

`publish_deferred` going 3 -> 0 is the proof that the allocator now declines to
mint the aliasing at all, rather than the emitter cleaning it up afterwards --
which is exactly what "fixing it at the source" has to mean. regression-suite
is 91/91 in BOTH arms.

### DEFAULT ON, 2026-09-06 — and the probe's own numbers were the wrong ones

It shipped OFF for a day on the strength of the table above (+2 spills, +3
reloads). That table is from a fixture built to CONTAIN the aliasing, so
extending intervals there really does add interference. It says nothing about
code that does not alias, which is almost all code.

**Timing could not answer it.** CratonBench, seven phases, CPU seconds
(user+sys), arms interleaved, median of 5:

| phase | off | on | on/off |
|---|---:|---:|---:|
| arithmetic | 6.21 | 6.33 | 1.019 |
| fib | 10.87 | 10.83 | 0.996 |
| sieve | 5.73 | 5.74 | 1.002 |
| matrix | 4.02 | 3.66 | **0.910** |
| hashmap | 11.42 | 11.82 | 1.035 |
| stringregex | 0.58 | 0.56 | 0.966 |
| bintrees | 11.98 | 12.30 | 1.027 |

`publish_deferred` is **0 on every one of those phases with the flag off** — the
flag provably cannot have changed anything — so that 0.910-1.035 spread is this
host's noise floor, not a cost. `matrix` reading 9% FASTER on a workload the
flag cannot touch is the tell. (The zero is real, not an unprinted line:
`sieve` reports `reg_reads=1 reg_publishes=4 publish_deferred=0`.)

**The deterministic counters could.** Same phases, allocator counters, which do
not move with host load:

| phase | off vs on |
|---|---|
| arithmetic, fib, sieve, matrix, hashmap, stringregex | **identical** |
| bintrees | splits 23->21, scan_reloads 10->7, reg_publishes 4->6 |

Six of seven byte-identical; the seventh allocates strictly better. Extending a
phi's interval by one position changes nothing unless something else wanted
that register at that position -- which is the aliasing itself.

**And it engages on real code, not just the fixture.** `publish_deferred` on
netty, flag off -> on: `DefaultPromiseTest` **8 -> 0**, `ByteBufUtilTest`
**2 -> 0**, `ObjectCleanerTest` 0 -> 0. So the aliasing genuinely occurs in
shipped code, which is also what says the two downstream guards were
load-bearing rather than theoretical.

Gate for the flip: probe OK on default / `=0` kill switch / `--nojit` /
HotSpot; regression-suite **91/91 with the default and 91/91 with the kill
switch**; `cargo test -p cratonvm-jit` 0 errors; `cargo test -p cratonvm-types`
green; the regression test green. The kill switch is not vacuous -- setting
`CRATONVM_JIT_IR_PHI_EDGE_INTERFERE=0` restores `deferred=8` and `deferred=2`
on those two netty classes.

**Both downstream guards stay.** This removes the CAUSE; they catch anything
that still mints an aliasing edge, and `phi_copy_publish_deferred` reading
non-zero on some future workload is the signal that something does.

The cost is one word load per deferred phi per edge, on edges that alias;
`phi_copy_publish_deferred` (printed by `CRATONVM_DBG_IR_LINEAR_SCAN`) counts
them so a future session can see whether it ever fires. The alternative —
running the resolver over the union of words and registers — would have to
model a copy that writes two locations at once, which the `CopyOp` shape does
not express.

## The other lane that found this, and why both fixes stay

`11abbff47` (`fix/springboot-psl-loaderjar-20260905`, merged hours after this
one) reaches the identical root cause from a completely different workload and
fixes it a different way. Its item 4 states the same sentence this page does --
`emit_copy_op` "argues that `resolve_parallel_copy`'s slot ordering covers
registers too -- true while each register belongs to one node, false across an
edge" -- and its victim was ByteBuddy: ASM's `ff ff` forward-branch
placeholders went unpatched, CratonVM's own verifier rejected the retransformed
bytes (`branch at offset 89 targets 88`), and every `mock()` of a class failed.
Two lanes hit it the same morning because `perf/ir-defaults-on-20260905` turned
the register fast path on by default that day.

Its fix is `gp_reg_owner`: `mark_gp_reg_live` now marks the register's PREVIOUS
owner unreadable, so `resident_gpr` stops handing back a register the publish
just overwrote.

**Both are on `dev` and both are load-bearing. Do not delete either as
redundant.**

* `gp_reg_owner` TOLERATES the clobber and is the broader of the two: it covers
  every publish, not only a phi edge, and it sends the stale reader back to its
  home word.
* The screen in this page PREVENTS the clobber at the phi edge, which is the
  one case that interlock cannot repair. `emit_copy_op` reads a source whose
  home was DROPPED through `assigned_gpr`, deliberately **not**
  `resident_gpr` -- "there is nothing to fall back to". Clearing `gp_reg_live`
  therefore does not stop that read; not emitting the clobbering publish does.
  (The comment there argues such a register "is exclusively its own, a clause
  of `phi_home_droppable`". This screen is what makes that true at an edge
  rather than assumed.)

Verified together on dev tip `48fff33f7`, 44 commits after this fix landed and
with both mechanisms in the same file: the probe reads
`PHI_COPY_REGISTER_ALIAS_OK` on all five arms (default, `--nojit`, HotSpot,
`CRATONVM_JIT_IR_PHI_COPY_REGS=0`, `CRATONVM_JIT_IR_PHI_RESIDENCY=0`);
`vm/tests/jit_ir_phi_copy_register_alias.rs` passes in 4.81 s; and the
hibernate-reactive MySQL 23-class union is 22/23 one-class-per-JVM, the single
failure being `SoftDeleteCollectionTest` with `checkpointTO=2` -- the 30-second
budget of the sibling retired page on a load-20 host, not a miscompile --
while `BasicTypesAndCallbacksForAllDBsTest`, the class this fix repaired, is
28/28.

## Blast radius

This is not a `java.time` bug. Any method whose merge carries two or more
long-typed phis whose sources the allocator gave the same registers as the
destinations could take it, and the failure is silent wrong arithmetic, not a
crash. It reached us through `Duration.toNanos()` only because
`LocalTime.truncatedTo` turns a wrong answer there into a division by zero —
i.e. because one consumer happened to be loud. Anything reading a wrong
`toNanos()` quietly (a timeout, a duration comparison, a metric) would not have
been noticed at all.
