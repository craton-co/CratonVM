# `TestMVStoreTool` never finishes its CREATE phase on CratonVM — every collector

| | |
|---|---|
| **Status** | **OPEN.** Not a correctness defect and not the corrupt-header family; split out of `g1-eight-byte-write-at-a-live-objects-base-20260906` so that page's scope stays on the header corruption. |
| **Scope** | `org.h2.test.store.TestMVStoreTool` at `-Xmx256m`, Windows, on the corpus rebuilt 2026-09-06 (see below). Reproduced on G1, ZGC and Generational. |
| **Oracle** | HotSpot 25.0.3 on the SAME classpath: `rc=0`, whole class in about four minutes. |

## The measurement

One binary (`cratonvm`, merged dev tip of 2026-09-07), one classpath, one
workload, `-Xmx256m`, timeouts as marked:

| collector | outcome | wall | how far it got |
|---|---|---|---|
| G1, no mark driver | `OutOfMemoryError: Java heap space` | 84 s | no output line at all |
| G1, `CRATONVM_G1_JIT_MARK_DRIVER=1` | TIMEOUT | 1800 s x4 | **still in CREATE**, ~7959 / 7094 / 1394 / 4749 pauses |
| ZGC | `OutOfMemoryError ... (native reference array of length 14053)` | 632 s | `Created in 589094 ms` |
| Generational | TIMEOUT | 1800 s | `Created in 1466595 ms` |
| **HotSpot** | **PASS** | **~4 min** | whole class |

Generational spends **1467 seconds** in a create phase HotSpot completes as part
of a four-minute total run. ZGC spends 589. G1 with the driver never emits the
line at all inside 30 minutes while doing ~4.4 GC pauses per second.

## What it is not

* **Not the corrupt-header family.** That reproduces only with
  `CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST=0` and is G1-only; this is on every
  collector and needs no flag.
* **Not the rebuilt corpus.** HotSpot passes `rc=0` on exactly this classpath.
* **Not purely the mark driver**, though the driver is implicated in the G1
  arm: without it G1 OOMs at 84 s, with it G1 survives but does not progress.
  The driver converts a fast OOM into a slow non-completion, which is worth
  knowing about a feature that is opt-in for throughput reasons.

## Where to start

The create phase is `MVStore` writing chunks through `FileStore`; the
CratonVM-side cost is unmeasured. A CPU profile of the G1-with-driver arm
against the HotSpot arm over the first 60 s would say whether this is
allocation rate, GC pause frequency, or the `nio`/`ByteBuffer` natives that
`DataUtils.writeStringData` sits on — the same natives implicated in the
`BufferOverflowException` this workload also throws under G1 (see that page).

## The corpus this was measured on

`/c/craton/h2corpus` `classes/`, `test-classes/` and `cp.txt` were deleted on
2026-09-06 at 17:46 — and `/c/craton/h2root/target/{classes,test-classes}` are
SYMLINKS into them, so every session's H2 workload broke at once. Rebuilt from
the intact sources at `C:/craton/CratonVM1/apps/h2database/h2/src` with all
three source roots (`main`, `tools`, `test`) in ONE `javac` invocation, because
they are mutually dependent — `src/tools` holds `org.h2.dev.*`, which the tests
import, and `src/tools` imports back into `org.h2.test.utils`. Resources copied,
`cp.txt` regenerated in its original form. 1705 classes; HotSpot passes on it.

It is H2 **2.4.249**, which may differ from the deleted build. Any comparison
against a measurement taken before 2026-09-06 17:46 is cross-corpus and must
say so.
