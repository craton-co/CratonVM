# Fixed: Binary Trees depth-18 gap versus HotSpot

Status: fixed (2026-07-14)

## Goal

Reduce the isolated `BenchSuite bintrees18` gap by at least half from the
published 12.9x snapshot (target: 6.45x or better), without changing the
HotSpot checksum `68332206`.

## Improvements implemented

- Enable the VM-scoped JIT allocation-class metadata cache by default, with
  `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE` as the opt-out.
- Enable pure inline TLAB `new` by default only for classes that require neither
  primitive-field initialization nor finalizer registration. Helper-requiring
  sites remain opt-in, and `CRATONVM_JIT_DISABLE_INLINE_NEW` remains available.
- Stop retaining a full per-object sweep-history vector unless the A2 forensic
  diagnostic is enabled. The old path generated more than a gigabyte of
  temporary metadata traffic during the roughly 19-million-object sweep.
- Coalesce adjacent dead objects while collecting verified sweep decisions,
  while retaining exact per-object records for sweep-zero, A2, and sweep-census
  diagnostics.
- Zero and publish maximal reclaimed spans rather than one free-list entry per
  object.
- Skip selective-promotion arena walks until an object can actually have
  reached the promotion age.
- Add the opt-in `CRATONVM_DBG_GCPHASE` phase timer used to attribute the pause.
- Inherit the cached JIT thread pointer and native-stack floor from the caller's
  same-method frame on direct self-recursion, avoiding two TLS/helper calls per
  recursive entry. `CRATONVM_JIT_NO_SELF_CACHE_INHERIT` restores the old path.
- Elide the full-GPR defensive spill at a direct self-call when precise maps
  prove every surviving value is already frame-resident. The old spill remains
  available with `CRATONVM_JIT_FULL_SELF_CALL_SPILL`.
- Treat an exact verifier-backed operand-stack oop as a trusted field receiver:
  retain the Java null/helper path, but skip redundant alignment and six-bound
  arena checks. Ambiguous merge states and standalone/unverified compile
  contexts retain the complete guard. This reduced generated `BenchSuite.check`
  from 1,235 to 932 bytes.

## Profiling evidence

On the local Windows host, `--Xmx 8g`, checksum correct in every run:

| Stage | Mark | Promotion | Sweep walk | Zero/publish | Total pause |
|---|---:|---:|---:|---:|---:|
| Initial release candidate | not split | not split | not split | not split | 8,104 ms |
| Phase-instrumented initial sweep | 174 ms | 829 ms | 6,226 ms | 1,403 ms | 9,025 ms |
| Bounded history + compact dead spans | 160 ms | 752 ms | 3,026 ms | 258 ms | 4,211 ms |
| Final release before age skip | 240 ms | 697 ms | 1,533 ms | 428 ms | 2,909 ms |
| Age-skip diagnostic build | 196 ms | 0 ms | 2,028 ms | 273 ms | 2,514 ms |

The phase timings vary with host load, but they consistently identify and
remove the original diagnostic-vector/free-list cliff.

## Validation

- `cargo test -p cratonvm-gc --lib`: 784 passed, 0 failed.
- `cargo test -p cratonvm-vm alloc_class_cache`: passed (no matching failures;
  unrelated integration targets were filtered).
- Final release checksums:
  - depth 10: `135854`
  - depth 14: `3222190`
  - depth 16: `14985902`
  - depth 18: `68332206`
- `cargo test -p cratonvm-jit --lib`: 903 passed, 0 failed.
- `cargo test -p cratonvm-vm --test jit_deep_recursion_fault_recovery`: 1 passed,
  0 failed.
- Release build: `cargo build --release --bin cratonvm` completed successfully.

## Acceptance result

Final release acceptance used five alternating fresh-process pairs, CPU 7,
`--Xmx 8g`, `CRATONVM_JIT_THRESHOLD=1`, and the exact checksum assertion.
No slow sample was discarded:

| VM | Reported depth-18 times (ms) | Median |
|---|---|---:|
| CratonVM | 23,661; 51,133; 14,495; 14,035; 14,581 | 14,581 |
| HotSpot | 1,343; 2,790; 2,329; 2,710; 2,751 | 2,710 |

The resulting median gap is **5.38x**, below the required **6.45x** and therefore
more than halves the published 12.9x gap. Every run returned `68332206`.

The host remained noisy, so these Windows numbers do not replace the published
Azure README row; they are the controlled same-host acceptance evidence for this
fix. A final phase-timed release run reported 5,212 ms total, including 1,299 ms
of GC work (117 ms mark, 978 ms sweep walk, 191 ms zero/publish), confirming that
the original eight-second sweep cliff is gone.
