# Architecture audit: measured closeout

Date: 2026-07-26
Base: `dev` at `3be41785e`
Host: Azure x86-64, 16 logical CPUs, JDK 25, Rust 1.96
Scope: the default real-JDK build

This is an incremental closeout over the other documents in this directory.
It re-read the integrated tree rather than treating earlier architecture notes
as current, built a fresh release binary, and added small paired probes whose
checksums must agree between CratonVM and HotSpot. It is an architecture review,
not a claim that the microbenchmarks predict application throughput.

## Executive assessment

CratonVM has unusually broad Java compatibility for a young VM, and several
recent improvements are real: O(1) quickened-bytecode lookup, typed flag
grouping, wider IR admission, tiered inlining, PICs in the IR call path, and
the first decomposition of `SharedVm`. The main performance limit is no longer
one missing peephole optimization. It is duplicated machinery and unclear
ownership:

1. two interpreter semantic paths selected partly by a class-name prefix;
2. two JIT backends with different lowering coverage;
3. raw, copyable object addresses crossing native/static boundaries while a
   moving collector relies on every owner manually implementing scan and
   rewrite hooks;
4. heap fields occupying a 16-byte `Value` slot even for 32-bit primitives;
5. a 280-method `NativeContext` trait and a 522,726-line native-builtins crate;
6. hundreds of environment reads and defaults distributed through hot and
   cold code;
7. lower layers importing higher-level implementations; and
8. a bootstrap function and production files large enough that invariants are
   difficult to see or test in isolation.

The order matters. Enabling moving GC or raising JIT thresholds before fixing
root ownership and backend convergence expands the correctness surface. The
highest-return sequence is: make class lookup loader-faithful, collapse the
interpreter, establish a central root/handle contract, converge JIT lowering,
then change object layout and collector defaults.

## Measurements

### Repository shape

The selected production source sets contain about 1.17 million Rust lines:

| crate | lines |
|---|---:|
| `vm` | 290,078 |
| `native-builtins` | 522,726 |
| `jit` | 94,214 |
| `gc` | 61,599 |
| `classloading` | 52,430 |
| `native-io` | 51,136 |
| `native-collections` | 49,415 |
| remaining selected core crates | 52,560 |

The largest production files include `interpreter.rs` (47,536 lines),
`x64.rs` (41,912), `native-builtins/src/lib.rs` (38,791), and
`vm_exec.rs` (21,824). `vm/src/vm.rs` is 74,218 lines but almost all of that
file is a feature-gated test module, so treating it as the production
orchestrator is misleading.

The clean release build took 4m21s after Cargo re-extracted a damaged cached
copy of `cc-1.2.58`. The unique binary is 147 MiB on disk; `size` reports
29,268,397 bytes of text, 643,024 bytes of data, and 2,493,096 bytes of BSS.
The cache damage was a host artifact and is not attributed to CratonVM.

### Concrete layout

`types/examples/cratonvm_architecture_layout_probe_20260726.rs` reports:

| item | bytes |
|---|---:|
| pointer / `ObjectRef` / `Option<ObjectRef>` | 8 / 8 / 8 |
| public `Value` | 16 |
| `CompactValue` / `RawSlot` | 8 / 8 |
| `ObjectHeader` | 32 |
| instance field slot | 16 |
| reference field / array element | 8 / 8 |
| identity-hash / forwarding / mark-word offsets | 8 / 16 / 24 |

Consequently, an object with eight `int` fields occupies 160 bytes before any
allocator overhead: 32 bytes of header plus eight 16-byte slots. A representative
compressed-oop HotSpot layout is about 48 bytes. Folding only the forwarding
pointer into the mark word changes 160 to 152 bytes; packed, class-specific
field offsets are the larger prize. A 24-byte header plus eight four-byte
fields would be 56 bytes before allocator overhead.

### Runtime probes

The runner pins one CPU, starts a fresh process for every measurement,
alternates VM order, uses three repetitions, and compares deterministic
checksums. Interpreter comparisons use `--nojit` and JDK 25 `-Xint`.

