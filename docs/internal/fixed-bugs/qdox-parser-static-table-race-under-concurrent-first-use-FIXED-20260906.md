# `TestContextAotGeneratorIntegrationTests.processAheadOfTimeWithWebTests` — two defects, neither of them a static-field publication race

**Status: FIXED 2026-09-06.** The test passes 4/4 on all three collectors
(Generational, G1, ZGC); it was 0/4, 0/4 and 1/4 on the tip of `dev` the day
before. The concurrent QDox reproducer this page was built around goes from
"1-11 lost parses per run on ZGC and SIGSEGV 3/3 on Generational" to
**24 000 concurrent parses with zero failures across all three collectors**.

## What the page said, and what was actually true

The open page proposed that CratonVM has "weaker static-field-publication
guarantees than HotSpot under concurrent first use of a class", on the strength
of a probe that storms QDox's `Parser` class from four threads and produces

```
ArrayIndexOutOfBoundsException: Index N out of bounds for length 0
```

**That hypothesis is refuted, and the probe was measuring something else.**

* **It is not first use.** `QdoxConcProbe3 ... 1` performs one complete
  single-threaded parse -- driving `Parser` and everything it touches all the
  way to `Initialized` -- before any worker starts. The failures survive
  unchanged.
* **It is not the static fields.** `Parser.yytable` / `yycheck` are written by
  a single `putstatic` at the end of a 33 KB static method called from
  `<clinit>`; a racing reader can only see `null` or the finished array. The
  observed length is `0`, which is neither.
* **It is the JIT and the collector.** `--nojit` runs 1 600 parses clean where
  the JIT arm SIGSEGVs in 90 s; `CRATONVM_NO_MOVING_YOUNG=1` runs clean; one
  thread runs clean and two threads do not.

"...out of bounds for length 0" is not a publication symptom at all. It is
**ZGC's own documented signature** for a conservative root left naming a span
the slide vacated: `compact_low_to` zeroes what it vacates, so a stale
reference reads a valid all-zero header and the array's length comes back `0`.
The comment that says so has stood in `gc/src/zgc.rs` since compaction shipped.

## Defect 1 — a parked thread's conservative JIT pins were published only under G1

`gc_quiescence`'s process-global pin registry holds the addresses a
CONSERVATIVE scan found in JIT frames: words that look like object bases but
whose holding slot the collector cannot rewrite, so the object must not MOVE.
G1 excludes their regions from the collection set; ZGC withholds their pages
from the relocation set.

Three sites publish into that registry, and **all three were gated on
`heap.is_g1()`** -- the safepoint deposit (`update_root_snapshot`), the
blocked-region deposit (`NativeContextImpl::deposit_root_snapshot_inner`) and
the initiator's own gather (`memory::roots::collect_roots`). Their comments say
why, and the reason expired twice without the gate moving:

> they can only over-retain (the young sweep runs non-moving while any thread
> is in JIT, so nothing is relocated)

* **ZGC compaction shipped 2026-08-13** and consumes this registry. Gated on
  G1, its consumer saw an empty set on its own collector -- which for a
  pin-by-value consumer does not read as "nothing to pin", it reads as
  "relocate everything".
* **The cross-thread JIT coverage handshake landed 2026-08-23** and let the
  generational moving-young cycle run while peers are in JIT, so its Cheney
  copy began relocating exactly the objects the comment promised it would not.

The fix publishes on every backend (`CRATONVM_GC_G1_ONLY_JIT_PINS=1` restores
the gate) and uses replace-semantics at the initiator too, so its entry
describes the current cycle rather than accumulating pre-move addresses.

A Cheney copy cannot honour a pin -- every live object in from-space is copied
-- so the generational collector additionally takes the non-moving sweep on any
cycle where a conservative JIT scan ran and a compiled frame is live
(`CRATONVM_GC_NO_PEER_PIN_DIVERT=1` restores the old behaviour). Two narrower
rules were measured and rejected: "a pin lands in young-from" left 1 crash in
3, and "the pin set is non-empty" left 3 in 3 -- the registry is
under-approximate (`is_object_address` drops interior and derived pointers, and
a peer whose last deposit saw no JIT frames publishes nothing), so a per-address
test inside it cannot stand in for the population's existence. G1 and ZGC absorb
that gap because they pin at region/page granularity; an all-or-nothing
per-cycle decision has no such slack.

## Defect 2 — the JIT compiler resolved constant-pool classes loader-blind, and DEFINED duplicates

The Spring test's own failure was never the AIOOBE. It was

```
java.lang.ClassCastException: class com.thoughtworks.qdox.parser.structs.TypeDef
    cannot be cast to class com.thoughtworks.qdox.parser.structs.TypeDef
        at com.thoughtworks.qdox.parser.impl.Parser.yyparse(Parser.java:3177)
```

`CRATONVM_TRACE_CLASSVALUE=1` names it in one line:

```
[cv-checkcast-fail] typecheck REFUSED: obj_cid=5444 obj_loader=Application
    obj_cls=com/thoughtworks/qdox/parser/structs/TypeDef
    site_target=5400 site_target_loader=UserDefined(3)
```

The site is correct: `intern_typecheck_target` resolved this `checkcast`'s
`CONSTANT_Class` through the compiling class's own loader and got the fork
loader's `TypeDef`. The OBJECT is the Application copy. Both copies are real --
`CompileWithForkedClassLoaderClassLoader` re-defines the classpath on purpose,
and HotSpot does the same -- so the only question is which of them the compiled
code should have allocated.

