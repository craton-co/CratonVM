# Moving-GC root ownership and dependency inversion

Fixed on `codex/complete-architecture-remediation-20260726`.

## Problem

Movable `ObjectRef`s stored in native Rust side tables were maintained by
separate call lists in `memory/roots.rs` and `memory/gc.rs`. Adding a scan
without the corresponding post-move rewrite (or vice versa) left either a
prematurely collected object or a stale from-space pointer. The collector also
depended directly on `cratonvm-native-collections` solely to discover overlay
owner edges, reversing the intended abstraction boundary.

## Resolution

- `vm::memory::native_roots` now owns a compile-time inventory whose entries
  contain a stable name and both scan/remap callbacks.
- All 22 built-in native/VM side-table sources route through that inventory.
  Lazy sources (instrumentation transformers and serialization caches) continue
  to use the paired, idempotent dynamic registry.
- The collection overlay contract is registered with
  `cratonvm_gc::external_roots` during collection-native initialization. The
  contract includes global scan, conditional owner discovery, per-owner edges,
  matching-owner edges, relocation, and liveness pruning.
- `cratonvm-gc` no longer depends on `cratonvm-native-collections`; the provider
  now depends on the collector abstraction.
- Registry tests cover provider idempotence, aggregation, relocation fan-out,
  and uniqueness of the built-in VM inventory. The existing 13-case overlay
  relocation harness continues to cover every overlay representation.

## Verification

- `cargo check -p cratonvm-vm --all-features`
- `cargo test -p cratonvm-gc external_roots --lib`
- `cargo test -p cratonvm-native-collections --test gc_relocation_harness`
- `cargo test -p cratonvm-vm memory::native_roots --lib`
- `cargo tree -p cratonvm-gc` contains no `cratonvm-native-collections`
- release binary:
  `/data/data/bin/cratonvm-complete-remediation-20260726-r4`
- `CRATONVM_GC=stress ...r4 --nojit -Xmx32m ... cratonvm.NativeRootRegistryGc`
  prints `NATIVE_ROOT_REGISTRY_GC_OK` after 240 rounds of collection-overlay,
  process-singleton, allocation-churn, and explicit-GC checks.

The `--nojit` probe mode is intentional: the independently tracked
`jit-interpreter-semantics-probe-mismatch-20260727.md` also changes TreeSet
cardinality on the default JIT path. The GC/root result is clean on the moving
interpreter path; the JIT mismatch is addressed in its own remediation slice.
