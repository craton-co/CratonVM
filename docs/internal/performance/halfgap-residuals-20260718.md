# Half-gap residuals round (2026-07-18)

Follow-up to `halfgap-20260717.md`: drive every residual from that round's
"Residuals / next levers" list to closure. Branch
`perf/halfgap-residuals-20260718` off dev `7a939ec01b`, Azure benchmark host,
worktree `/data/wt-halfgap-resid-20260718`.

## Final numbers (2026-07-18, medians of 5 alternating pairs, CPU 13)

Host load ~7-10 during the sweep (unchanged rows moved within the ±10-15%
contention band; see README note). Binary Trees row from the same-day
quiet-window triple.

| Row | JDK 25 C2 | CratonVM | ratio | round start | change |
|---|---|---|---|---|---|
| Arithmetic 2B | 2,006 | 4,895 | **2.44x** | 3.54x | 64-bit const-div fusion |
| Fibonacci 44 | 1,719 | 4,790 | 2.79x | 2.95x | (noise; no code change) |
| Sieve 100Kx20K | 2,851 | 6,508 | 2.28x | 2.55x | (noise/JDK bimodal) |
| Matrix 1280² | 2,349 | 6,875 | 2.93x | 2.69x | (noise; no code change) |
| HashMap 1M | 45 | 201 | **4.47x** | 5.16x | boxing-helper raw store |
| String/Regex 1M | 147 | 1,561 | **10.6x** | 15.6x | SB native cache + early ldc |
| BinTrees d18 -Xmx8g | 188 | 10,650 | **56.6x** | 58.1x | triggers default ON, stable |

Default-heap Binary Trees no longer wedges: it now runs like `-Xmx8g`
(~12-14 s under load) instead of the 21-37 s treadmill.

## What landed (commit order)