All eight deterministic cases produced exactly one checksum across both VMs
and all repetitions. Median elapsed time:

| probe | iterations | JDK 25 ns | CratonVM ns | CratonVM / JDK |
|---|---:|---:|---:|---:|
| interpreter, default package | 2,000,000 | 43,720,457 | 574,775,366 | 13.15x |
| interpreter, `org.springframework.*` | 2,000,000 | 46,211,748 | 1,534,507,357 | 33.21x |
| interface dispatch, 1 receiver type | 10,000,000 | 8,860,009 | 10,631,104,772 | 1,199.90x |
| interface dispatch, 4 receiver types | 10,000,000 | 23,999,520 | 10,942,073,691 | 455.93x |
| interface dispatch, 16 receiver types | 10,000,000 | 22,821,076 | 11,104,084,772 | 486.57x |
| retained eight-`int` object allocation | 500,000 | 7,421,429 | 1,470,188,713 | 198.10x |
| preallocated exception throw/catch | 250,000 | 6,412,188 | 58,050,177 | 9.05x |
| uncontended monitor | 2,000,000 | 20,088,787 | 468,391,685 | 23.32x |
| `System.nanoTime` native boundary | 500,000 | 11,401,026 | 66,774,202 | 5.86x |

The two interpreted HotSpot rows differ by only 1.06x. CratonVM's
package-selected slow path is 2.67x slower than its own fast path for identical
source and checksum. The compiled dispatch rows are all near 10.6–11.1 seconds
regardless of receiver diversity, which is evidence that this containing
kernel did not realize a useful optimized/PIC path; it is not evidence that one
PIC hit intrinsically costs a microsecond.

Interpret the ratios as diagnostic signals, not a leaderboard. In particular,
the dispatch cases measure whether an entire kernel reaches and benefits from
compiled code; they do not isolate the nanosecond cost of one PIC hit.

## Findings and changes recommended

### P0: remove loader-blind class lookup from production paths — fixed

The audited release build emitted 47 deprecation warnings for
`ClassManager::find_class_by_name`, spread through interpreter, VM execution,
utility, reflection, native, and invocation paths. The API explicitly says it
is loader-blind. This is correctness debt in a VM supporting multiple class
loaders, and it also prevents negative/positive lookup caches from having an
unambiguous key.

Resolved on `codex/complete-architecture-remediation-20260726`: all 47 calls
now take an initiating/defining class or loader, require bootstrap identity, or
fail closed when a legacy metadata path genuinely has no context. The VM crate
denies deprecated APIs, the all-feature check passes, and the release warning
count is zero. Verification and rationale are in
`docs/internal/loader-blind-class-lookup-fixed-20260726.md`.

### P0: collapse the package-selected interpreter implementations — fixed

The audited `interpreter.rs` routed many Spring/JDK classes to the decoded
fallback by class-name policy. Quickening was not the issue:
`QuickenedCode::resolve` uses an instruction-start bitmap, per-block cumulative
counts, and popcount for O(1) PC mapping.

Resolved on `codex/complete-architecture-remediation-20260726`: package names
no longer select execution policy, the `Frame::is_jdk_class` field and prefix
classifier are removed, and every verified class uses the raw-byte handlers
with unsupported/guarded cases falling through to the decoded handler.

The migration also closed semantic drift found during the change: both paths
now use heap-validated reference coercion and identical `aastore` recovery,
and raw-byte returns emit the JVMTI `MethodExit` event that only decoded
returns emitted before. A verifier-on/raw versus `--noverify`/decoded
differential probe has identical output. The original two-million-iteration
paired probe changed from 0.59s default / 1.47s Spring to 0.55s / 0.56s with
the same checksum. Details are in
`docs/internal/interpreter-package-routing-fixed-20260727.md`.

### P0: make roots an owned subsystem before moving objects by default

