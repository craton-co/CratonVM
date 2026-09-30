# G1: heap-filling probes exit before their first output line with an uncaught `OutOfMemoryError`

> **STATUS (2026-09-30, G1 lane `g1-gc-fix-0930`): OPEN, narrowed; now also
> the home of the G1 rows the retired
> `docs/internal/gc/g1-humongous-refusal-with-free-space-and-old-array-fixup-misses-FIXED-20260930.md`
> carried.** Two runs per row, Linux release builds, plain `dev` `a93a98c3e`
> against the lane's build (young evacuation-headroom trigger, fresh
> last-ditch mark, cleanup scrub):
>
> | Probe | Heap | `dev` | lane |
> |---|---|---|---|
> | `FullHeapResolveProbe` (`--compatible`) | `-Xmx64m` | rc 1, no stdout | rc 1, no stdout (with `CRATONVM_DISABLE_JIT=1` or `CRATONVM_JIT_OSR=0`: `round 0 lambda-0`, then rc 1) |
> | `GenR4W4NativeStringOomProbe 4096` | `-Xmx64m` | rc 1, no stdout | rc 1, no stdout |
> | `NativeGrowthReclaimProbe` | `-Xmx128m` | rc 1, no stdout | rc 1, `alCapacity=4/4 alAdd=0/4 toCharArray=0/4 sbCapacity=4/4 PROBE-FAIL` |
> | `GenR4W4HeapFullThrashProbe` | `-Xmx128m` | rc 1, no stdout | rc 124 (killed at 300 s) |
> | `GenR4W6JitOomRootProbe` | `-Xmx64m` | 4 `PASS` lines, rc 124 | the same |
>
> What the `FullHeapResolveProbe` runs show (`CRATONVM_DBG=g1diag`):
>
> - **The young generation is not bounded on the compiled allocation path.**
>   `jit_newarray` collects only when an allocation FAILS; it never asks
>   `needs_gc`, so the lane's evacuation-headroom trigger cannot act there.
>   With the JIT on, the first pause started at 29 Eden / 35 Free and the
>   third at 31 Eden / 2 Free; interpreted, the survival predictor had fallen
>   to near zero over ~3000 one-region pauses and admitted 41 Eden against 15
>   Free just as the next 90%-live fill began.
> - **An evacuation failure then wedges young collection.** The pause keeps
>   every region it could not empty as Survivor, so it ends at 0 Free with the
>   whole young generation in the next collection set; each later young pause
>   copies nothing and frees nothing (`63M->63M`) until the data dies. HotSpot
>   keeps evacuation-failed regions as Old, and recovers with
>   `Pause Full (G1 Compaction Pause)` -- HotSpot's G1 passes this probe only
>   by running several of them.
> - Not yet verified: which allocation throws the uncaught error. The
>   candidate is the one outside the probe's `try`, `junk = new
>   byte[pieces][]` at the top of the next round, which needs the previous
>   round's dropped data reclaimed first.

> **STATUS (2026-09-29, gc defects round, orchestrator, wave d10 verification): OPEN, measured, cause not classified.**
> This is a G1 backend finding. G1 internals were out of scope for the GC defects round, so this page only records the measurement for a G1 lane.

## What was measured

The build was the Linux release `cratonvm-gcd-d10` of the round branch at `b30b8abaa`, run side by side with wave d9's `cratonvm-gcd-d9`. Each row ran twice per binary, from `docs/internal/gc-defects-round-20260927/verify-d10/vd10.list`, with `-XX:+UseG1GC`. Both binaries behave the same on these rows:

| Probe | Heap | Stdout | rc |
|---|---|---|---|
| `FullHeapResolveProbe` (`--compatible`) | `-Xmx64m` | empty | 1 |
| `GenR4W4NativeStringOomProbe 4096` | `-Xmx64m` | empty | 1 |
| `NativeGrowthReclaimProbe` (and with `CRATONVM_JIT_OSR=0`) | `-Xmx128m` | empty | 1 |
| `GenR4W4HeapFullThrashProbe` | `-Xmx128m` | empty | 1 |
| `GenR4W6JitOomRootProbe` | `-Xmx64m` | partial (`PASS` lines), then killed by the timeout | 124 |

Every rc-1 run ends with:

```
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/OutOfMemoryError: Java heap space
[cratonvm-cli] (no Java stack frames were captured for this exception)
```

Some stderr files also carry `[g1] pause #N ran with a live compiled frame and an EMPTY JIT root publication`. That line appears in 2 of the 2 `FullHeapResolveProbe` files and in 3 of the 3 thrash files.

The same probes on HotSpot 25.0.4 print their lines and exit rc 0 under `-XX:+UseG1GC`. Checked for `GenR4W4NativeStringOomProbe 4096`: HotSpot prints `fill: OutOfMemoryError "Java heap space"`, `native-strings ok`, `recovered ok`, `PASS`. On CratonVM with Generational and ZGC, the same rows print output (see `common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications.md`).

Other G1 rows at the same heap sizes run normally, for example `Gcd1JniRootsProbe -Xmx64m`. So this is not a start-up failure at a small heap. It is the first heap-filling phase, whose `OutOfMemoryError` should be caught by the probe. The error escapes with no Java frames, which suggests it is thrown from a door that has no Java frame to attach, or that the catching frame is lost.

## Why it matters

Every G1 row of the OOME battery is blind until this is fixed:
- `gcd-d10o-g1-latched-overhead-exit-skips-the-marking-cycle-20260928.md` cannot be verified.
- Item 1 of the w34 page (`FullHeapResolveProbe`) cannot be verified on G1.

## Next step

Rerun one row with `CRATONVM_DBG_ATHROW=1` to get the throw site:

```
CRATONVM_DBG_ATHROW=1 cratonvm -XX:+UseG1GC -Xmx64m -cp tools/bench GenR4W4NativeStringOomProbe 4096
```

Then decide which it is:
- The G1 allocation path throws from a native or boot door that bypasses the probe's `catch`.
- A G1 humongous refusal is involved (`g1-humongous-refusal-with-free-space-and-old-array-fixup-misses.md`: the w34 page already gates its G1 `NativeGrowthReclaimProbe` row on it).
