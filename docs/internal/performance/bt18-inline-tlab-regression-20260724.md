# bt18 4x regression: inline TLAB `new` demotion (2026-07-24)

Status: fixed (this branch).

## Symptom

Binary Trees depth-18 (`bench/BinTreesClassic.java`, `-Xmx8g`, pinned,
Azure EPYC bench host) regressed from the documented 1,468 ms median
(`binarytrees-half-gap-20260718.md`) to ~5,600–8,700 ms on `dev`, with the
young collector running **two** full non-moving cycles instead of one and
`CRATONVM_NO_JIT_INLINE_PUTFIELD=1` showing **zero** delta (the inline
fresh-ctor stores were no longer being emitted at all).

## Root cause

Commit `1ee92e3fd` ("close IVFKnn stale-precise-root-mirror + young-GC
exact-walk hardening") demoted the inline TLAB `new` fast path to opt-in
(`CRATONVM_ENABLE_UNSAFE_INLINE_TLAB_NEW`), routing every `new` through the
`new_object` helper, because the then-current emission committed the TLAB
cursor **before** writing the object header — a thread suspended (or
walked) between the two stores exposed a committed-but-unheadered object
("malformed young-space spans" in the ES concurrent-merge investigation).

The demotion re-helperized the hottest allocation path, and both
`binarytrees-half-gap-20260718` optimizations sat on top of it:

1. the JIT-refill 90%-occupancy single-cycle trigger interplay, and
2. the inline constructor bodies' compact reference stores.

Deterministic attribution: young mark-cycle count flips 1→2 exactly at
`1ee92e3fd` vs its parent (`CRATONVM_DBG_GCPHASE=1`); on the unmodified
regressed binary, `CRATONVM_ENABLE_UNSAFE_INLINE_TLAB_NEW=1` alone restores
1,527–1,533 ms / single cycle / exact checksum `68332206`.

Wall-clock bisecting on the shared host was misleading (converged on a
docs-only commit under load noise); the cycle-count probe is the method of
record for this class of regression.

## Why default-on is safe now

A post-demotion hardening pass rewrote the emission to be
suspension/walker-safe: the **full header is written before the
cursor-commit store**, which is the single linearization point (x86-64 TSO
never reorders a store ahead of older stores), so no walker — concurrent or
post-suspension — can observe a committed-but-unheadered object. A thread
frozen mid-sequence leaves the half-built object *beyond* the published
cursor, i.e. invisible.

This branch closes the remaining gap: header fields that still relied on
the "TLAB refill zeroes the region" assumption (empirically violated once
before — the offset-4/12 BinTrees-18/ECJ incident) are now written
explicitly per allocation: identity_hash (8), gc_age/gc_flags dword (20,
legacy arm), forwarding_ptr (24–31), mark_word (32–39). Nothing in the
inline path depends on refill zeroing.

Default flipped ON; kill switch `CRATONVM_NO_JIT_INLINE_TLAB_NEW=1`; the
legacy opt-in variable remains accepted (redundant).

## Acceptance

- `regression-suite/perf/run-cratonbench-gate.sh` bintrees anchor
  (1,468 ms +5%) with checksum `68332206` on the Azure bench host.
- Single young cycle under `CRATONVM_DBG_GCPHASE=1`.
- `CRATONVM_NO_JIT_INLINE_PUTFIELD=1` regains a measurable delta
  (inline ctor stores emitted again).
- jit/gc/vm unit suites green; the IVFKnn/ES walker protections from
  `1ee92e3fd` (frame-metadata registration, exact-walk lockstep checks)
  are untouched by this change — only the allocation emission and its
  default changed.