`ObjectRef` is a copyable `NonNull<u8>` with unsafe `Send` and `Sync`.
Native/static owners therefore retain raw addresses, while compaction requires
every owner to expose matching scan and rewrite hooks. The audit found roughly
280 textual scan/update hook references across 31 files and many static
`ObjectRef` patterns in `native-builtins`. Missing one is silent heap
corruption.

Introduce a VM-owned `RootRegistry`:

- stacks/JIT frames publish precise maps;
- JNI-like/native/static retention uses stable handles, not raw `ObjectRef`;
- subsystems register `RootProvider` implementations once;
- a stop-the-world root snapshot is the collector's only external root input;
- debug compaction poisons old regions and validates every handle after a move.

This also removes the current dependency from `gc` to `native-collections`.
The VM should aggregate roots downward; a collector should not know a Java
library overlay exists.

### P1: converge JIT lowering instead of growing two feature matrices

The IR path is healthier than older documentation claimed: admission is now up
to 64 invokes, 64 instance-field operations, 64 static-field operations,
16 allocation sites, 8,000 bytecodes, and 20,000 IR nodes. Static calls lower
directly and virtual/interface calls have inline caches/PICs. Tiered inlining
uses 6/35/325-byte callee limits and 750/2,000-byte total budgets.

The split remains structural. The IR declines `athrow`, `invokedynamic`,
`multianewarray`, and type checks. A non-eliminated allocation has no IR
lowering, so the method falls back to the single-pass backend to retain its
inline TLAB bump. Capability and correctness fixes must therefore be
implemented twice or silently change which backend runs.

Use one typed mid-level IR and one lowering library for allocation, calls,
barriers, exceptions, safepoints, and deoptimization metadata. Let baseline
and optimizing tiers differ in graph construction and optimization budget, not
runtime semantics. Add per-reason counters for every fallback and publish them
in benchmark output; optimize the top reasons rather than merely increasing
caps.

The dispatch probe's end-to-end result shows that small interface-call kernels
still fail to realize the intended compiled/PIC performance. Before tuning PIC
assembly, record tier transitions and fallback reasons for the containing
method; otherwise the work risks optimizing a path the workload never reaches.
The existing method-statistics diagnostic could not provide that evidence
because it was silent on normal VM exit. That diagnostic is now fixed and
verified on both controlled shutdown paths; see
`docs/internal/jit-method-stats-normal-exit-fixed-20260726.md`.

### P1: pack instance fields and then shrink the header

Current-status correction (2026-07-27): descriptor-backed instance fields are
already packed at natural 1/2/4/8-byte widths in the production allocators,
interpreter, JIT helpers, and collectors. The remaining 16-byte-cell path is
the required fallback for descriptor-less/padded synthetic slots. The last
allocation hot-path registry lookup was removed by
`../packed-object-fields-performance-20260727.md`; this section is retained as
the recommendation that led to that verification, not as current-state truth.

The 16-byte universal heap field slot dominates ordinary object size. Use
class-computed byte offsets with 1/2/4/8-byte primitive storage, 8-byte
references initially, and alignment-aware field ordering. Keep `Value` as an
interpreter/native boundary type, not the physical instance layout.

Then fold forwarding state into the mark word to reach a 24-byte header.
Reaching 16 bytes also requires relocating or encoding kind, array element
type, age, and flags; it is not achieved by deleting the four-byte identity
hash because alignment restores the space. Compressed references become
worthwhile after precise root and barrier contracts exist.

### P1: replace the native god interface and split compatibility packs

`NativeContext` exposes 280 methods through one trait object. It makes every
native capable of reaching almost every VM service, hides lock-order and
safepoint requirements, complicates mocking, and creates a wide rebuild
boundary. The large native crates dominate the source tree and incremental
build surface.

Replace the trait with a concrete per-call context containing narrow facades
such as `HeapAccess`, `ClassAccess`, `InvokeAccess`, `ThreadAccess`, and
`ExceptionAccess`. Annotate operations that may allocate, block, throw, or
safepoint. Split standard-library compatibility into separately compiled
packs, with the default real-JDK binary linking only essential natives,
intrinsics, and explicitly selected application bridges.

### P1: parse configuration once and make defaults reviewable

