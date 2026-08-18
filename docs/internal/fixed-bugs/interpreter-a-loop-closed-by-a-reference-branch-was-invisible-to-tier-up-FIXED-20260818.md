# A loop closed by a reference branch was invisible to tier-up — FIXED 2026-08-18

**Status:** FIXED.

**Reproducers:** `probes/BackEdgeShapeProbe.java` (the tier-up hole),
`probes/InterpFastPathCoverageProbe.java` (the opcode-coverage gap),
`difftest/seeds/InterpFastPathParity.java` (fast-path / decoded-path parity).

## What was wrong

The interpreter's back-edge accounting — bump `Frame::backward_count`, record a
PGO back edge, offer the frame to `try_osr_with_backoff` — lived only in the
raw-bytecode fast path in `execute_frame_from_index`, and only on `goto` (0xa7)
and the twelve int-comparison branches. `try_osr_with_backoff`'s own comment
calls itself

> the one funnel all fourteen back-edge sites go through

and that was true. Fourteen was the number of *arms that had been written*, not
the number of branches that can close a loop.

`ifnull` (0xc6), `ifnonnull` (0xc7), `if_acmpeq` (0xa5) and `if_acmpne` (0xa6)
had **no fast-path arm at all**. They fell through to the decoded handler in
`interpreter/opcodes.rs`, which sets `frame.pc` and does nothing else. A loop
closed by one of those four therefore:

* never recorded a PGO back edge,
* never incremented `Frame::backward_count`, so it could not reach
  `OSR_THRESHOLD` and could never enter an OSR-compiled body, and
* never earned whole-method tier-up credit either, because
  `pop_and_recycle_frame_with_reason` feeds `ProfileStore::add_loop_work` from
  that same counter.

`goto_w`, `tableswitch`, `lookupswitch` and `ret` had the same hole for the same
reason, and under `-Xverify:none` — where `use_fast_path` is false and *every*
opcode takes the decoded handler — so did the entire instruction set.

## Why it survived

javac puts a `while`/`for` loop test at the **top** and closes the loop with
`goto`. Both of the obvious shapes are therefore accounted, which is why nothing
noticed:

```
  private static long intEdge(int[] a)      // for (int i = 0; i < a.length; i++)
    ...
     7: if_icmpge  23
    20: goto       4        <-- back edge is goto, ACCOUNTED
```

The shape that was not accounted is `do { … } while (ref-condition)`:

```
  private static long condEdge(Node head)   // do { … } while (p != null)
    18: ifnonnull  4        <-- back edge is ifnonnull, UNACCOUNTED
```

That is not an exotic shape. It is every `do`/`while` over a linked structure,
and it is what a **bottom-test frontend emits for an ordinary `while` loop** —
ECJ, and the Kotlin and Scala backends. Nothing in this tree's own corpus is
built that way, which is exactly why the gap sat in the one funnel every
back-edge site was believed to go through.

## Measurements

`probes/BackEdgeShapeProbe.java` builds ONE 200 000-node chain and walks it
twice per rep. The two walks read the same fields, do the same arithmetic and
touch the same memory in the same order. The only difference between them is the
opcode that closes the loop: `while (p != null)` gives `goto`, and
`do { … } while (p != null)` gives `ifnonnull`. Arms interleaved, three runs of
each binary, alternating binaries:

| run | binary | `gotoEdge` ms | `condEdge` ms | ratio | `osr=` | checksum |
|-----|--------|--------------:|--------------:|------:|-------:|----------|
| 1 | base  | 4 | 434 | 87.6 | 2 | 127469280 |
| 1 | fixed | 4 |   4 |  1.02 | 3 | 127469280 |
| 2 | base  | 4 | 433 | 91.9 | 2 | 127469280 |
| 2 | fixed | 4 |   4 |  1.05 | 3 | 127469280 |
| 3 | base  | 4 | 421 | 86.7 | 2 | 127469280 |
| 3 | fixed | 4 |   4 |  0.98 | 3 | 127469280 |

