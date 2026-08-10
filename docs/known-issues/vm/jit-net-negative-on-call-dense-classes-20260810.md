---
name: jit-net-negative-on-call-dense-classes-20260810
description: On ZipContentTests the JIT costs +118.95s CPU (+83.1%, 5/5 interleaved pairs) versus --nojit. It is not compilation (231 compiles, total_compile_time_ms=3), not slow compiled code (the JIT beats the interpreter 4.6x-98.8x on the same binary), and not the conservative JIT root scan (band_scans=0 over a whole run). A warmup-threshold sweep recovers it monotonically, and the best configuration measured is JIT-on-but-almost-nothing-compiled at 125.39s - better than full JIT (286.30s) AND better than --nojit (143.06s). The tier-up policy admits far too much on call-dense code.
metadata:
  type: known-issue
  area: jit, tiering, throughput
---

# The JIT is a net negative on call-dense classes — `ZipContentTests` at +83% CPU

**OPEN, root-caused to the admission policy and priced. Filed 2026-08-10.**

Split out of
`internal/fixed-suite-bugs/springboot/zipcontenttests-bytebuffer-accessor-call-cost-RETIRED-20260810.md`,
which found this while answering a smaller question and could not contain it:
the accessor intrinsic that page was about has a ~10% ceiling, and this is 83%.

## The measurement

`org.springframework.boot.loader.zip.ZipContentTests`, Azure Linux, `-Xmx 2g`,
five interleaved pairs, **per-process CPU time** (the host is shared and its
`load1` swung 11.4 → 27.7 during the run; wall clock is unusable here and CPU
held to sd 2.40):

| pair | JIT | `--nojit` | delta |
|---|---:|---:|---:|
| 1 | 268.24 | 145.17 | +123.07 |
| 2 | 259.38 | 142.37 | +117.01 |
| 3 | 262.03 | 143.83 | +118.20 |
| 4 | 259.25 | 141.76 | +117.49 |
| 5 | 261.13 | 142.17 | +118.96 |

**mean +118.95 s, sd 2.40, sem 1.08 → +83.1% CPU, 95% CI [+116.0, +121.9] s,
5/5.** Every run 29 tests / 0 failed on both arms.

## What it is not

Each of these was a live hypothesis killed by a measurement, not by argument.
They are recorded because the next person will have the same three ideas.

**Not compilation.** `CRATONVM_DBG=jit-method-stats`: 147 methods tracked,
`compiles: c1=136 c2=95 osr=9 deopts=5 c2_bailouts=0 total_compile_time_ms=3`.
Three milliseconds cannot be 119 seconds. The counter is not hiding a second
compile path either: the interpreter's `EagerFirstCall` door — the one that
never reports through `compilation_complete` — does not fire while
`CRATONVM_BG_COMPILE` is default-ON (`vm/src/runtime/interpreter.rs:1920`
returns `None` to interpret and leaves the worker to compile off-thread).

**Not slow compiled code.** `probes/ByteBufferScalarSplitProbe.java`, same
binary, min-of-3, ns/op — the JIT wins every arm:

| arm | JIT | `--nojit` | JIT speedup |
|---|---:|---:|---:|
| `buffer.get(int)` | 286.99 | 3091.12 | 10.8x |
| `buffer.getShort(int)` | 1010.46 | 4918.66 | 4.9x |
| `buffer.getInt()` | 844.54 | 4876.78 | 5.8x |
| raw `byte[]` read | 1.66 | 164.00 | 98.8x |

**Not the conservative JIT root scan.** This one fit everything else:
`scan_active_jit_frames` runs on every object-returning native call and, while
any chain entry lacks an oop map, blind-scans the whole stack band *below* the
compiled frame — a band whose width tracks Java stack depth, which is exactly
what separates a 3-frame microbenchmark from a 35-70-frame JUnit stack. The
optimizing IR backend publishes no oop maps and 95 of the compiles are C2.
`CRATONVM_DBG=jit-scan-prof` prices it:

| arm | scans | cache_hits | band_scans | band_words | precise_frames |
|---|---:|---:|---:|---:|---:|
| JIT | 1445 | 0 | **0** | **0** | 2614 |
| `--nojit` | 0 | 0 | 0 | 0 | 0 |

The blind band scan never runs — every entry carries precise metadata — and
1445 scans cannot be 119 s however wide the band. The `--nojit` row reading
exactly zero is the control that these counters see a JIT-only cost.

## What it is: the admission policy admits too much

Sweeping the warmup threshold recovers the whole delta, monotonically:

| `CRATONVM_JIT_THRESHOLD` | CPU (s) | c1 | c2 | osr |
|---:|---:|---:|---:|---:|
| 500 (default) | 286.30 | 136 | 95 | 9 |
| 5 000 | 281.92 | 118 | 82 | 9 |
| 50 000 | 254.84 | 0 | 110 | 9 |
| 500 000 | **125.39** | 0 | 16 | 9 |
| (`--nojit`) | 143.06 | 0 | 0 | 0 |

