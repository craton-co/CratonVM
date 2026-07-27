# Architecture audit: measured closeout

Date: 2026-07-26
Base: `dev` at `3be41785e`
Host: Azure x86-64, 16 logical CPUs, JDK 25, Rust 1.96
Scope: the default real-JDK build

Retired: 2026-07-27

## Retirement status

This document is retained under `docs/internal` as historical evidence. Its
actionable architecture findings have been implemented; it is no longer a
backlog or known-issue document.

The final convergence work closes the remaining items:

- baseline and optimizing x86-64 JITs share `jit::runtime_lowering` for
  runtime-sensitive allocation and dispatch stubs;
- megamorphic dispatch uses an eight-set, two-way, atomically published hashed
  vtable tail instead of a mutex-protected helper map;
- escaping C2 allocations lower through the compact-layout/TLAB-aware runtime
  stub instead of rejecting the whole optimized method;
- live `monitorenter`/`monitorexit` bytecodes compile to direct thin-lock
  helpers, while only the exact per-PC scalar proof permits lock elision;
- JIT/native argument decoding, forwarding, and pinning stay inline through
  the eight-slot x64 argument envelope;
- the Bouncy Castle kernels and SunEC compatibility code are physically owned
  and separately compiled by `native-builtins-crypto` and
  `native-builtins-security`; and
- bootstrap order is encoded as typed `Allocated -> ClassesReady ->
  NativesReady -> RuntimeReady` transitions with boundary invariant tests.

The original measurements and assessment below are intentionally preserved so
the evidence that motivated the changes remains reviewable. Sections marked
“fixed” describe the integrated replacement.

### Final convergence evidence

The final implementation checkpoint was merged and pushed to `dev` at
`fac232c91` (including architecture commits `c1269e77c`, `12af763d1`,
`bc68152be`, and `83e078aa5`). Validation used the isolated worktree
`/data/data/wt-architecture-final-convergence-20260727` and the isolated Cargo
target `/data/data/target-architecture-final-convergence-20260727`.

The residual audit after the first integration found and closed three monitor
handoff defects that a scanner-only test could not expose:

- javac synchronized blocks have catch-all handlers that read the saved lock
  local, so the existing params-only handler reconstruction still rejected
  them; the one-shot precise exceptional-frame request is now wired for the
  conservatively supported monitor/`invokestatic` protected shape;
- monitor-only compiled methods were incorrectly marked TLS-free, causing
  `jit_thread_mut()` to fail and return the deopt sentinel; emitting a live
  monitor call now forces dispatch-aware entry; and
- a future handler reference local was decoded before its first `astore`;
  snapshots now replace every provably dead local with `Undefined` before
  reading a machine home, preventing an uninitialized sentinel from becoming
  a fabricated object root.

The checked-in `check-monitor-jit-path-20260727.sh` gate uses ordinary javac
bytecode, requires both monitor methods to reach method-entry JIT compilation,
compares JIT and `--nojit` checksums, forces an exception from inside the
protected region, and verifies that catch-all cleanup both rethrows and
releases the lock. On the shared host's debug artifact it passed with checksum
`824872`; the 50,000-call measured portion was 27,681,621 ns with JIT versus
564,345,523 ns with `--nojit`. These loaded-host timings are diagnostic only;
the correctness and compilation assertions are the retirement gate.

Structural validation at this checkpoint:

- `cargo test -p cratonvm-jit --lib`: 1,026 passed, 0 failed;
- `cargo check -p cratonvm-vm --lib`: passed;
- independent checks for `cratonvm-native-builtins-crypto`,
  `cratonvm-native-builtins-security`, and the facade: passed;
- typed bootstrap boundary tests: 2 passed;
- thin-lock monitor tests: 8 passed; and
- `git diff --check`: passed.

The final default release profile completed from the isolated target in
6m18s after the runner waited for a safe memory window on the shared host.
The installed artifact is
`/data/data/bin/cratonvm-architecture-final-convergence-20260727-r1`
(154,856,848 bytes, SHA-256
`8942855d0d1d66833c014ebca7a792dca4edb5168a18d884f7ab3b9fb2518315`).
Its release gates passed:

- the monitor gate matched checksum `8250000`, compiled both javac monitor
  methods, exercised exceptional cleanup, and measured 18,908,906 ns with JIT
  versus 332,059,324 ns with `--nojit`;
- mono, poly4, and mega16 interface checksums matched across HotSpot, JIT, and
  interpreter, every JIT case beat the CratonVM interpreter, and the poly4
  cloned cache stabilized after four helper calls;
- both normal return and explicit exit emitted exactly one grouped
  JIT-method-statistics record;
- verified interpreter, decoded fallback, JIT, and HotSpot matched checksum
  `14088725157972731584`; and
- `results-final-convergence-20260727.tsv` passed paired row-count and
  deterministic-checksum validation for interpreter, dispatch, allocation,
  exception, monitor, and native-call probes. Native timing output is retained
  but intentionally excluded from checksum equality because that probe reads
  time.