**~105x on that loop, and the two shapes are now indistinguishable — which is
the correct outcome, not a suspiciously good one.** They are the same loop.

`osr=` is the engagement counter beside the number: 2 → 3 is `condEdge`'s OSR
compile, the one the old accounting could never request. The checksum is
identical in every row, so the arms agree on the answer.

### The opcode-coverage gap, separately

The same audit found the fast path had grown up around a set of opcodes and
never been completed: it had arms for `ishl`/`ishr`/`iushr` but not the long
shifts, `ineg`/`lneg` but not `fneg`/`dneg`, `irem`/`lrem` but not
`frem`/`drem`, `lcmp` but not `fcmpl`/`fcmpg`/`dcmpl`/`dcmpg`,
`i2l`/`i2f`/`i2d`/`l2i` but not the other eight conversions, and `dup`/`pop` but
not the other six stack shuffles.

`probes/InterpFastPathCoverageProbe.java` carries its own control: one kernel
built only from opcodes that already had arms, one dominated by opcodes that did
not. Under `--nojit` (with the JIT on, both kernels tier up and the
interpreter's per-opcode cost stops being what is measured), six interleaved
runs, medians:

| kernel | base ms | fixed ms | delta |
|--------|--------:|---------:|-------|
| `controlKernel` (pre-existing arms) | 756 | 740 | −2.1% |
| `coverageKernel` (new arms)         | 1885 | 1407 | **−25.4%** |

The control's own run-to-run spread on the base binary is 727–851 ms, which
covers its −2.1%; read it as "did not move". The coverage kernel's spread is
1851–2120 base against 1357–1425 fixed — no overlap.

## The fix

Two halves, because the hole had two.

**`cond_branch_arm!`** — the twenty lines every conditional-branch arm repeats,
written once, immediately above the dispatch loop. The eleven int arms that
carried a byte-identical copy now call it, the twelfth (`if_icmpge`, whose copy
differed only in whitespace) does too, and so do four NEW arms for `if_acmpeq`,
`if_acmpne`, `ifnull` and `ifnonnull`. The macro takes `frame`, `saved_pc`, `b1`
and `b2` as parameters because `macro_rules!` gives local-variable identifiers
definition-site hygiene and those four are bound inside the loop.

`if_acmpne`'s arm declines while `CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE` is
armed, so the decoded arm that owns that instrument still runs; the gate is
hoisted per-`execute_frame` next to `pgo_enabled`.

**A generic back-edge hook on the decoded path**, in the
`Ok(InstructionResult::Continue)` arm, keyed on the resulting pc being below
`saved_pc` rather than on a list of opcodes. That covers `goto_w`,
`tableswitch`, `lookupswitch`, `ret` and every branch under `-noverify` — and
any opcode added later, which an enumeration would have to remember. `Continue`
is the only result it can key off: `FramePushed` and `Return` change the frame
under `frame_idx`, and a thrown exception leaves through `Err`, so a handler
landing pad below `saved_pc` is not mistaken for a loop back edge.

## Four more defects from the same audit

* **`iinc` could panic.** It was the one fast-path local-access arm without a
  `b1 < max_locals` guard; its `iload`/`istore`/`astore`/`lstore` neighbours all
  carry one. `set_local_int_unchecked` indexes `frame.locals` unchecked, so a
  per-class `skip_verification` class — which the global `use_fast_path` gate
  does **not** cover — reached it with an out-of-range index and panicked, in
  the module whose header declares it must never panic in production.

* **`refs_equal` was not reflexive on the null handle.** A JNI `jobject` null
  crossing the operand stack as raw bits arrives as `Value::Long(0)`.
  `ref_operand_is_null` calls that the null reference and `refs_equal` already
  answered `true` for the mixed pair (`Long(0)` vs `Object(None)`) — but the
  pair where both sides are the smuggled form had no arm, so
  `if_acmpeq(nullHandle, nullHandle)` answered `false` while
  `if_acmpeq(nullHandle, null)` and `ifnull(nullHandle)` both answered `true`.

* **`lookupswitch` was a linear scan.** JVMS §6.5 requires the key table sorted,
  and `lookupswitch` is what javac emits for the `hashCode()` arm of a string
  switch and for sparse `enum`/`int` switches — tables that routinely run to
  hundreds of entries. `LookupSwitch::target` is now a binary search with a
  linear fallback, and the fallback is the correctness argument rather than
  decoration: this VM can run with verification skipped and nothing else on the
  path proves the table is ordered. The JIT already did
  "CMP chain for small, binary search for large"
  (`jit/src/x64/bytecode_walk.rs`); the interpreter was the outlier.

* **`TOTAL_INSTRUCTIONS` never counted one.** Its only bump site is the top of
  `execute_instruction`, which the fast path reaches only when it has no arm for
  the opcode — so read as a total it under-reported by the fast path's share,
  which is the large majority of executed bytecodes, and any ratio taken against
  it came out inflated by exactly the factor nobody had measured. Renamed
  `DECODED_INSTRUCTIONS`. (`arraylength`'s decoded arm also built three
  `String`s per execution for a message only a null receiver reads; now
  `Arc<str>` clones.)

## Verification

`difftest/seeds/InterpFastPathParity.java` exercises all 28 newly-fast-pathed
opcodes — confirmed present by `javap`, not assumed — at the inputs where two
implementations of one opcode could plausibly disagree: NaN (which of `l`/`g`
answers), the §2.8.3 saturating float→integer narrowings, shift distances above
the operand width, signed zero, and a category-2 value through every shuffle.

It runs on the `jit-on`, `nojit` and `interp-decoded` axes, and that is what
makes it a comparison rather than a self-check: `interp-decoded` is the only
mode that passes `--noverify`, so it takes the decoded arms while the other two
take the new fast-path arms. **The seed is only meaningful on the fixed binary**
— on the base binary all three axes reach the decoded handler for these opcodes
and agree trivially.

The path split was confirmed directly rather than assumed, via
`CRATONVM_DBG_HOTPATH_COUNTS`: the same run reports `decoded_instr=4572` in
fast-path mode and `decoded_instr=40117` under `--noverify`. All 84 output lines
are identical across fast path, decoded path and HotSpot.

Both new unit tests were confirmed to FAIL against the code they guard before
being kept:

* `instruction::tests::lookupswitch_target_resolves_sorted_and_unsorted_tables`
  — deleting the linear fallback fails it on key 5 of the unsorted table
  (observed: `left: -1, right: 40`).
* `test_refs_equal_jni_null_handle_is_reflexive` — deleting the
  `(Long(0), Long(0))` arm fails its first assertion.

## What this leaves open

The fast path still has no arm for the opcodes that dominate real OO bytecode:
`getfield`, `putfield`, `getstatic`, `putstatic`, `ldc`/`ldc_w`/`ldc2_w`, `new`,
`checkcast`, `instanceof`, `tableswitch`/`lookupswitch`, and
`monitorenter`/`monitorexit`. Every one of them pays the quickened-stream
resolve plus the ~200-arm `Instruction` match on every execution. The
`SiteCache` (`interpreter/site_cache.rs`) already answers a warm field site in
an array index and two integer compares — a fast-path arm on top of it is the
largest single interpreter win still on the table, and is a separate change with
its own measurement.

Three smaller ones from the same reading:

* The operand stack keeps a `kinds` side array parallel to `slots`, so every
  push and pop is two stores instead of one. Folding the kind into the
  `CompactValue` tag space would halve that traffic.
* `ValueStack::push_compact` marks `KIND_UNKNOWN` by construction, which is why
  the shuffle opcodes need the `*_with_kind_unchecked` family added here. The
  remaining `push_compact` call sites have the same latent loss.
* The decoded handler re-indexes `thread.frames[frame_idx]` once per operand
  touched; the fast path hoists a frame pointer for exactly this reason
  (frame-arena.md §6.1). `execute_instruction` could take `&mut Frame` plus a
  narrow thread handle instead.
