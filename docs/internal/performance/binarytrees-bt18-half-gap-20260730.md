# Binary Trees depth-18 second half-gap closure (2026-07-30)

Status: **REGRESSION — not fixed.** Controlled interleaved measurement (Azure
EPYC host, 10 rounds, `-Xmx8g`, `taskset -c 13`, host load < 6, checksum
verified every run) shows the candidate is **~12-13% SLOWER** than plain
`origin/dev`, not faster:

| arm | median ms | checksum | non-moving-GC fallbacks |
| --- | --- | --- | --- |
| baseline (`origin/dev` @ `fcc723007`) | 1805 | 68332206 | 1 |
| candidate (this branch) | 2033 | 68332206 | 6 |
| Temurin JDK 25 (reference) | 175 | 68332206 | n/a |

Root cause, isolated by disabling each candidate mechanism independently
(`CRATONVM_JIT_GC_INERT_SELFREC=0`, `CRATONVM_NO_JIT_TLAB_ZERO_ELISION=1`):
neither the GC-inert self-recursion optimization nor the TLAB zero-elision
change the fallback count or explain the slowdown — both are neutral-to-
slightly-positive in isolation. The **512 MiB young-semispace cap is the
actual cause**: shrinking the young semispace from `-Xmx8g`'s uncapped ~2 GiB
down to 512 MiB forces ~6x more young GC cycles for this workload, and *every
young GC that fires in this benchmark already falls back to the non-moving
sweep* on both baseline and candidate alike (confirmed by running baseline at
`-Xmx2g`, where its own uncapped semispace is also 512 MiB: baseline then
also shows 6 fallbacks, identical to the candidate at `-Xmx8g`). This
"moving young almost never actually moves for this workload" behavior is a
**pre-existing, unrelated defect**, not introduced by this branch — but the
512 MiB cap is what turns a benchmark that used to GC once (cheap) into one
that GCs 6 times, each one paying the (expensive, pre-existing) non-moving
fallback cost instead of a cheap copy. The RSS win is real and large (candidate
~807 MB vs baseline ~2.37 GB peak) but the wall-clock cost of 6x more
fallback-laden GC cycles outweighs the paging savings on this host.

The previous session's empirical cap sweep (256/384/512/.../2048 MiB,
"Fix" section below) compared cap sizes only against **each other**, not
against a clean, host-load-controlled measurement of true uncapped
`origin/dev` — so it never surfaced that every capped variant is slower than
no cap at all for this specific fallback-dominated workload. See
`## Path forward` at the bottom for options.

## Goal and acceptance

This follow-up targets the current `dev` regression in CratonBench
`CratonBench bintrees`, whose fixed workload is the depth-18 Binary Trees
case. The result must retain checksum `68332206` and reduce the excess over
Temurin JDK 25 by at least 50%:

```text
gap reduction =
  1 - (candidate median - HotSpot median)
        / (baseline median - HotSpot median)
```

`TODO(final acceptance table)`

## Root causes

Two independent costs dominated the current large-heap run:

1. `-Xmx8g` eagerly sized each young semispace to 2 GiB. Binary Trees touched
   hundreds of thousands of demand-zero pages before useful allocation work,
   producing about 2.3 GiB peak RSS and several seconds of kernel CPU on the
   Azure EPYC host.
2. The JIT redundantly zeroed every new object's body and GC header even
   though all TLAB refill backends already return zero-filled allocation
   ranges. The recursive `itemCheck` kernel also paid a general safepoint root
   shadow/spill sequence around its exact self-call despite having no
   allocation, arbitrary call, or backward-edge path.

## Fix

- Initial young-semispace sizing is capped at 512 MiB while retaining the
  existing `Xmx / 4` ceiling for later growth. Old-generation reservation
  continues to account for that maximum young capacity, so this changes
  startup commitment rather than the heap's long-run capacity contract.
- G1 TLAB refill explicitly zeroes reused allocation ranges, completing the
  invariant already provided by the generational and simple collectors.
- x64 object allocation relies on the refill invariant and elides redundant
  zero stores for the body, identity-hash, forwarding, and mark fields. Header
  class/kind/shape publication and cursor-last visibility are unchanged.
  `CRATONVM_NO_JIT_TLAB_ZERO_ELISION=1` restores the conservative stores.
- A deliberately narrow proof recognizes allocation-free exact self-recursive
  kernels. It rejects allocation, arbitrary calls, unresolved fields, and
  backward branches; only the loader-stable exact-self target that the call
  planner already proved may remain. Such a kernel omits the prologue poll and
  the hot self-call root shadow/spill. Its cold stack-overflow guard still
  spills conservatively and therefore forces the moving collector fallback if
  collection occurs there. `CRATONVM_JIT_GC_INERT_SELFREC=0` disables this
  optimization.

The 512 MiB initial cap was selected empirically. Prototypes at 256, 384, 768,
1,024, and 2,048 MiB were slower or less stable; the smaller settings paid
more non-moving young cycles, while the larger settings returned page-fault
cost toward the regression.

## Validation

- Every JIT benchmark execution produced checksum `68332206` on both baseline
  and candidate, across all isolation runs (10 + 6 + 6 + 3 rounds).
- `cargo test -p cratonvm-gc --lib -- --test-threads=1`: 873 passed, 0 failed
  (post-`origin/dev`-merge, commit `f76c706ea`).
- `cargo test -p cratonvm-jit --lib -- --test-threads=1` (Windows,
  post-merge): one crash, `x64::tests::cooperative_poll_runs_in_a_pure_compiled_method`
  (STATUS_ACCESS_VIOLATION) — **verified pre-existing on a clean, unmodified
  `origin/dev` checkout** (commit `4d8a39a39`), unrelated to this branch.
  Tracked separately, not a gate on this doc.
- The changed diff passes `git diff --check`.
- Gap-reduction formula does not apply — the candidate is currently a
  regression, not an improvement, so there is no acceptance table to fill in.

## Path forward

Options, not yet decided:

1. **Drop the 512 MiB young cap, keep zero-elision + GC-inert self-recursion.**
   Isolation testing showed these two are neutral-to-mildly-positive on their
   own (zero-elision ~2-3% faster than candidate-without-it, same fallback
   count). This would give a modest, low-risk win plus the RSS/paging
   reduction goes away (back to ~2.3 GiB peak at `-Xmx8g`).
2. **Root-cause why moving-young always falls back to non-moving for this
   workload** (`missing-exact-rbp` / `innermost-rbp-belongs-to-unguarded-callee`
   in `gc_quiescence`) and fix *that*, independent of any cap. If moving-young
   actually moved, more frequent (smaller) GCs would very plausibly become a
   net win again, since each one would be cheap. This is a materially bigger
   investigation, not a bt18-specific fix.
3. **Keep the 512 MiB cap for its RSS benefit, accept the wall-clock cost**
   — only sensible if peak memory matters more than wall time for this
   benchmark's role in the suite (unlikely, given CratonBench gates on time).
4. **Try intermediate/adaptive cap values** now that the true comparison
   point (uncapped baseline) is known — every value tested previously (256
   through 2048 MiB) was only ever compared against other capped values, so
   it's still open whether some cap beats true uncapped `origin/dev`; option
   1's numbers suggest not, since even 512 MiB (the "best" of the prototypes)
   loses to no cap at all.

The remaining allocation-publication / non-moving-sweep / template-JIT frame
cost noted in earlier drafts of this doc is real, but secondary to resolving
the regression above.