Read the last two rows together, because that is the finding:

**JIT-on-but-almost-nothing-compiled (125.39 s) beats BOTH full JIT (286.30 s)
and full interpretation (143.06 s).** The nine OSR compiles and sixteen C2s at
that threshold are the genuinely hot loops and they pay for themselves — 17.7 s
better than interpreting them. The ~215 further compiles the default admits
(240 publishes against 25) cost **160.9 s of CPU** between them. (`c1` and `c2`
both count a method that publishes twice, so read that as compiles, not as
distinct methods; the census is 316 publishes over 147 distinct methods.)

The 500 000 arm ran at `load1` 23.0, the highest of the four, and still won by
a factor of two. This is not a load artifact.

So compiling a method here is not free-and-sometimes-useful; it has a real
per-method cost that short, frequently-called methods never amortize. The
interpreter's own gate comment already says so, and this is its price tag:
raising the threshold "keeps short-lived / call-heavy code interpreted …
instead of paying CratonVM's currently-slower JIT'd dispatch for code that
never amortizes the switch"
(`vm/src/runtime/interpreter/dispatch_static.rs:1533`).

Why the accessor probe reports the opposite: it measures steady state *inside*
one long-lived OSR-compiled frame — five million iterations under a single JIT
entry — so it amortizes the per-invocation entry cost over five million
iterations and reports none of it. "The JIT is 10.8x faster" and "the JIT costs
+83% on this class" are not in contradiction; they are two regimes, and the
page that measured only the first drew a conclusion about the second.

## Not per-invocation JIT entry either

The obvious reading of the sweep is that each compiled method pays an
interpreter→JIT entry cost on every call that a short method never amortizes.
`CRATONVM_DBG=jit-scan-prof` now counts those entries (`push_entry_full`, the
one door every transfer of control into compiled code passes through):

| threshold | CPU | `jit_entries` | compiles |
|---:|---:|---:|---|
| 500 | 261.91 | 15 333 333 | c1=135 c2=95 osr=9 |
| 500 000 | 123.64 | 11 641 053 | c2=16 osr=9 |

Entries grow **24%** while CPU grows **112%**. Charging the whole 138 s delta to
the 3.7 M extra entries prices one entry at **37 µs**, which is not a dispatch
cost by three orders of magnitude. **Entry count does not track the cost.**

The same counter says where to look next, by what it cannot see: a frame entered
by a **direct JIT→JIT call pushes no entry guard**, so calls made *from* compiled
code never reach `push_entry_full`. Those go through `jit_invoke_virtual_mic` and
`jit_invoke_dispatch`, and each of those helpers does real per-call work before
it dispatches anything — a `note_jit_boundary` bump, a SATB flush, a thread
lookup, and `forward_jit_reference_args`, which **re-parses the callee's
descriptor string on every single call** (`vm/src/jit/helpers.rs:407`) rather
than reading a precomputed reference-argument mask off the call site.

That funnel scales with how many *calls compiled code makes*, not with how many
times compiled code is entered — which is exactly the quantity that grows when
you compile 215 more short, call-dense methods while entry count barely moves.
`CRATONVM_DBG_MIC_PROF=1` has rdtsc totals for both helpers
(`cyc_mic_total`, `cyc_disp_total`) plus the `hit_entry`/`hit_noentry` split that
says whether the inline cache ever learns a target or re-pays the compile probe
on every call.

## The funnel, and the inline cache that never publishes

`CRATONVM_DBG_MIC_PROF=1` on the two thresholds (this needed its own fix first —
see "levers" below — because the per-call trace on that switch wrote 268 MB of
stderr in 73 s and buried the counters):

| | thresh 500 | thresh 500 000 |
|---|---:|---:|
| `mic_calls` | 26 362 938 | 16 414 119 |
| `hit_entry` | 387 110 (1.5%) | 256 132 (1.6%) |
| `hit_noentry` | 5 359 501 | 3 995 148 |
| `miss` | 1 173 523 | 280 143 |
| `disp_calls` | **15 254 558** | **2 658 968** |
| `pub_probe_none` | 5 359 501 | 3 995 148 |
| `pub_barred` | 0 | 0 |
| **`pub_published`** | **0** | **0** |

Two things to read here, and one not to.