There are 528 direct `std::env::var`/`var_os` source call sites in the selected
core directories; `env_cache.rs` itself has 40. Even where a `OnceLock` removes
the system-call cost, scattered parsing duplicates default policy and permits
two subsystems to interpret one feature differently.

Build one immutable typed `VmConfig` at process entry and inject read-only
sub-configs into the VM, GC, JIT, loader, and native registry. Keep environment
variables as a launcher input format, not a global runtime API. Require every
experimental flag to declare type, default, owner, expiry condition, and
whether it is safe to vary between VMs in one process. Add a CI check rejecting
new direct environment reads outside the config layer.

### P2: finish ownership boundaries and phase the bootstrap

The new `SharedVm` realms improve naming, but do not enforce access. Its
constructor/bootstrap region still spans roughly 2,483 lines. Express startup
as typed phases (`Allocated`, `ClassesReady`, `NativesReady`,
`RuntimeReady`) whose transitions validate invariants and return only the
capabilities available in that phase. That makes partial startup failures
testable and removes order assumptions from a monolith.

Break two dependency inversions:

- `classloading` imports the concrete `jit` crate and stores
  `Arc<cratonvm_jit::CompiledMethod>`; move the compiled artifact descriptor
  and parameter-slot contract into `jit-api`;
- `gc` imports `native-collections` for overlay roots; replace this with the
  root registry described above.

Enforce the intended crate layers in CI by checking `cargo metadata`, because a
diagram that allows forbidden edges to compile will drift again.

### P2: make performance and documentation self-invalidating

Architecture prose in this pass was already stale about quickening, JIT caps,
call lowering, inlining, and the achievable first header shrink. Replace exact
duplicated constants in prose with generated tables where possible. Add tests
that assert documented defaults and a small checked-in architecture probe to
the release qualification job.

Track at least:

- interpreter ns/bytecode by opcode family and engine;
- compile count, tier, fallback reason, code bytes, and compile latency;
- allocation bytes/object, TLAB hit rate, barrier cost, pause time, and roots
  scanned by provider;
- class/native resolution hit rates and lock wait time;
- startup phase timings and resident memory; and
- release text size plus per-crate clean/incremental build time.

## Prioritized execution plan

1. **Correctness gate — complete:** the 47 loader-blind calls are migrated,
   dual-loader namespace tests pass, and the deprecated API is denied in
   production.
2. **Interpreter convergence:** one semantic handler set, generated variants,
   package routing removed; preserve checksum and exception tests.
3. **Root ownership:** stable native handles, provider registration, exact JIT
   maps, poisoned-old-region compaction stress.
4. **JIT convergence:** shared runtime lowering and fallback telemetry; add
   allocation, throw, type-check, and invokedynamic support by observed rank.
5. **Layout:** packed primitive fields, 24-byte header, then evaluate compressed
   references with heap-size and barrier data.
6. **Modularity/config:** facade-based native context, compatibility packs,
   immutable typed config, phased bootstrap, enforced dependency rules.
7. **Continuous evidence:** run the checked-in probes on one pinned host and
   alert on checksum mismatch or statistically meaningful median regressions.

## Reproduction

Build the layout probe:

```bash
cargo run --release -p cratonvm-types \
  --example cratonvm_architecture_layout_probe_20260726
```

Build a uniquely named VM and run the paired matrix:

```bash
cargo build --release -p cratonvm-cli \
  --target-dir /data/data/target-architecture-audit-20260726
install -m 755 \
  /data/data/target-architecture-audit-20260726/release/cratonvm \
  /data/data/bin/cratonvm-architecture-audit-20260726

tools/architecture-probe-20260726/run-architecture-probe-20260726.sh \
  -Exe /data/data/bin/cratonvm-architecture-audit-20260726 \
  --java /home/victor/jdk25/bin/java \
  --java-home /home/victor/jdk25 \
  --cpu 13 --reps 3
```

Raw results from this run are checked in beside the runner. Checksums for every
deterministic case must match across VMs before timing is interpreted.
