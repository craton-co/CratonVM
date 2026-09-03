# A compiled `for (e : treeMap.tailMap(k).entrySet())` iterates ZERO entries, once

## Status

**OPEN, reduced and localized 2026-09-03.** Not fixed. What is new is a
one-second deterministic reproducer, a localization to a single method's
compiled body, and a list of subsystems that are ruled OUT.

This is the defect that currently **blocks**
`jit/bug-box-unbox-intrinsic-segv-under-relocation-20260902`: H2
`TestRandomMapOps` dies here at 11-22 s, and that page's SIGSEGV does not appear
until 25-183 s.

## The reproducer

`probes/TreeTailIterProbe.java` -- 42 hard-coded keys, one hot loop, no H2:

```
javac -d /tmp/tti probes/TreeTailIterProbe.java
cratonvm --java-home $JDK25 -cp /tmp/tti TreeTailIterProbe 3000
```

HotSpot: `badTail=0 badHead=0`. CratonVM: `FIRST tail divergence iter=502
got=0 want=6`. Deterministic -- the same iteration every run.

## What is measured

The map has 42 keys, first 55, last 1985. For `from = 1810` the VM answers
correctly everywhere except one place:

| question | answer |
|---|---|
| `ceilingKey(1810)` | `1937` correct |
| `higherKey(1809)` | `1937` correct |
| `tailMap(1810).size()` | `6` correct |
| `tailMap(1810).entrySet().size()` | `6` correct |
| `for (e : tailMap(1810).entrySet()) n++` | **`0`** |

And the decisive one, asked *inside the compiled body on the failing call*: the
loop yields 0, then re-iterating **the same view object** in another method
yields 6.

```
EMPTY-ITER sub.size=6 sub.isEmpty=false subCls=java.util.TreeMap
           esSize=6 esCls=java.util.TreeMap$EntrySet mSize=42 reIter=6
```

The data is intact. The compiled loop is wrong.

## Where it is

`CRATONVM_JIT_DENY=TreeTailIterProbe.iterTail` makes it clean, and denying the
caller (`main`) does not. So the defect is in that one method's compiled body.
With tier-up switches flipped the failure moves between `iterTail` and
`iterHead`, which says the same thing about whichever one compiles at that
moment.

**How many iterations fail depends on the probe, and the difference is worth
knowing before you read a count.** Without the `EMPTY-ITER` diagnostic the
method fails exactly ONCE, on the first entry to the freshly compiled body, and
is right forever after -- which is what first suggested a compile transition.
The committed probe carries the diagnostic, and then fails persistently
(`badTail=2496` of 3000 from iteration 503): the extra code in the cold path
changes the method enough that whatever repaired it no longer happens. Same
first failure, different recovery. Do not read the count as severity.

`subCls=java.util.TreeMap`, not a `NavigableSubMap`: the native `tailMap`
returns a natively-managed TreeMap. `native-collections/src/lib.rs` records why
that matters -- such a map "navigates the `root` field a natively-managed
TreeMap never populates", which is exactly what an iterator built from real JDK
bytecode would read, and exactly what an empty iteration looks like beside a
correct `size()`.

**That is a hypothesis, not a finding.** What is measured is the table above and
the `CRATONVM_JIT_DENY` arm.

## Ruled OUT (each arm run, each still fails at iter 502)

`CRATONVM_DISABLE_JIT=1` is the only arm that makes it clean. These do not:

* `CRATONVM_JIT_NO_OSR=1` -- not the OSR entry
* `CRATONVM_JIT_IR=0` -- the single-pass tier gets it wrong too
* `CRATONVM_JIT_IR_INLINE=0` -- not IR inlining
* `CRATONVM_JIT_SP_INLINE_IC=0` -- not the inline MIC/PIC cascade
* `CRATONVM_JIT_VIRTUAL_TIERUP=0`, `LOOP_WORK_TIERUP=0`,
  `C2_ALLOC_UPGRADE=0` -- not the recompile doors
* `CRATONVM_ZGC_RELOCATE=0`, `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1`,
  the BOX_UNBOX family either way -- unrelated

One lever that looked clean was not: `CRATONVM_JIT_SP_IC_DENY='*'` is a
substring match, not a glob, so it matched nothing and its "no change" said
nothing. Check a bisect lever engages before reading it.

## Next step

`iterTail` compiles TWICE (12 877 then 10 476 bytes for a four-line method).
Dump both and find which call in the loop -- `entrySet()`, `iterator()`,
`hasNext()` -- reaches real `TreeMap` bytecode instead of its native shadow:

```
CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=TreeTailIterProbe.iterTail \
  cratonvm --java-home $JDK25 -cp /tmp/tti TreeTailIterProbe 1200
```
