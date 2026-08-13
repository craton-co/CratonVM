# netty full-suite FAIL / HANG / ABORTED buckets — untriaged

**Status:** OPEN (carried forward 2026-08-12). Nothing here has been
individually adjudicated against HotSpot; this doc exists so the observations
are not lost, not because any of them is a confirmed CratonVM defect.

These are the non-CRASH observations from the 3-GC-variant 657-class netty run
at `70c8b8cd6` (Windows). The CRASH bucket from that same run was root-caused
and fixed on 2026-08-12 — see
`docs/internal/fixed-bugs/netty-jni-native-codec-sigsegv-FIXED-20260812.md`,
which is where the original report lives in full. That work did **not** touch
any of the below.

## The counts, as measured

657 classes each, ~87 min wall each:

| variant | PASS | FAIL | HANG | CRASH | ABORTED | NOTESTS |
|---|---|---|---|---|---|---|
| default (`-XX:+UseGenerationalGC`) | 425 | 131 | 50 | 5 | 8 | 38 |
| g1 (`-XX:+UseG1GC`) | 425 | 126 | 55 | 5 | 8 | 38 |
| zgc (`-XX:+UseZGC`) | 427 | 130 | 49 | 5 | 8 | 38 |

CRASH, ABORTED and NOTESTS are *exactly* identical across the three; PASS /
FAIL / HANG vary by a few classes. So the FAIL and HANG buckets are largely
GC-independent — which is evidence against a GC-triggered correctness bug and
says nothing about what they actually are.

**The CRASH=5 column is now 0** (four classes at 100%, the fifth matching
HotSpot exactly). The rest of the table has not been re-measured since; the
numbers above are the 2026-08-12 pre-fix run and should be regenerated before
any of them is quoted as current.

## FAIL bucket (~130 per variant) — needs a signature pass

Not triaged. The harness's own auto-extracted `sig` column was **empty for
119 of the 131** default-variant FAILs, so this needs a raw-log read rather
than a tsv scan. The work is: dedupe by exception signature, then one HotSpot
cross-check per distinct signature. Nothing in this bucket can be called a
confirmed CratonVM bug before that.

## HANG bucket (~50 per variant) — one sample, one explanation, 49 unchecked

25 of the ~50 cluster in `io.netty.buffer` (allocator / pooled-ByteBuf classes
with large parameterized method counts). **One** of them was sampled:
`PooledByteBufAllocatorTest` at a 600 s cap instead of the suite's 180 s
completed in **202 s** (`ABORTED=1`, the same result type HotSpot produces),
against HotSpot's **17 s**. So that one is not a deadlock — it is a ~12x
throughput gap that happens to cross the cutoff.

That is a sample of one. Treat the other 24 as *likely* the same pattern and
not as confirmed. The 16 `io.netty.handler.codec` HANGs and the smaller
`util.concurrent` / `resolver.dns` / `handler.ssl` / `handler.pcap` clusters
have not been sampled **at all** and could be genuine hangs.

## `found=N aborted=N` classes — never cross-checked

Several classes report an `aborted` count equal to their full test count, e.g.
`WrappedUnpooledUnsafeByteBufTest` (`found=413 aborted=413`) and
`LittleEndianUnsafeNoCleanerDirectByteBufTest` (`found=412 aborted=412`).

JUnit 5 "aborted" (as distinct from "failed") normally means every test hit a
failed `Assumption`. The plausible reading, given the `Unsafe` / `NoCleaner`
naming, is that these assert `PlatformDependent.hasUnsafe()` or similar and
CratonVM's answer differs from HotSpot's — a graceful skip, not a crash. That
is a hypothesis from the class names, not a measurement: **HotSpot may abort
the same classes for the same reason**, in which case there is nothing here at
all. One run of these classes under HotSpot settles it.

## Related

* `docs/internal/fixed-bugs/netty-jni-native-codec-sigsegv-FIXED-20260812.md`
  — the CRASH bucket from this same run, root-caused and closed.
* `apps/netty-suite-runner/README.md` — the harness, and the Linux host's own
  earlier validation run (whose "every concrete class CRASHes" finding is from
  2026-08-10 and predates several heap fixes; re-measure before relying on it).
