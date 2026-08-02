# `NoSuchMethodError java/lang/Object.hasNext()Z` — a live iterator reads back as `ClassId(0)`

## Status
**OPEN (2026-08-02).** Reproduced on `dev` @ `750a95f8e3` with a receiver dump.
Split out of `bug-h2-testmultithread-concurrent-update-timeout.md`, where it was
a two-paragraph aside; it is a silent memory-safety defect and deserves its own
page.

Sibling face, same family, already root-caused from the other end:
`bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep.md`. **Read that page
before this one** — it establishes that `java.lang.Object` / `ClassId(0)` has
**four** possible causes and that guessing between them has already cost two
sessions.

## Severity
**HIGH** — silent. A still-referenced object is read back as an all-zero header.
Here it surfaces as a linkage error on a `java.util.Iterator`; the same corrupt
read reaching a field access is a wrong value or a SIGSEGV with no Java frame.

## Symptom

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
     method="java/lang/Object.hasNext()Z"
     caller="org/h2/test/db/TestMultiThread.testConcurrentUpdate()V @pc=252"

CRATONVM_DBG_CCE_BT: site=nsme_dispatch method=java/lang/Object.hasNext()Z
  CCE-BT-STK[3] org/h2/test/db/TestMultiThread.testConcurrentUpdate pc=252
  CCE-BT-STK[2] org/h2/test/db/TestMultiThread.test pc=28
  CCE-BT-STK[1] org/h2/test/TestBase.testFromMain pc=11
  CCE-BT-STK[0] org/h2/test/db/TestMultiThread.main pc=9
  NSME-RECV addr=0x200625bf088 tid=0 blocked=false epoch=54
  NSME-RECV SHAPE kind=Object num_fields=0 mirror_of=<not a registered mirror>
```

`@pc=252` is the `for (Future<Void> job : jobs)` result loop
(`TestMultiThread.java:381`). The receiver is the synthetic `Iterator` local
javac emits for that loop — held only by `testConcurrentUpdate`'s frame while
the thread is parked inside `job.get(5, TimeUnit.MINUTES)`.

`num_fields=0` and a class of `java/lang/Object` are the ambiguous
`ClassId(0)` face. An `ArrayList$Itr` has fields; this one has none.

A second witness, same loop shape, different method, seen 2026-08-01 on the
pre-`750a95f8e3` tree:

```
NoSuchMethodError method="java/lang/Object.next()Ljava/lang/Object;"
     caller="org/h2/test/db/TestMultiThread.testConcurrentInsert()V @pc=197"
```

HotSpot emits neither, ever.

## What is NOT yet established

The all-zero header does **not** by itself say the young sweep did it. Per the
sibling page, `ClassId(0)` is also an un-hashed `new Object()`, an old-gen free
block whose bytes were re-zeroed by `OldGen::allocate`, and the zeroed tail of
an old-gen compaction. **Do not repeat the inference that page had to retract.**

Two facts that narrow it, both from the reproducing run:

* every young collection in that run took the **non-moving** fallback
  (`[moving-young] fallback #64: reason=compiled-frame-oop-not-published`, and
  the same reason at #4/#5/#6/#7/#16/#32). Nothing was relocated, so this is a
  *reclamation* gap, not a remap gap — which rules out one of the two
  mechanisms, not three of the four causes;
* the victim is reachable only from a Java frame local of a thread inside a
  blocking region, which is the `deposit_root_snapshot` path
  (`vm/src/vm/vm_exec.rs`), not the `collect_roots` path.

## What this session added: the verdict is now flag-free

`GenerationalHeap::reclaimed_hole_at` (added by the sibling investigation) asks
the heap whether an address is inside a free-list hole, past the allocation
frontier, or in the inactive semispace — none of which a live object can be.
It was wired into the interpreted `checkcast` path and the compiled `checkcast`
path, and **into nothing else**. A dispatch miss wears the identical face and
reported nothing, so the run above produced an ambiguous dump and no verdict.

`vm/src/memory/reclaim_guard.rs` now holds that verdict once, and the `invoke`
dispatch terminal in `vm_exec.rs` calls it — gated on `ClassId(0)`, because
that terminal is also reached in bulk on healthy runs (a missing method on a
synthetic classpath stub logs there on every call) and the probe takes the
young and old heap locks. `jit::helpers`' copy was migrated onto the same
function; the two `checkcast` copies had already drifted (only the
interpreter's consulted the young-sweep ring), and a third copy would have
compounded that.

The next occurrence prints which region the receiver is in, and — via the
unconditional old-gen reclamation ring — what the block held and which
collector freed it, **with no flag set in advance**. That is the one thing
neither this page nor its sibling could get from the run that reproduces.

## Reproducing

```bash
cd <fresh writable dir>          # H2 writes ./data
CRATONVM_DBG_CCE_BT=1 <cratonvm> --java-home /home/victor/jdk25 --Xmx 1g \
  -c "<h2>/target/classes:<h2>/target/test-classes:$(cat <h2>/craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

Rate: **1 occurrence in 6 runs** — ~2 400 s of class runtime, 2026-08-02,
16-core Azure host at load 6-15. Budget several runs; it is nowhere near a
per-run certainty.

| run | flags | outcome | NSME |
| --- | --- | --- | --- |
| 1 | `DBG_CCE_BT` | pass, 727 s | **1** |
| 2 | `+DBG_REMAP_TRACE +DBG_ZERO_RANGES` | fail (`LOCK_TIMEOUT`), 348 s | 0 |
| 3 | same | pass, 428 s | 0 |
| 4 | same | pass, 420 s | 0 |
| 5 | none (guard binary) | pass, 430 s | 0 |
| 6 | none (guard binary) | pass, 445 s | 0 |

Note runs 2-4 carry `CRATONVM_DBG_REMAP_TRACE`, which maintains a per-object
ring and changes timing. It is NOT one of the flags that feed
`retain_dead_objects` (checked — those are `DBG_SWEEP_ZERO`, `DBG_A2`,
`DBG_SWEEP_CENSUS`, `DBG_WATCHREF`), so it does not swap the collector; but 0/3
under it against 1/1 without it is too small a sample to say it does not perturb
the race. Prefer an uninstrumented run now that the verdict is unconditional.

`docs/internal/repros/h2-blocked-frame-roots/BlockedFrameRootProbe.java` is a
reduced driver — `main` parked in `Future.get()` over the same enhanced-for
while N workers allocate hard, with a canary object and the `jobs` list checked
after each round. **It does not reproduce** (40 rounds × 8 threads at
`--Xmx 64m`, 0 events). It is committed as a *negative* result so the next
person does not rebuild it: whatever the trigger is, a shallow interpreted main
frame parked in `get()` is not sufficient. The H2 run has deep JIT-compiled
frames on the parked thread — the fallback reason names exactly that
(`compiled-frame-oop-not-published`) — and that difference is the next thing to
put into a probe.

## Related

* `bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep.md` — same family,
  old-gen face, has the `reclaimed_hole_at` verdict and the reclamation ring.
* `../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`
  — the same all-zero receiver reaching `Thread.clone`.
* `bug-h2-testmultithread-concurrent-update-timeout.md` — the class this was
  found in, whose own problem is throughput, not this.
