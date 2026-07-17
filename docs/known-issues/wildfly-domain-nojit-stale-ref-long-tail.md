# WildFly domain no-JIT boot: narrow stale-ref long tail — tree-key / compute-value producers (canary-only)

Status: OPEN — narrow residual, 2026-07-17. This doc replaces
`wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`,
whose five root-cause families were all closed by
`fix/wildfly-cce0079-residuals3-20260717` (full mechanism writeup:
`docs/internal/fixed-suite-bugs/wildfly-cce-residuals-blocked-region-and-pin-waves-FIXED.md`;
the retired parent doc's full history:
`docs/internal/fixed-suite-bugs/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root-RETIRED.md`).

## What remains

After the census-leak fix, exact slot-origin write-back, ~85-site pin waves,
the pin-truncate-misuse fixes, and the ABI-tear SIGSEGV fix:

- **Standalone (JIT and no-JIT)**: fully clean — 0 CCE / 0 SIGSEGV / 0 canary
  firings across every post-fix batch (see the FIXED doc's matrix).
- **Domain no-JIT**: plain-boot CCE dropped from 4/4 to ~1-in-several with
  frequent full `OK_BOTH_SERVERS` boots, but the
  `CRATONVM_DBG_STALE_OBJREF` canary still fires on most domain boots, at
  two remaining reader signatures:
  1. `tree_compare → comparator_compare → natural_compare → read_string` —
     a TreeMap comparison reading a stale String KEY out of a tree node
     (the key was stored stale by an unidentified upstream producer, or the
     node graph is island-stale).
  2. `native_chm_compute_if_present → native_map_put → set_field` — the
     store canary firing on a value that arrived already-stale through the
     (fully pin-disciplined) compute chain — i.e. the producer is upstream
     of the BiFunction result.

These are 1-2 remaining producers; every capture in the parent
investigation ultimately reduced to a concrete unpinned span or a
pin-stack truncate misuse, and there is no reason to believe these differ.

## How to pick this up

Worktree/probes: `/data/data/wt-cceres3-20260717/probes/` (Azure host) —
`run-domain.sh` / `batch.sh`, frozen binaries `cratonvm-cceres3-d1..d17`,
all logs + `summary.txt`. Repro: domain batch with
`CRATONVM_DISABLE_JIT=1 CRATONVM_DBG_STALE_OBJREF=1
CRATONVM_DBG_STALE_OBJREF_CYCLES=8 CRATONVM_DBG_BLOCKGC=1 RUST_BACKTRACE=1`
(~most boots fire within 420 s).

Diagnostics landed for exactly this hunt (all default-off):
- `CRATONVM_DBG_BLOCKGC` — frame verifiers (DEPOSIT/WAKE/ARRIVE/SAFEPOINT/
  INITIATOR-STALE), PIN-STALE (stale at pin time), **PIN-DANGLING** (handle
  past the pin stack ⇒ someone truncated caller pins), PIN-UNDERFLOW
  (native returned with fewer pins than entry), writeback/heal traces.
- `CRATONVM_DBG_UNPIN_RING` — truncation-provenance ring (base/prev_len +
  backtrace per truncate), dumped by the PIN-DANGLING report; this named
  the last two culprits in one probe each.
- Store canaries in the `set_field`/`set_array_element` funnels + the
  always-on return-value healing barrier (names stale-returning natives).

Start with the tree-key reader: decode which TreeMap (management-model
attribute registry) via `CRATONVM_DBG_CCE_BT`-style receiver identity at
the panic, then audit the producers that populate that tree (`tm_put`
insert/rebalance paths, and whatever BiFunction results flow into it via
`compute*`). The pin rule everything else fell to: the pin stack is
strictly LIFO per scope — one truncate from the scope's first pin; never
unpin per-iteration pins pinned before later ones; callbacks must never
pin into a callee's scope (use JNI global roots for accumulate-under-
callback, cf. `stream_apply_chain_full`).
