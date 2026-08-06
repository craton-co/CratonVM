# What the `CRATONVM_DBG_SITE_ALIAS` gate on `jit_invoke_virtual_mic` is worth: ~15–20 ns per helper entry

| | |
|---|---|
| **Status** | ✅ **FIXED** by `5d299ca6f` (2026-08-06 15:28). This page is the measurement that commit did not carry |
| **What it was** | `note_site_identity(...)` — a diagnostic for `CRATONVM_DBG_SITE_ALIAS` — was called **unconditionally** at the top of `jit_invoke_virtual_mic`. Two of the three call sites had the `if site_alias_detect_enabled()` guard; this one had the comment but not the `if` |
| **Cost while it was live** | ~15–20 ns on every *entry to the helper*, plus unbounded growth of a thread-local `FxHashMap<JitSiteKey, (String, String, String)>` and `[site-alias]` lines on stderr in ordinary runs |
| **Found by** | two routes independently — `scripts/jdk-only-strict-probes.sh` saw the stderr lines in both CratonVM arms of `JdkOnlyPlatformProbe` (this is what `5d299ca6f` cites), and an `OriginTrackedYamlLoaderTests` run printed `[site-alias]` events with the flag unset while reporting 25→620 distinct keys |

## The number

Three independent instruments, all on one quiet Windows box, all agreeing:

| instrument | gate on | gate off / detector live | delta |
|---|---:|---:|---:|
| **microbenchmark**, isolated (`jit_native_dispatch_profile`, 4 passes, flat) | 0.5 ns (`site_alias_detect_enabled()`) | 15.5 / 14.2 / 15.6 / 15.9 ns (`note_site_identity()` warm hit) | **~15.0 ns** |
| **two binaries**, flag unset, interleaved, n=8 each | median 116.5 ns/call | median 136.2 ns/call | **19.8 ns** (mean 19.3) |
| **one binary**, flag off vs on, interleaved, n=4 each | median 127.9 ns/call | median 143.2 ns/call | **15.3 ns** (mean 17.6) |

The one-binary row is the one with no code-layout confound: same executable,
one environment variable. The two-binary row is the A/B proper — stock `dev`
against `dev` with only that `if` removed.

For scale, an ordinary Java call on this VM is ~8.4 ns and the post-fix native
funnel is ~25 ns, so the ungated diagnostic cost **more than a whole ordinary
call**, on a path that carries every compiled call to a native method.

Spread is wide on this host (identical configurations ranged 106–157 ns), which
is why every row above is a median of interleaved runs rather than a best-of.
The microbenchmark, which is immune to that, is the tightest number and the one
to quote.

## Which call shapes actually enter the helper — and why the obvious probes measure nothing

This took three wrong instruments to establish, so it is recorded rather than
left for the next reader to rediscover. `probes/JitDispatchHelperProbe.java`
carries both arms.

| shape | enters `jit_invoke_virtual_mic` per call? | evidence |
|---|---|---|
| CratonBench `bintrees` | **no** | `static` methods on a `static final class Node`; `distinct JitSiteKeys=0` |
| CratonBench `hashmap` | **no** | served by `native-collections`; `distinct JitSiteKeys=0` |
| CratonBench `stringregex` | barely | 9 distinct keys for the whole phase |
| hot Java-target virtual call, 4 receiver types | **no** | `JIT_PIC_ENTRIES` is 4, so the polymorphic inline cache serves it in compiled code |
| hot Java-target virtual call, 8 receiver types (megamorphic) | **no** | 8.97 ns/call vs 7.27 at 4 types — slower, but still served in compiled code |
| **hot call to a NATIVE method** (`AtomicInteger.get()`) | **yes, every call** | 104.4 ns/call flag-off vs 129.7 flag-on on the same binary |

**A "megamorphic dispatch" probe is not a probe of this helper.** Going past
`JIT_PIC_ENTRIES` makes the site megamorphic and measurably slower, which looks
like the right lever — and `CRATONVM_DBG_SITE_ALIAS=1`, which makes every entry
do a hash lookup and three string comparisons, changed its timing by nothing at
either 4 or 8 types. That is the tell: if the flag does not move a shape's
timing, that shape is not entering the helper, and an A/B on it is a vacuous
green.

So an A/B of this change on CratonBench would have reported "no difference" and
been **wrong about why** — the benchmark never reaches the code. The check that
prevents that mistake is one run with `CRATONVM_DBG_SITE_ALIAS=1`: a zero key
count means the instrument is blind.

## What else the ungated call cost, besides time

* `SITE_IDENTITY` is a thread-local map documented "unbounded on purpose so
  nothing is missed" — correct for a diagnostic, a leak for an ordinary run. It
  reached **1539 distinct keys** in one `WebMvcAutoConfigurationTests` run, each
  holding three owned `String`s, per thread, never cleared.
* The event lines went to stderr with the flag unset, which is how
  `jdk-only-strict-probes.sh` caught it.

Neither is measured in nanoseconds, and either alone would justify the gate.

## Why the end-to-end suite arm is absent

`WebMvcAutoConfigurationTests` was tried first, on the strength of its 1539
distinct keys. One run takes **503 s**, and the effect being chased is well
under 1% of that — below the run-to-run spread of a Spring context suite. Eight
runs would have bought a number that could not be distinguished from noise, so
the arm was dropped in favour of the three above rather than reported as
"within noise" and left to read as "costs nothing".

## Reproducing

```text
cargo test --release -p cratonvm-vm --lib jit_native_dispatch -- --ignored --nocapture
```

gives the microbenchmark rungs, including `note_site_identity() UNGUARDED
[warm hit]`, which this branch added next to the existing
`site_alias_detect_enabled() gate [off]` rung.

For the end-to-end arms, compile `probes/JitDispatchHelperProbe.java` and run
`JitDispatchHelperProbe native 20000000`, interleaving the arms and taking
medians. Check reach first: the same command under `CRATONVM_DBG_SITE_ALIAS=1`
must be materially slower and must report a non-zero key count.
