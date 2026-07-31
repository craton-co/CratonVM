# The root-snapshot cache bypass stopped firing when real ForkJoinPool became the default — RESOLVED

| | |
|---|---|
| **Status** | **RESOLVED 2026-07-31.** The bypass is gone, not repaired: it guarded a hazard that cannot occur in the lane as it exists today, and the cache was measured against that hazard directly before removing it. A regression gate now fails the build if any presence predicate drifts the same way. |
| **Category** | GC-CORRECTNESS / THROUGHPUT (root snapshot caching, native re-entry) |
| **Filed** | 2026-07-30, verifying the deep-audit dispositions against the code |
| **Closed by** | `fix/rootsnap-cache-trigger-20260730` |

## What was wrong

`vm/src/runtime/env_cache.rs` exposed `real_forkjoinpool()`, built with
`cached_is_set!` — true iff `CRATONVM_REAL_FORKJOINPOOL` was *present* in the
environment. Two sites read it as "are we in the real ForkJoinPool lane?":

- `update_root_snapshot` used the frozen-frame cache only when
  `rootsnap_cache() && !conservative_locals && !real_forkjoinpool()`;
- `remap_rs_cache_after_gc` cleared the cache outright when `real_forkjoinpool()`.

That predicate was written when real ForkJoinPool was opt-in, so "the variable
is set" and "we are in the real lane" were the same statement. Real
ForkJoinPool then became the default (`flags().natives.real_forkjoinpool`,
opt out with `CRATONVM_SYNTHETIC_FORKJOINPOOL`), nobody sets the old variable
any more, and the predicate went silently false on exactly the configuration it
was written to catch. **The bypass had not fired on a default run since the
flip.**

The original filing left two coherent positions and no evidence:

1. the hazard is now universal, so the cache is always unsound and must go;
2. the hazard was specific to the opt-in lane, so the bypass needs a narrower,
   dynamic trigger.

## What settled it

### 1. A direct check for the failure, instead of a proxy for it

The bypass claims the cache can lose a root: "frames that look prefix-stable to
the cache can still expose changing local/operand roots around those native
returns". That is a checkable statement, so it was checked rather than inferred
from corruption rates.

`CRATONVM_DBG_ROOTSNAP_VERIFY=1` (new, default-inert, in
`update_root_snapshot`) re-scans **every** frame the uncached way after each
cached snapshot and reports any root the fresh scan has that the cached
snapshot lacks. A lost root is reported at the moment it is lost, not several
GCs later as a corrupted header. `CRATONVM_DBG_ROOTSNAP` was extended with
cache-engagement counters (`cached_calls` / `reused_frames` / `reused_roots`)
so a clean result cannot be a run where the cache never engaged, and
`CRATONVM_DBG_ROOTSNAP_EVERY=N` sets the tally period (these workloads publish
far fewer than the old 200k-call default).

Result, all in the real lane (the default), binary
`cratonvm-rsverify3-20260730`:

| workload | GC stress | cached snapshots verified | frames / roots reused | lost roots |
|---|---|---|---|---|
| `Fork6` (200 reps) | `CRATONVM_DBG_GC_STRESS=262144` | 25,600 | 160,941 / 264,341 | **0** |
| `Fork6Hard 128 20` | 262144 | 5,000 | 49,746 / 77,028 | **0** |
| `Fork6Hard 64 5` | 65536 | 600 | 4,517 / 7,007 | **0** |
| `Fork6Hard 128 40` | none | <200 (no GC pressure, few publishes) | — | **0** |

(Tallies print every `CRATONVM_DBG_ROOTSNAP_EVERY` snapshots, so each count is
the last tally, not the run total; misses are printed the moment they happen, so
the zero column is exact. `Fork6` under stress was run twice with identical
results.)

Two broader checks, on the post-fix binary `cratonvm-rsfinal-20260731`:

* `Fork6Hard 64 10` under 262144, run once as the default and once with
  `CRATONVM_REAL_FORKJOINPOOL=1`: byte-identical tallies (1,000 verified
  snapshots, 7,754 frames / 12,058 roots reused, 0 misses, `ALL-OK`) — the two
  lanes are now the same code path, which is the whole of the behaviour change.
* A real JUnit 5 workload —
  `org.hibernate.orm.test.mapping.converted.converter.YearMonthConverterTest`
  under the Hibernate harness, with and without GC stress: 3/3 green, 0 misses.
  It publishes far fewer snapshots than the FJP repros, so it is corroboration,
  not the main evidence.