1. **`a3dc8a15ad` dev-tip Linux build fix** — e13232a9a6 added
   `StartConnect::DeferredFailure` without covering it in `sc_connect_bound`
   (E0004, workspace didn't build). Dev independently landed its own arms in
   parallel; the merge follow-up `2b68905da3` defers to dev's version.

2. **`e3f79f9737` young-walk corruption ROOT CAUSE + fix; triggers default
   ON.** The trigger-ON bt18 corruption (5/5 wrong checksums, "cursor
   overshot into free block", GAP-sentinel/half-zeroed-header hexes) was
   never a sweep bug: the ergonomics-derived young capacity was
   **1 GiB − 4 — not 8-aligned**. Near-full, `refill_tlab`'s
   `requested.min(available)` minted unaligned TLAB sizes (32764, 9772 —
   caught by free-list alignment tripwires + a backtrace at the birth site);
   the free-list split remnant then sat at a +4 offset (off the 8-byte object
   grid every walk assumes), and `Tlab::new`'s release round-down left an
   untracked zeroed sliver between the tail filler and the next region. The
   mark oracle amplified any grid break by silently dropping every
   conservative root above its truncation point (the 676xxxxx under-count
   family). Fix layers: `Arena::new`/`grow` round capacity down;
   `Arena::alloc` rounds every size **up** (grid invariant for all callers,
   with permanent bounded tripwire warns); `refill_tlab` rounds
   `actual_size`/`take` down; the oracle records its trusted frontier and
   falls back to direct plausibility-checked candidate validation above it.
   `CRATONVM_TLAB_GC_TRIGGER` now defaults ON (opt out `=0`);
   `docs/known-issues/tlab-trigger-gc-young-walk-corruption.md` retired to
   `docs/internal/tlab-trigger-gc-young-walk-corruption-FIXED.md`.
   Validation: bt-default 5/5 checksum 68332206 with zero walk warnings
   (was 5/5 corrupt), bt-8g 10.64 s ×3 stable, StreamOnlyStressRepro
   -Xmx32m ×3 + -Xmx512m OK, gc lib 791/791.

3. **`adfb020d28` ldc-String wired in the early-compile path** (the second
   unwired site from last round). It had been inserting every
   string-constant-bearing method into `jit_skip_set` — which also blocks the
   hot-path `jit::try_compile`, leaving OSR artifacts as those methods' only
   compiled form. Wide-string/Class ldc keep the old skip behavior.

4. **`b443a065c1` StringBuilder object-native kind** — `append(I)/(C)/
   (String)`, `toString`, `length` join the exact-receiver native cache
   (registry's 3-string hash paid once per callsite, not per call).
   `append((String)null)` bails to the generic path for the `"null"`
   semantics. StringRegex 2,140 → 1,564 ms. Byte-exact SB/StringBuffer
   parity probe vs HotSpot.

5. **`9d9311f74d` boxing fast-path raw store** — `jit_integer_value_of_direct`
   wrote the wrapper's value via layout-dispatching `set_field_as` on an
   object it had just laid out itself; the TLAB arm now writes the 16-byte
   Value cell directly. HashMap 221 → ~194 ms.

6. **`113232887a` long const-arith fusion** — the int const peephole had no
   cat-2 sibling, so `i*3 - i/2 + i%7` over longs paid guarded CQO+IDIV per
   op. `ldc2_w K; l{mul,div,rem,add,sub}` now fuses: pow2 → SAR with sign
   fix; non-pow2 → 64-bit signed magic (Hacker's Delight 10-4 in u128,
   mulhi via one-operand IMUL, `+n` when the magic wraps negative,
   `+(n>>>63)` after the shift). Unit test vs exact i128 division (14
   divisors × 2000+ dividends incl. i64::MIN/MAX); JIT-fused Java probe
   over mixed-sign longs matches HotSpot exactly. Arithmetic
   6,488 → ~4,900 ms.

7. **`7e47b948bb` BCE step-provenance guards + sound inclusive elision
   (opt-in).** Two things:
   * **Latent soundness hole (fixed, default-on):** `find_induction_variable`
     admits `iadd;istore` IVs (Sieve's `j += i`) without naming the step
     operand, and both BCE paths elided checks with no step-sign/overflow
     proof — a runtime-negative step walks the elided index below the array
     base. `find_iv_step_provenance` now proves the canonical shapes and
     names the step local; variable-stride loops get preheader guards
     `step >= 0 && step <= Integer.MAX_VALUE - bound`; unprovable shapes
     refuse elision; the guard-less static path requires the unit step.
     Probe: `j += step` with `step = -1` deopts and throws AIOOBE exactly
     like HotSpot.
   * **Inclusive (`<=`) loops (SECURITY FIX V17), sound-guard form:** guard
     `array.length > bound` (JBE) + `bound != Integer.MAX_VALUE` instead of
     refusing the loop. Probe-verified at the exact boundary
     (`length == bound+1` elides; `length == bound` deopts → AIOOBE).
     **Ships opt-in** (`CRATONVM_JIT_INCLUSIVE_BCE=1`): on the memory-homed
     template bodies the elision measured a ~2x NET LOSS on the Sieve OSR
     artifact — the removed per-element check is a predicted-never-taken
     branch + cache-hit length load (~free), while shrinking every unrolled
     body reshuffles code layout this frontend-sensitive kernel depends on.
     Re-evaluate when loop bodies get register homes.

8. **`1b7caa01b4` chore** — removed six stray `*.remote.rs` scp-staging
   copies (5.5 MB) accidentally committed at the repo root by cacb642162.

## Fibonacci: no session-scale lever remains

The "self-call dispatch" residual was already closed by earlier work:
disassembly shows fib's body IS the optimized-tier product with a guarded
DIRECT self-recursive call (IR `invoke_kind 4`, default-on) — no dispatch
helper on the recursion. The remaining ~2.8x is register allocation (every
IR value round-trips through a frame slot) and recursion inlining, neither
of which the backends do. That is the next structural lever for ALL kernel
rows (it is also why kernel-reg-homes beat BCE elision on Sieve — and why
those homes currently exclude guard-bearing bodies: a guard's deopt
reconstructs from frame-stashed state).

## Residuals / next levers

- **Register allocation for loop bodies / IR** — the single lever behind
  Fibonacci, Sieve, Matrix and the frame-slot traffic visible in every
  disassembly. Includes making speculative-guard deopts spill-compatible so
  kernel homes and BCE can coexist.
- HashMap: remaining per-op fixed costs are architectural (JIT-boundary
  bookkeeping, provenance bitmap, safe-native-call wrapper on the fallback);
  next real step is inlining the box+put/get composition in codegen.
- Binary Trees vs C2 (~56x): allocation-rate structural gap (per-object
  helper path vs C2's inline TLAB + escape analysis); the GC no longer
  wedges, so further cuts come from allocation codegen, not collection.
- `CRATONVM_JIT_INCLUSIVE_BCE` re-evaluation once register homes land.
