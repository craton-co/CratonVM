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
bug should wait for. Left for whoever wants the instruction back.

The cost is one word load per deferred phi per edge, on edges that alias;
`phi_copy_publish_deferred` (printed by `CRATONVM_DBG_IR_LINEAR_SCAN`) counts
them so a future session can see whether it ever fires. The alternative —
running the resolver over the union of words and registers — would have to
model a copy that writes two locations at once, which the `CopyOp` shape does
not express.

## Blast radius

This is not a `java.time` bug. Any method whose merge carries two or more
long-typed phis whose sources the allocator gave the same registers as the
destinations could take it, and the failure is silent wrong arithmetic, not a
crash. It reached us through `Duration.toNanos()` only because
`LocalTime.truncatedTo` turns a wrong answer there into a division by zero —
i.e. because one consumer happened to be loud. Anything reading a wrong
`toNanos()` quietly (a timeout, a duration comparison, a metric) would not have
been noticed at all.