The shared host load was 26.50-26.98 while the paired matrix ran, so those
absolute timings remain diagnostic. The checksum, lowering, compilation, and
exception-cleanup assertions are the retirement evidence.

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

### P0: make roots an owned subsystem before moving objects by default — fixed

`ObjectRef` is a copyable `NonNull<u8>` with unsafe `Send` and `Sync`.
Native/static owners therefore retain raw addresses, while compaction requires
every owner to expose matching scan and rewrite hooks. The audit found roughly
280 textual scan/update hook references across 31 files and many static
`ObjectRef` patterns in `native-builtins`. Missing one is silent heap
corruption.

`vm::memory::native_roots` now owns the compile-time inventory of native/VM
side-table roots, with paired scan/remap callbacks. Dynamic providers are
paired and idempotent, and the collections overlay registers through
`cratonvm_gc::external_roots`. The collector no longer imports
`native-collections`. Precise JIT maps, native pins, relocation fan-out, and
the moving-GC stress probe verify the ownership boundary. Full evidence is in
`docs/internal/moving-gc-root-registry-fixed-20260727.md`.

### P1: converge JIT lowering instead of growing two feature matrices — fixed

The IR path is healthier than older documentation claimed: admission is now up
to 64 invokes, 64 instance-field operations, 64 static-field operations,
16 allocation sites, 8,000 bytecodes, and 20,000 IR nodes. Static calls lower
directly and virtual/interface calls have inline caches/PICs. Tiered inlining
uses 6/35/325-byte callee limits and 750/2,000-byte total budgets.

The two tiers now keep different construction/optimization policies but share
one runtime-sensitive x86-64 lowering module. Both call the same hashed
megamorphic-vtable emitter and allocation ABI emitter, including precise-frame
republication. The PIC's old mutex/`HashMap` megamorphic tail is replaced by a
fixed 8x2 atomic table; generated code hashes the receiver class and probes the
two adjacent ways before resolving through the miss helper.

The optimizing IR now emits `Op::New` with compact layouts enabled and lowers
every allocation that survives scalar replacement through the same
class-initializing, TLAB-aware `new_object` runtime stub used by the baseline
fallback. Null allocation failure is converted to the common `i64::MIN`
exception sentinel. `NewArray` remains a deliberately baseline-specialized
operation, not a silent C2 miscompile or whole-method `New` limitation.

Live monitor bytecodes no longer force baseline compilation to fail. They call
direct VM helpers whose common path is the mark-word thin CAS; only contention
enters the GC-blocked parking protocol. The previous coarse elision test
(`scalar_replaced` nonempty anywhere in the method) was also removed because it
could elide an unrelated escaping receiver; only the exact
`sr_monitor_scalar_ops` per-PC proof may remove a lock.

Ordinary javac synchronized blocks now reach the same path through a narrow
precise exceptional-frame handoff. Unsupported protected throwing shapes
(field/array/allocation/cast/divide and direct `athrow` paths without a
per-site frame) remain interpreted rather than receiving zeroed handler
locals. This is a fail-closed coverage boundary, not an optimistic whole-method
admission.

The normal-exit method statistics diagnostic and the checked-in interface
performance gate remain the continuous evidence for tier transitions and
dispatch behavior. See
`docs/internal/jit-method-stats-normal-exit-fixed-20260726.md`.

### P1: pack instance fields and then shrink the header — fixed/reevaluated

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

The production contract intentionally remains a 32-byte header while compact
fields deliver the material footprint reduction. Forwarding and locking retain
separate words because folding them would couple collector relocation state to
the thin/inflated monitor state machine. A 24-byte header is therefore an
independent object-model experiment, not an unresolved correctness issue in
this audit. The implemented layout and its compatibility contract are recorded
in `docs/internal/compact-object-and-field-layout.md`.

### P1: replace the native god interface and split compatibility packs — fixed

`NativeContext` exposes 280 methods through one trait object. It makes every
native capable of reaching almost every VM service, hides lock-order and
safepoint requirements, complicates mocking, and creates a wide rebuild
boundary. The large native crates dominate the source tree and incremental
build surface.

`NativeContext` is now a composition facade over narrow capability traits
(`NativeHeapAccess`, `NativeClassAccess`, `NativeInvokeAccess`,
`NativeThreadAccess`, `NativeSystemAccess`, and peers). Implementations and
test mocks compile against the capability boundary, and the loader-aware
`invoke_special_by_class_id` operation preserves declaring-class identity.

The compatibility split is physical, not a module alias: Bouncy Castle AES,
ChaCha, NewHope and tables live in `native-builtins-crypto`; SunEC integer and
point code lives in `native-builtins-security`. Cargo compiles both as
independent workspace crates, while `native-builtins` retains only registration
and Java-object marshalling. Its direct P-curve dependencies moved with the
SunEC implementation.