These are the canonical repros for the bug family the bypass came from
(`docs/internal/fixed-suite-bugs/fork6-fjp-multithread-jit-root-reclamation-FIXED.md`).
The cache is demonstrably engaged throughout: every snapshot in these runs took
the cached path (`calls == cached_calls`), reusing 6–10 frames and 10–15 roots
per snapshot rather than re-scanning them.

### 2. Why it is clean — the hazard has no thread to happen on

The real-FJP lane keeps a Bridge native surface for
`ForkJoinPool.submit`/`invoke`/`execute` and `ForkJoinTask.fork`/`join`/`get`
(`native-api/src/registry.rs`, `native-builtins/src/phases_early.rs`), and
those natives run every task **inline on the submitting thread**. Measured with
a probe that records `Thread.currentThread().getName()` inside `compute()`:

```
HotSpot 25:        COMPUTE_THREADS=[main, ForkJoinPool.commonPool-worker-1 … -15]  PARALLELISM=15
CratonVM default:  COMPUTE_THREADS=[main]                                          PARALLELISM=1
```

The hazard the bypass described — a pool worker holding a forked subtask in its
own frames across a peer-triggered collection — requires a pool worker. This
lane has none. That is the mechanism behind the empty miss column, and it makes
position 2's premise right in a stronger sense than it was stated: the hazard
was not merely "specific to the opt-in lane", it is specific to worker threads
that this lane does not create.

### 3. The A/B/C stress matrix agrees

Interleaved, 8 reps per arm, `Fork6Hard 64 10` under
`CRATONVM_DBG_GC_STRESS=262144`, arms: **A** default (cache live, bypass inert),
**B** `CRATONVM_REAL_FORKJOINPOOL=1` (bypass fires), **C**
`CRATONVM_ROOTSNAP_CACHE=0` (cache off everywhere).

* pre-fix binary: 24/24 `ALL-OK`, 0 corruption signatures, 0 panics, 0 timeouts
* post-fix binary: 24/24 the same, with A and B now behaviourally identical

("Corruption signature" counts `class_id=ClassId(0)` out-of-bounds field reads,
`Stale pointer detected`, `RECLAIMED-LIVE` and `implausible object size` — the
shapes this bug family produces when a root is dropped.)

`cargo test -p cratonvm-vm --lib`: 2306 passed, 0 failed.

## What changed

* `vm/src/runtime/env_cache.rs` — `cached_is_set!(real_forkjoinpool, …)` and its
  `KNOWN GAP` note are **deleted**. The surviving guard on the cache is
  `roots::conservative_locals_enabled()`, which reads the *resolved* flag and is
  additionally gated on GC quiescence, i.e. it answers "is the hardening running
  right now", not "was the lane requested".
* `vm/src/runtime/interpreter.rs` — both bypass terms removed
  (`update_root_snapshot`, `remap_rs_cache_after_gc`), with the reasoning and
  the re-check instruction (if real FJP ever gains task-executing workers, run
  the verifier against that lane) recorded at the site.
* `vm/src/runtime/interpreter.rs` — `CRATONVM_DBG_ROOTSNAP_VERIFY` and the
  cache-engagement counters are kept as maintained diagnostics, not scaffolding.
* `vm/src/runtime/env_cache.rs` — new test
  `no_presence_predicate_shadows_a_compound_flag_default` (below).

Behaviour on a default run is unchanged: the removed term was already always
false. It changes behaviour only for a run that explicitly sets
`CRATONVM_REAL_FORKJOINPOOL=1`, which now gets the same cache the default gets —
arm B of the matrix above.

## Why not the other resolutions