**`pub_published = 0`.** The megamorphic inline cache never publishes a callable
entry, in either configuration. `pub_probe_none` is *exactly* equal to
`hit_noentry`, so the uniform reason is that `try_jit_compile_callee` handed
back nothing — not `pub_barred` (0), not a downgrade inside
`jit_entry_publishable`. 93% of MIC hits therefore find an empty slot. This is
the pathology the source already anticipated in `mic_prof`'s own doc comment
("`hit_entry == 0` while `hit_noentry` climbs means the slot never learns a
target"); it is now measured on a real workload. **No call site out of compiled
code ever becomes a direct jump.** Every one keeps paying the helper.

**`disp_calls` 5.7x.** Compiling the ~215 additional methods multiplies calls
through the *generic* dispatcher — the slowest path — from 2.66 M to 15.25 M,
while `jit_entries` moved only 24%. That is the quantity that tracks the cost:
not how often compiled code is *entered*, but how many calls compiled code
*makes*, each one funnelled.

**What NOT to read: the cycle totals.** `cyc_mic_total` and `cyc_disp_total` are
`CycGuard` spans covering the whole helper *including execution of the callee*,
so they are inclusive and they nest — a JIT→JIT call inside a callee is counted
at both levels. At threshold 500 they sum to 2.9e12 cycles against a run of
~260 s CPU, which is impossible and is exactly what "inclusive and nested"
predicts. They are useful as ratios between arms, not as a share of run time.
`cyc_compile_probe` (4.35e9 vs 2.31e9 cycles, ~1.8 s at 2.4 GHz) is small — the
re-probing itself is not the bill.

So the direction of the fix is publication, not the threshold: make a warm call
site out of compiled code resolve to a direct entry once, instead of re-entering
`jit_invoke_virtual_mic` / `jit_invoke_dispatch` on every call forever. Until it
does, every method admitted to the JIT converts its call sites from interpreter
dispatch into a permanently-cold helper funnel, which is why admitting more of
them makes this class monotonically slower.

## What NOT to do

**Do not raise the default `CRATONVM_JIT_THRESHOLD` on this evidence.** One
class says the current value is wrong *for call-dense code*; it says nothing
about the compute-loop benchmarks the value was presumably tuned on, and a
global flip trades one workload for another silently. The lever is here so the
next person can price the trade on the gauntlet, not so it can be flipped.

The fix belongs in *admission*, not in the threshold: a method whose body is
mostly invokes and which carries no back-edge has little for the compiler to
win and pays the entry cost on every call. C2 entry here is currently
**structural rather than hotness-driven** — after every successful C1 publish
the worker unconditionally enqueues a C2 recompile
(`jit/src/tiered.rs:2067`), gated only by `c2_upgrade_would_engage`'s bytecode
scan (`jit/src/lib.rs:4994`) and not by any invocation count. That is also why
`CRATONVM_TIER_C2_THRESHOLD` is **inert** for the path that produces nearly all
of these compiles: raising it to 2e9 still produced `c2=97` against a baseline
`c2=95`.

## Levers, and which of them are inert

Recorded because three of the obvious ones do nothing, and each would have
produced a confident wrong answer read on CPU time alone.

| lever | works? | note |
|---|---|---|
| `CRATONVM_JIT_THRESHOLD` | **yes** | the sweep above |
| `CRATONVM_DBG=jit-scan-prof` | **yes** | added for this page; also counts `jit_entries` |
| `CRATONVM_DBG_MIC_PROF` | **yes, after a fix** | its counters are what named `pub_published=0`, but the per-call `[DISP_TRACE]` used to ride the same switch and wrote 268 MB of stderr in 73 s here (15.25 M `disp_calls`, one `eprintln` each). Split onto `CRATONVM_DBG_MIC_TRACE`; MIC_PROF is counters-only. Read the `mic_calls=` line, not the trailing `gc_collections=` one, and note `total_dispatches` on it is a separate `CRATONVM_DBG_LETSGO` counter that reads 0 unless that flag is set |
| `CRATONVM_NO_JIT_SCAN_CACHE` | yes, but uninformative | 266.21 vs 265.19; the cache never hits anyway (`cache_hits=0`), so "free" means "the cache was doing nothing", not "the scan is cheap" |
| `CRATONVM_TIER_C2_THRESHOLD` | **INERT** | C2 entry is structural; still `c2=97` at 2e9 |
| `CRATONVM_NO_PRECISE_JIT_MAPS` | **INERT** | `jit/src/x64.rs:1895` computes `precise_maps = precise_jit_maps_enabled() \|\| moving_young_enabled()` and moving-young is default-ON (`types/src/flags.rs:884`), so the opt-out cannot turn safepoint emission off |

## Reproducers

* `probes/JitEntryFloorProbe.java` — prices one interpreter→JIT crossing by
  pinning the CALLER to the interpreter with
  `CRATONVM_JIT_DENY=JitEntryFloorProbe.driver` while the callee still
  compiles. The deny list is the only lever that holds one side of a call at a
  chosen tier, and the probe prints which of its own methods compiled so a
  vacuous arm is visible rather than averaged in.
* `probes/ByteBufferScalarSplitProbe.java` — run it under `--nojit` too; that
  control is what refuted the dispatch-floor reading.

## Affected classes

Caught on `ZipContentTests`, and nothing about it is special: any class whose
time goes into many short calls from frames that never go hot — which is most
of a JUnit suite — is in the same regime.