The native-call boundary also shares an eight-slot inline scratch contract.
Raw JIT argument decoding uses `SmallVec<[Value; 8]>`; forwarding and pin-index
buffers use the same capacity. Receiver plus ordinary x64 register arguments
therefore cross the bridge without transient heap allocations, with a tested
heap fallback for larger descriptors.

### P1: parse configuration once and make defaults reviewable — fixed

There are 528 direct `std::env::var`/`var_os` source call sites in the selected
core directories; `env_cache.rs` itself has 40. Even where a `OnceLock` removes
the system-call cost, scattered parsing duplicates default policy and permits
two subsystems to interpret one feature differently.

The launcher now installs the immutable typed `VmFlags` snapshot before
subsystems initialize. Declared flags are served from that snapshot; undeclared
application/OS variables retain live environment semantics. Direct reads were
removed from the audited core crates, and the flag-surface check rejects new
bypasses. `--nojit` is a typed overlay rather than a late environment mutation.
See `docs/internal/runtime-environment-boundary-fixed-20260727.md`.

### P2: finish ownership boundaries and phase the bootstrap — fixed

Startup now carries a private `BootstrapPhase<State>` token through
`Allocated`, `ClassesReady`, `NativesReady`, and `RuntimeReady`. Each consuming
transition validates the invariant owned by that boundary: classes exist and
`java/lang/Object` is present, native registration is nonempty, and runtime
hooks are wired. Only `BootstrapPhase<RuntimeReady>::finish` can produce the
completed bootstrap duration. Unit tests exercise every failed boundary and
the only valid transition chain.

Both dependency inversions are closed. The classloading invoke cache is generic
over its compiled artifact and depends only on `jit-api`; the VM supplies
`Arc<CompiledMethod>`. The GC consumes registered external-root providers and
has no `native-collections` dependency. Dependency-tree assertions accompany
both boundaries. See
`docs/internal/classloading-jit-dependency-inversion-fixed-20260727.md` and
`docs/internal/moving-gc-root-registry-fixed-20260727.md`.

### P2: make performance and documentation self-invalidating — fixed

The checked-in `tools/architecture-probe-20260726` matrix compares deterministic
checksums, alternates VM order, pins a CPU, and rejects incomplete runs.
Focused gates cover interpreter equivalence, JIT method statistics, and
interface dispatch and thin-lock monitor performance. Layout constants and
runtime-helper offsets have executable inventory/contract tests rather than
prose-only duplicates.

The runtime diagnostics expose the originally requested dimensions:

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
2. **Interpreter convergence — complete:** one semantic handler set, generated
   variants, package routing removed; checksum and exception tests preserved.
3. **Root ownership — complete:** paired provider registration, exact JIT
   maps/native pins, moving-GC relocation stress.
4. **JIT convergence — complete for the tracked residuals:** shared hashed
   dispatch and allocation lowering, escaping C2 allocation, direct live
   monitor helpers, and exact lock elision.
5. **Layout — complete for this audit:** packed primitive/reference fields and
   a documented 32-byte compatibility header; further header compression is a
   separate experiment.
6. **Modularity/bootstrap — complete for the tracked residuals:** facade-based
   native context, physically separate crypto/security packs, and typed
   bootstrap phases.
7. **Continuous evidence — complete:** checked-in pinned-host checksum and
   performance gates, plus executable layout/lowering invariants.

## Reproduction

Build the layout probe:

```bash
cargo run --release -p cratonvm-types \
  --example cratonvm_architecture_layout_probe_20260726
```

Build a uniquely named VM and run the paired matrix:

```bash
cargo build --release -p cratonvm-cli \
  --target-dir /data/data/target-architecture-final-convergence-20260727
install -m 755 \
  /data/data/target-architecture-final-convergence-20260727/release/cratonvm \
  /data/data/bin/cratonvm-architecture-final-convergence-20260727-r1

INTERP_ITERS=100000 DISPATCH_ITERS=100000 ALLOC_ITERS=20000 \
EXCEPTION_ITERS=20000 MONITOR_ITERS=20000 NATIVE_ITERS=20000 \
tools/architecture-probe-20260726/run-architecture-probe-20260726.sh \
  -Exe /data/data/bin/cratonvm-architecture-final-convergence-20260727-r1 \
  --java /home/victor/jdk25/bin/java \
  --java-home /home/victor/jdk25 \
  --cpu 13 --reps 1 \
  --out tools/architecture-probe-20260726/results-final-convergence-20260727.tsv

tools/architecture-probe-20260726/check-monitor-jit-path-20260727.sh \
  -Exe /data/data/bin/cratonvm-architecture-final-convergence-20260727-r1 \
  --java-home /home/victor/jdk25 \
  --iterations 500000 --cpu 13
```

Raw results from this run are checked in beside the runner. Checksums for every
deterministic case must match across VMs before timing is interpreted.
