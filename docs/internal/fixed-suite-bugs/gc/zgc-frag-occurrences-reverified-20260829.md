# The ZGC fragmentation occurrences, re-verified — one closed, one re-attributed, one still open (and one JIT defect in the way)

**Retired 2026-08-29**, replacing
`docs/known-issues/gc/zgc-arena-fragmentation-occurrences-to-reverify-20260829.md`.
That page existed so three occurrences outside H2 "are not lost, and so nobody
reads 'the H2 classes pass' as 'the family is closed' without running them". It
specified the matrix; this is the matrix, run.

Binaries: CratonVM `dev@96e07ca86` (Windows, and a Linux build of the same tip on
the Azure host). The Hibernate arms additionally carry this branch's
self-recursion JIT fix, for the reason in §4.

## The verdicts

| occurrence | verdict |
|---|---|
| Spring `SimpleClientHttpResponseTests` | **re-attributed OFF this family** — hangs with the gauge firing ZERO times |
| Hibernate `sql.exec.SmokeTests` | **closed** — passes in all four arms, and not because of the 2026-08-29 repairs |
| Hibernate `DefaultCatalogAndSchemaTest` | **still open**, and it is a LOW-end failure the 2026-08-29 repair cannot reach |

## 1. Spring `SimpleClientHttpResponseTests` — not fragmentation

All four arms rc=124 at a 500 s cap; real HotSpot passes 5/5 in **9 s**. The
`hi0` arm timed out with **zero** `zgc frag gauge` lines, and so did a separate
240 s `CRATONVM_GC_STATS=1` run — a symptom absent while the failure is
unchanged is not the cause.

And the page's own "symptom to match on" was matching the wrong guard. Its
livelock signature was *"the guard's own `occurrence` counter doubles on every
firing (32768 -> 1048576 -> …)"*. With the ANSI escapes stripped, every
`occurrence=` in the failing log belongs to the **descriptor-coercion** guards
(G30-1 and its W7-84 autobox sibling); the `zgc frag gauge` line carries no
`occurrence` field at all. Both live in `cratonvm::gc::guard`, which is how two
defects came to share one signature.

Detail and the safe way to census it:
`known-issues/gc/simpleclienthttpresponse-hang-is-not-arena-fragmentation-20260829.md`.

## 2. Hibernate `sql.exec.SmokeTests` — closed, and NOT by the repairs

PASS `17/17` in all four arms, `zgc frag gauge` lines **0**, and
`hi_cycles=0 declined=0` in every arm that reported them.

That last pair is the distinction the parent page asked for and is why it asked:
*"A class that stops failing while `cycles=0 declined=N` did not stop failing
because of the high-end compactor."* Here it is stronger than that — `declined`
is 0 as well, so the compactor was never offered a candidate at all. The
`both=0` arm, which is the pre-2026-08-29 behaviour byte for byte, also passes.
Something else closed this class between 2026-08-11 and now.

## 3. Hibernate `DefaultCatalogAndSchemaTest` — open, and at the LOW end

No `@@RESULT` in any arm, 4 `OutOfMemoryError`s per run on the default arm,
against real HotSpot's `found=132 ok=132 failed=0` in 77 s.

The failing requests are **16400** and **21008** bytes. The high end serves
requests `>= 65536`, so these are LOW-end requests and the 2026-08-29 high-end
compactor cannot apply — `hi_cycles=0 declined=0` on every arm, exactly as that
implies. No arm of the specified matrix could have moved this class, and none
did.

The diagnostic already names the wall: **one 32-byte
`org/hibernate/type/format/jackson/JacksonJsonFormatMapper`**, `walls=1`,
standing between 21,240 free bytes and a 21,272-byte contiguous window, on a
heap with 880 MB on the free list whose largest block is 14,232 bytes.
Full detail:
`known-issues/gc/zgc-low-end-fragmentation-defaultcatalogandschema-20260829.md`.

## 4. A JIT defect was standing in front of occurrence 3, and it is fixed

Before this branch's fix, `DefaultCatalogAndSchemaTest` produced no `@@RESULT`
in any arm **and no fragmentation lines either** — it looked like a hang with no
GC signal. The cause was one line of stderr during Hibernate bootstrap:

```text
thread 'cratonvm-jit-compiler' panicked at jit/src/ir_lower.rs:3282:38:
index out of bounds: the len is 4 but the index is 4
```

`emit_self_recursive_call` marshals the VM context pointer plus every Java
argument into an entry-ABI register — four of them on Win64. A self-recursive
method with 4+ arguments indexes off the end. The panic is on the BACKGROUND
COMPILER THREAD, so answers stay correct and the VM carries on — but the thread
does not come back, and MEASURED with `CRATONVM_DBG_JIT_COMPILED=1` on
`probes/SelfRecArgs.java`: f1/f2/f3 compile, f4 panics, and then **nothing
compiles for the rest of the process**. The class ran to its 600 s cap entirely
interpreted.

With the fix it reaches its ACTUAL failure, which is §3. That is the whole value
of it here: **a verification matrix run against a class that cannot reach its own
failure measures nothing**, and the four green-looking "NORESULT, frag=0" rows
the first pass produced would have been read as "the fragmentation is gone".

Gate: `jit/tests/ir_vs_singlepass.rs::selfrec_more_args_than_entry_regs_still_compiles`,
falsified by removing the guard (panics) and restoring it (passes).

## What this corrects in the record

* Three occurrences filed under one "symptom to match on" turned out to be three
  different things: a coercion storm, a closed-by-something-else class, and a
  low-end fragmentation failure. Grouping by symptom is what put them together;
  the `occurrence=` counter belonging to a neighbouring guard in the same module
  is what kept them there.
* **Ask which END the request came from before running any compaction arm.**
  `request=16400` settles in one line what a four-arm matrix costs an hour to
  say. The parent page's matrix was well specified and still could not have
  produced a useful answer for occurrence 3, because both of its levers act on
  the other end of the arena.
* An engagement line you cannot obtain is worth saying so: the page asked for
  `[GC] zgc-high-compaction: cycles= declined=` "on the default arm", and those
  print at VM exit. For a class that never terminates they never print. They are
  in the Hibernate table because those runs exit; they are absent from the Spring
  table for the same reason.