`CRATONVM_DBG_DEFINE=1` + `CRATONVM_DBG_DUPCLASS=1` show where the Application
copy came from:

```
[DBG_DUPCLASS] fallback candidate ClassId(5400) (loader=UserDefined(3)) for ".../TypeDef"
[DBG_DUPCLASS] rejecting existing UserDefined-loader candidate ... so a SEPARATE
               ClassId will be created under Application
  0: resolve_fast_path_class_id ... 7: load_class_concurrent_for
  9: compile_osr_artifact  10: background_compile_task
```

The **background JIT compiler**, OSR-compiling the fork loader's
`Parser.yyparse`, resolved that method's `new` / `anewarray` / trivial-ctor
constant-pool entries with `SharedVm::load_class_concurrent` -- which is
deliberately loader-blind, and whose own doc says it "can only ever produce a
Bootstrap/Extension/Application-loaded class". For a method whose owner was
defined by a user loader that is not a lookup, it is a **definition**: it
refuses the fork loader's copy and mints a second one under `Application`,
which the compiler then bakes into the site. Compiled code allocated
Application-namespace `TypeDef`s inside a fork-namespace parser, and the site's
own (correct) `checkcast` refused them.

`jit_bridge::resolve_cp_class_for_owner` replaces those four calls. An owner
defined by a built-in loader keeps `load_class_concurrent` unchanged; an owner
defined by a user loader is answered by a LOOKUP through
`get_loaded_class_id_for_requester`, which prefers that loader's own definition
and never defines. A miss declines the optimisation -- every one of those sites
already has a deferred path for an unresolvable target, and the interpreter
resolves it correctly at run time. Declining to optimise is always available;
minting a second class identity is not.
`CRATONVM_JIT_LOADER_BLIND_CP_RESOLVE=1` restores the old call.

**The page half-guessed this one.** Its "live candidate" was "CratonVM's own
background JIT compiler thread racing the interpreter thread over the same
class's metadata during first use". Right thread, wrong mechanism: not a race
-- a resolution that used the wrong dictionary, deterministically.

## Evidence

`repros/qdox-parser-static-array-race/QdoxConcProbe3.java`, 4 threads x 400
fresh-builder parses of the same generated source (Azure `20.80.105.49`, one
binary per arm):

| collector | before | after |
|---|---|---|
| ZGC (default) | 1, 3, 5, 9, 9, 11 failures per run | **0 in 9 runs** (14 400 parses) |
| Generational | SIGSEGV 3/3, in compiled `java/lang/StringUTF16.compress`, faulting inside a span the collector had just decommitted | **0 in 9 runs** |
| G1 | clean (the one backend the pin gate admitted) | clean |

ZGC keeps compacting: `compaction_cycles` 3-27 and `objects_relocated`
11 000-141 000 per run after the fix, with `relocation_on_proven_jit` non-zero
-- the pins withhold pages, they do not stop relocation.

The Spring class itself, `run-suite.sh run --list` with one class:

| collector | before | after |
|---|---|---|
| Generational | `found=4 succ=0 fail=4` | `found=4 succ=4 fail=0` |
| ZGC | `found=4 succ=1 fail=3` | `found=4 succ=4 fail=0` |
| G1 | `found=4 succ=0 fail=4` | `found=4 succ=4 fail=0` |

## Ruled out, so nobody re-runs them

* A static-field publication race (the page's own hypothesis) -- warmup does not
  help; the fields are written by one `putstatic`; the observed value is a
  zero-length array, not `null`.
* The 2026-08-06 `native_unmod_get` / `al_state` bug -- confirmed intact by the
  page, and the real cause touches neither.
* A stale TLAB skip span -- `CRATONVM_GC_NO_TLAB_SKIP=1` still SIGSEGVs 3/3.
  The `skip_spans_hold_no_root` guard DOES fire once on this workload, and that
  fire is a true positive for a different, already-fixed defect; it is not this
  one.
* Constructor inlining (`CRATONVM_JIT_DISABLE_INLINE_NEW=1`), the inline-splice
  coverage claim (`CRATONVM_JIT_INLINE_OOP_COVERAGE=0`) and the pinned-peer
  depth credit (`CRATONVM_XT_PINNED_PEER_DEPTH=0`) -- all still crash 3/3.
* The cross-thread JIT coverage handshake
  (`CRATONVM_XT_JIT_COVERAGE_HANDSHAKE=0`) reads clean, but it is a MASK, not
  the defect: it only lowers how often relocation happens under a live compiled
  frame (`relocation_on_proven_jit` 3 -> 1), and ZGC still lost a parse with it
  set at one thread.

## Diagnostics kept

Three instruments were hard-coded to one past investigation's class names and
are now driven by `CRATONVM_DBG_DUPCLASS_FILTER`, so one filter drives the
whole question:

* `[DEFINE-DBG]` -- every `define_class`, with its loader.
* `[DEFINE-DBG-JAVA]` -- the JAVA callers of a `ClassLoader.defineClass`. A
  loader that defines a class its parent already has is either an isolating
  loader doing its job or a delegation that failed, and only the caller chain
  separates the two.
* `[cv-checkcast-fail] typecheck REFUSED` now prints `site_target` and both
  sides' loaders. Its old `target_cid` is the loader-BLIND unique-by-name
  answer, which is `None` the moment two loaders define the name -- so on
  exactly the class-identity failures it exists to explain, it reported nothing.