* **Repoint the predicate at `flags().natives.real_forkjoinpool`** ("make it
  honest"): the flag is default-true, so the bypass would fire always, the
  frozen-frame cache would be dead on every run, and
  `root_snapshot_cache_tests::local_write_invalidates_cached_deep_frame_roots`
  fails (0 cached roots where it expects 2). Measured before this work, and the
  reason the original filing stopped there.
* **Retire the cache (position 1)**: it exists for the deep-stack native-heavy
  workloads that hung without it (SpringRepositoriesExtension, Groovy compile at
  depth ~46, 95s→42s), and the verifier found nothing wrong with it.
* **Write a narrower dynamic trigger (position 2)**: a trigger needs a window to
  fire in. Every candidate — "an FJP Bridge native is re-entering Java on this
  thread", "a task is in flight" — is *true for most of the run* in an
  inline-execution lane, so it would kill the cache exactly where JUnit's
  ForkJoinPool test executor submits the whole test tree through one `submit()`
  — the deep-stack shape the cache was introduced for. A guard that costs the
  fix it was built alongside, for a hazard with no thread to occur on, is worse
  than no guard.

## The regression gate

The filing's closing observation was that "there is no gate for *a predicate
quietly changed meaning*". There is one now:
`runtime::env_cache::tests::no_presence_predicate_shadows_a_compound_flag_default`
scans this crate's own source for `cached_is_set!` / `cached_is_ok!` predicates,
scans `types/src/flags.rs` for how each of those variables is resolved, and
fails if any of them has a **compound** default (`||`, `&&`, or a negated
presence test) — i.e. a default that can be true with the variable unset, which
is exactly when a presence test stops meaning "active".

Verified to catch the original defect: re-adding
`cached_is_set!(real_forkjoinpool, "CRATONVM_REAL_FORKJOINPOOL")` fails the test
with

```
these env vars have a compound (non-presence) default in flags.rs but are still
answered by a presence predicate in env_cache.rs, so the predicate now means
"explicitly requested" and not "active":
[("CRATONVM_REAL_FORKJOINPOOL", "real_forkjoinpool: !present(src,
\"CRATONVM_SYNTHETIC_FORKJOINPOOL\") || present(src,
\"CRATONVM_REAL_FORKJOINPOOL\")")]
```

## The sweep the lesson asked for

Every presence predicate was audited against the resolved flags, as the filing
recommended.

* **`cached_is_set!` / `cached_is_ok!` (51 predicates, all in `env_cache.rs`)** —
  45 are `CRATONVM_DBG_*` / `*_TRACE` diagnostics with no resolved flag or with a
  bare `present()` default, where "set" and "active" are the same question by
  construction. Eight also appear in `flags.rs`; seven of those resolve as bare
  `present(src, …)`. `CRATONVM_REAL_FORKJOINPOOL` was the only compound one — the
  bug in this doc. The gate above now enforces that this stays true.
* **`runtime_var_os(…).is_some()` (≈209 sites)** — the great majority are
  `CRATONVM_NO_*` / `CRATONVM_DISABLE_*` opt-outs, where a presence test is the
  correct and complete semantics. Ten name variables that also exist in
  `flags.rs`; eight are `DBG` diagnostics.
* **`CRATONVM_SHADOW_STACK` in `jit_scan_cache_enabled()`
  (`vm/src/jit/conservative_roots.rs`) — same shape, not a defect.** The JIT-scan
  cache disables itself when `CRATONVM_SHADOW_STACK` or
  `CRATONVM_DBG_FORCE_MOVING` is *present*, and the module comment justifies that
  as address stability ("the two diagnostic modes that lift the non-moving
  guarantee disable the cache"). But `shadow_stack_enabled()` is
  `present(CRATONVM_SHADOW_STACK) || moving_young_enabled()`, and moving-young is
  now the default — so, exactly as in this doc, the presence test no longer
  tracks whether the shadow stack is active. It is **not** a live bug: the cache
  is additionally keyed on `heap.collection_count()` (minor + major), so any
  collection that could relocate a cached address invalidates the whole entry
  first. The two presence tests survive as explicit-request kill switches, and
  the soundness argument rests on the generation key, not on them. Left as-is
  deliberately; recorded here so the next reader of that comment is not misled.

## Reproducing any of this

```bash
# does the cache lose roots in the real-ForkJoinPool lane?
CRATONVM_DBG_GC_STRESS=262144 CRATONVM_DBG_ROOTSNAP=1 \
CRATONVM_DBG_ROOTSNAP_VERIFY=1 CRATONVM_DBG_ROOTSNAP_EVERY=200 \
  cratonvm --java-home <jdk25> -c <repro-dir> Fork6
# [ROOTSNAP-VERIFY] verified_snapshots=N miss_snapshots=0 missed_roots=0
# [ROOTSNAP] calls=N … cached_calls=N reused_frames=… reused_roots=…
# any [ROOTSNAP-MISS] line is a root the cached snapshot dropped.
```

Repros: `docs/internal/fixed-suite-bugs/repros/A4-fork6/Fork6.java` and
`Fork6Hard.java` (the latter restored to that directory by this work — it was
referenced by the FIXED doc but had been lost in a docs move).
