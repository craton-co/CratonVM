# Full scoped review - 2026-07-02

Branch/worktree: `codex/full-review-20260702-001` in
`C:\craton\CratonVM-review-20260702-001`.

Base: `dev` at `911e0a2539f629fe079c7fcd5b4f45e48168040a`.

Scope: all repository files except `../../../apps` and `..`, per the
follow-up instruction. Findings below intentionally do not rely on reviewing
those excluded folders.

## Executive status

CratonVM is not ready for a public release, a blocking CI gate, or a broad
crates.io publication wave yet. The workspace builds, but the normal release
quality gates do not pass:

- `cargo build --workspace`: passed, with warnings.
- `cargo fmt --all -- --check`: failed.
- `cargo test --workspace --all-targets --no-fail-fast`: failed during
  compilation in `jit/tests/ir_vs_singlepass.rs:1050`.
- `cargo clippy --workspace --all-targets -- -D warnings`: failed.
- 85 percent test coverage cannot be established because workspace tests do not
  compile and coverage is advisory with no threshold.
- `../../../fuzz` does not build because it requests an undeclared
  `legacy-synthetic-crypto` feature from `cratonvm-native-builtins`.
- crates.io readiness is blocked by failing package checks, unpublished
  dependency graph edges, stale release docs, and crates with intentionally
  `publish = false` dependencies still reachable from default features.

The highest-risk technical themes are:

- GC and handle lifetime unsafety across moving collectors, native APIs,
  AWT queues, flat C handles, and G1 scratch buffers.
- Fail-open behavior for security-sensitive mechanisms: TLS trust stores,
  `-javaagent`, JVMTI agents, path validation, UDP egress policy, invalid C ABI
  values, and fuzz target coverage.
- Parser/verifier robustness gaps in class attributes, JImage, JFR dump parsing,
  descriptors/signatures, and legacy bytecode verifier paths.
- Release process drift: docs still claim warning-free builds, 19 crates,
  6,000+ tests, and blocking quality gates, while current verification shows
  otherwise.

## Central verification

### Build

`cargo build --workspace` passed. It emitted warnings that should be treated as
release blockers if the public docs continue to claim a warning-free codebase:

- `../../../vm-cli/src/main.rs` is present in both the `cratonvm` and optional `java`
  binary targets.
- `panic` is ignored for the bench profile.
- `craton-gpu` could not find Java annotation sources.
- `cratonvm-jit-cuda` emitted `javac` fixture/annotation failures.
- `../../../jit/src/x64.rs` has unused doc comments/attributes and direct function-item
  integer casts.
- `native-builtins` has many `unexpected_cfgs` warnings for undeclared features:
  `app-stubs`, `legacy-synthetic-crypto`, and `synthetic-quarkus-arc`.
- `native-builtins/src/t27_tls.rs:119` / `:138` exposes a private interface
  warning for `RuntimeTlsIdentity`.

### Formatting

`cargo fmt --all -- --check` failed with rustfmt diffs across multiple crates,
including `classloading`, `gc`, `vm`, and `vm-cli`. This is a direct release/CI
gate failure.

### Workspace tests

`cargo test --workspace --all-targets --no-fail-fast` failed before running the
full suite:

- `jit/tests/ir_vs_singlepass.rs:1050` expects
  `&dyn Fn(u16) -> Option<(usize, u8, u32, bool)>` but receives
  `&dyn Fn(u16) -> Option<(usize, u8)>`.

Because the workspace test suite does not compile, the tests do not all pass.

### Clippy

`cargo clippy --workspace --all-targets -- -D warnings` failed:

- `vm/build.rs:14` and `vm/build.rs:15`: `clippy::doc_lazy_continuation`.
- `types/src/value.rs:149`: `comparison_to_empty`.
- `../../../jfr/src/builtin.rs`: several `too_many_arguments` failures.
- `../../../jfr/src/dump.rs`: `seek_from_current`.
- `../../../jfr/src/event.rs`: `should_implement_trait`.
- `../../../jfr/src/repository.rs`: `result_large_err` and doc-list indentation.

### Coverage

The repository cannot currently prove 85 percent code coverage:

- `docs/COVERAGE.md:61` says the coverage job is `continue-on-error`.
- `docs/COVERAGE.md:81` says no coverage threshold is enforced.
- Workspace tests fail to compile, so a meaningful whole-workspace coverage run
  is not available.
- A static `rg` count found 12,569 Rust test markers, but this is not coverage
  data and does not account for skipped/ignored tests.

## Module review findings

### `reader`

High:

- `reader/src/attribute.rs:959` caps nested attributes at 64 levels but still
  permits stack overflow. `cargo test -p cratonvm-reader` aborts with
  `STATUS_STACK_OVERFLOW` in deeply nested `Code`/`Record` attribute cases.
  Recursive paths include `reader/src/attribute.rs:1265` and `:1679`.
- `reader/src/jimage.rs:638` negates an untrusted redirect value. `i32::MIN`
  can overflow before checked arithmetic.

Medium/low:

- `reader/src/signature.rs:353`, `:360`, and `:461` accept malformed generic
  signatures with trailing garbage because parsers do not require EOF.
- `reader/src/class_file_version.rs:133` uses a lexicographic version-range
  check that accepts invalid minors and rejects preview minors at the max.
- `reader/src/class_reader.rs:160` accepts trailing garbage after class
  attributes.
- `reader/src/class_reader.rs:96`, `:458`, and `:493` use
  `from_bits_truncate`, silently erasing unknown access flags.
- `reader/src/jimage.rs:329` accepts jimage minor `1.x` despite comments/docs
  saying `1.0`.

Tests:

- `cargo test -p cratonvm-reader` failed by stack overflow.
- Targeted nested attribute tests also failed by stack overflow.

### `types`

High:

- `types/src/value.rs:483` exposes safe `decode_value` that can fabricate an
  `ObjectRef` from any non-null 8-byte aligned `VTAG_OBJECT` payload. This
  bypasses the stronger `plausible_heap_pointer` check at
  `types/src/value.rs:475`.
- `types/src/value.rs:525` has the same gap through
  `jlong_bits_as_aligned_object_ptr`.
- `types/src/compact_value.rs:467` and `:514` accept unaligned/null-page object
  payloads, then `to_value` later decodes them as `Long`; tests/docs around
  `types/src/compact_value.rs:2481` claim unaligned values panic.
- `types/src/heap_types.rs:425` relies only on `debug_assert!` for inflated
  monitor pointer alignment. Release builds can mask a corrupted pointer.

Medium/low:

- `types/src/value.rs:337` marks `ObjectRef` as `Send`/`Sync` more broadly than
  the surrounding invariants justify.
- `types/src/float_format.rs:22` and `:46` format subnormal Java
  `double`/`float` values differently from JDK 25 for `MIN_VALUE`.
- `types/src/field_layout.rs:110` resizes a dense Vec to `class_id + 1`; a
  corrupt/high class id can force huge allocation.
- `types/src/intern.rs:33` uses `FxHash` for classfile strings. That is a
  performance tradeoff but weak against adversarial hash flooding.
- `types/README.md:13` says `StringPool` is lock-free, while the implementation
  uses `RwLock`.

Tests:

- `cargo test -p cratonvm-types` passed.

### `native-api`

High:

- `native-api/src/registry.rs:598` safe trait defaults
  `copy_from_native_memory` and `copy_to_native_memory` perform raw
  `copy_nonoverlapping` from any positive `i64`. These should be unsafe,
  required overrides, or fail-closed defaults.
- `native-api/src/registry.rs:827` documents `bulk_array_copy` as returning
  `false` on mismatch, but the default loops unconditionally without length,
  type, overflow, or overlap checks and returns `true`.

Medium/low:

- `native-api/src/charset.rs:160` validates malformed UTF-16 only for UTF-8
  strict encode. UTF-16 writes lone surrogates, UTF-32 replaces, and multibyte
  behavior is lossy.
- `native-api/src/charset.rs:831` lossy UTF-16/UTF-32 decoders drop trailing
  incomplete bytes instead of producing U+FFFD.
- `native-api/src/fd_table.rs:1844` in-memory pipes use unbounded `VecDeque`,
  enabling memory exhaustion.
- `native-api/src/ffi.rs:552` `UpcallTable::generation_of` can return a
  generation for empty removed slots; stale handles can resolve to a later
  callback.
- `native-api/src/fd_table.rs:1364` reports TCP/UDP writable as always true.
- `native-api/src/native_ring.rs:180` records native exits by ring slot index,
  which can corrupt diagnostics after wraparound.

Tests:

- `cargo test -p cratonvm-native-api` passed.

### `native-collections`

High:

- `native-collections/src/lib.rs:34326`, `:34409`, `:34612`, `:34880`, and
  `:34911` implement `CompletableFuture` dependent stages that can run eagerly
  on pending/exceptional sources, invoke callbacks with null, and swallow
  errors.
- `native-collections/src/lib.rs:32983` and `:33035` allow `StampedLock` to
  grant conflicting locks after a spin/yield cutoff.

Medium/low:

- `native-collections/src/lib.rs:4528` and `:4969` make `HashMap.keySet()` drop
  a legal null key.
- `native-collections/src/lib.rs:34066` makes
  `CopyOnWriteArrayList.addIfAbsent` non-atomic.
- `native-collections/src/lib.rs:33451` and `:33541` allow
  `PriorityBlockingQueue` to accept null and make `take` equivalent to `poll`.
- `native-collections/Cargo.lock:83` still contains stale `rustjvm-*` package
  naming.
- `native-collections/README.md:18` has stale synthetic-JDK claims.

Tests:

- `cargo test -p cratonvm-native-collections --locked` passed.

### `native-io`

High:

- `native-io/src/lib.rs:12672` and `:12682` async file open bypasses path
  validation/confinement, ignores `OpenOption[]`, opens read+write+create, and
  casts negative positions to `u64` at `native-io/src/lib.rs:12703` and `:12748`.
- `native-io/src/datagram.rs:645` registers guarded UDP behavior, but
  `native-io/src/lib.rs:13478` overwrites it with a legacy
  `DatagramChannel.send` path. `native-io/src/lib.rs:13668` sends via
  `udp_send` without `check_outbound_target`, bypassing SSRF/private-network
  policy.

Medium/low:

- `native-io/src/lib.rs:12922` async read/write futures return synthetic
  `CompletedFuture`, but no local `Future.get/isDone/cancel/isCancelled`
  registration was found.
- `native-io/src/lib.rs:12824` and `:12876` pass a Java `String` to
  `CompletionHandler.failed` instead of a `Throwable`.
- `native-io/src/lib.rs:412` and `:428` confined path validation fails open when
  CWD canonicalization fails.

Tests:

- `cargo test -p cratonvm-native-io` passed.

### `native-builtins`

High:

- `native-builtins/src/x509_manager.rs:1250`, `:1286`, and `:1288` can trust a
  same-subject fake root. Anchor matching is by subject only; verification uses
  the chain-supplied final cert and skips the final cert.
- `native-builtins/src/x509_manager.rs:970` and `:998` make custom trust stores
  non-restrictive by always adding OS roots after caller-provided keystores.
- `native-builtins/src/keystore.rs:1173`, `:1181`, `:1190`, and `:1463`, plus
  `native-builtins/src/t27_tls.rs:230` and `:2940`, let keystore loads globally
  expand TLS trust roots. Deleting an entry does not remove global trust.

Medium/low:

- `native-builtins/src/lib.rs:6841` and
  `native-builtins/src/lang_system.rs:2678` / `:2704` wire
  `ClassLoader.defineClass2(ByteBuffer, ...)` to a byte-array handler.
- `native-builtins/src/lang_system.rs:2798` and `:2940` swallow `defineClass`
  failures as null instead of throwing exceptions.
- `native-builtins/src/zip_real.rs:437` and `:617` have direct-buffer deflate
  paths that silently make zero progress.
- `native-builtins/src/lib.rs:6860`, `:6871`, and `:6881` make
  `Thread.setContextClassLoader(null)` fail to round-trip because the getter
  falls back to the app loader.
- `native-builtins/Cargo.toml:15` and `:18` do not declare features used by
  source: `app-stubs`, `legacy-synthetic-crypto`, and
  `synthetic-quarkus-arc`.
- `../../../native-builtins/README.md` says `legacy-synthetic-crypto` is default-on,
  while `default = []`.
- `native-builtins/tests/stub_ratchet.rs:73` still uses a high baseline
  compared with the live synthetic-stub count, and native registrations tagged
  as `Bridge` can evade the census.

Tests:

- `cargo test -p cratonvm-native-builtins --test stub_ratchet` passed.
- `cargo test -p cratonvm-native-builtins --test aes_gcm_kat` passed.
- Builds emitted many check-cfg/private-interface warnings.

### `classloading`

High:

- `classloading/src/verifier.rs:262` has a legacy `jsr`/`ret` fallback that can
  accept invalid control flow. Related issues: `classloading/src/verifier.rs:972`
  checks only `target > code_len`; `:1041` drops negative/overflow cases; targets
  are not instruction-boundary checked; `ret` is excluded at `:1095`.

Medium/low:

- `classloading/src/class_path.rs:89` and `:94` mmap classpath reads can crash
  if files are mutated/truncated concurrently.
- `classloading/src/proxy_gen.rs:146`, `:515`, and `:988` can panic on constant
  pool/local-slot overflow despite returning `Result`.
- `classloading/src/class_path.rs:886`, `:1668`, `:1875`, and `:1983` have
  incomplete Windows backslash filtering; `pkg\Foo` is not covered.
- `classloading/Cargo.toml:14` excludes fixtures/classes, while tests such as
  `classloading/tests/wp2_4b_redefine.rs:43` and
  `classloading/tests/wp_security_robustness.rs:59` silently return if fixtures
  are absent. Packaged crate tests lose meaningful integration coverage.
- README/Cargo/lib docs contain mojibake and stale signer TODOs.

Tests:

- `cargo test -p cratonvm-classloading --quiet` passed, with ignored tests.

### `jit-api`

Low:

- `jit-api/src/gpu_lowering.rs:12` and `:68` have contradictory docs: one says
  exactly one implementor, another says zero consumers. No local
  `impl GpuLowering` / `PtxEmitter` was found.
- `jit-api/src/lib.rs:526` and `:1382` say 45 fields / 38 required pointers,
  but the current ABI is 46 / 39.

Tests:

- `cargo test -p cratonvm-jit-api` passed.
- `cargo test -p cratonvm-jit-api --features gpu-lowering` passed.

### `jit`

Critical/high:

- `jit/src/x64.rs:24756` `estimate_max_stack` underestimates accepted bytecode
  frame-spill space. It does not count `ldc`, `ldc_w`, or `ldc2_w` pushes,
  while codegen pushes at `jit/src/x64.rs:16371`. `spill_size` is computed at
  `jit/src/x64.rs:7451`, and `push_stack` lacks bounds checks at
  `jit/src/x64.rs:8166`. A valid method can alias frame areas. Prefer
  `CachedBytecodeMethod.max_stack` or a complete verifier-derived estimator.
- `jit/src/lib.rs:3971` has unsafe JIT code-range replacement semantics.
  `JitCache` uses an XOR hash at `jit/src/lib.rs:3798`, registers a new code
  range before overwriting at `:4008`, and can retain stale range to
  `cm_ptr` mappings.
- `jit/tests/ir_vs_singlepass.rs:1041` / `:1050` do not compile after field
  resolver widening, breaking workspace tests.

Medium/low:

- `jit/src/deopt.rs:970` and `:994` leave production
  `materialize_virtual_objects` as a panicking stub, conflicting with scalar
  deopt comments in `jit/src/lib.rs:932`.
- `jit/src/x64.rs:1461` `jit_scan` accepts switch payloads past `code_len`;
  emitters reject later at `jit/src/x64.rs:18441` and `:18573`.
- `jit/src/x64.rs:23774` has misplaced docs/attributes on a `thread_local!`
  macro invocation.

Tests:

- `cargo test -p cratonvm-jit` failed with the resolver compile error.

### `jit-cuda`

High:

- `jit-cuda/src/analyzer.rs:436`, `:466`, and `:587` can false-positive
  reduction recognition. Stack-mutating operations such as `pop` do not clear
  the reduction candidate set.

Medium/low:

- `jit-cuda/build.rs:73` and `:176` make fixture generation non-reproducible:
  missing `craton.gpu.*` Java sources produce build warnings, and tests pass
  only because existing `.class` files remain.
- Annotation admission is analyzer-only/no-op in places:
  `jit-cuda/src/analyzer.rs:174`, `:642`, and `:649`; lowering does not fully
  consume the annotations. `AllowDivByZero` is not passed through, and lowering
  still emits a zero-divisor guard at `jit-cuda/src/lowering/emit.rs:944`.
- `jit-cuda/src/annotations.rs:119` documents `@GpuExclude` precedence, but
  `jit-cuda/src/analyzer.rs:225` ignores it for direct API users.
- Analyzer/lowering support drift: analyzer permits operations that lowering
  rejects, including `frem`/`drem` at `jit-cuda/src/lowering/emit.rs:620` and
  compares at `:673`.
- `jit-cuda/Cargo.toml:34` declares `gpu-it` without wiring meaningful
  integration tests; the ptxas test is ignored at `jit-cuda/tests/lowering.rs:512`.
- README usage examples are stale.

Tests:

- `cargo test -p cratonvm-jit-cuda` passed, but manual `javac` probes failed
  and build warnings confirm missing sources.

### `cuda-bridge`

High:

- `cuda-bridge/src/lib.rs:688` and `:754` expose safe async memcpy APIs while
  relying on unenforced host-buffer lifetimes. Backend docs at
  `cuda-bridge/src/backend_cuda.rs:769` and `:1070` describe DMA that can
  outlive the caller's slice. Make these APIs unsafe or return guards/pinned
  buffers/scoped streams.

Medium/low:

- Kernel launch queues work before completion-event bookkeeping. If event
  creation fails after launch, stale buffer ordering can remain
  (`cuda-bridge/src/launch.rs:167`, `:193`;
  `cuda-bridge/src/backend_cuda.rs:475`, `:481`).
- Default no-CUDA tests report green while skipping all assertions
  (`cuda-bridge/tests/stub_op_log.rs:41`, `cuda-bridge/src/lib.rs:132`).
- `gpu-it` is advertised but has no local tests (`cuda-bridge/Cargo.toml:40`,
  `cuda-bridge/README.md:13`).
- README examples use `cuda_bridge::`, while the package default crate path is
  `cratonvm_cuda_bridge`.

Tests:

- `cargo test -p cratonvm-cuda-bridge` passed.
- `cargo check -p cratonvm-cuda-bridge --features cuda` passed.
- `cargo test -p cratonvm-cuda-bridge --features cuda` ran zero tests.

### `gc`

High:

- `gc/src/collector.rs:149` and `:164` expose safe public construction for
  `StopTheWorldToken`; it does not prove a real stop-the-world state. Safe
  moving-GC entrypoints can race mutators.
- `gc/src/g1.rs:3730`, `:5093`, `:5162`, `:5284`, and `:5332` use aligned
  `ptr::read`/`write` over `[u8; N]` scratch buffers with alignment 1. This is
  UB for `Value`, `i64`, `u64`, and `f64`.

Medium/low:

- `gc/src/g1.rs:4375` and `:4392` descriptor-aware allocation leaves reference
  fields/beyond-descriptor slots zeroed; zeroed `Value` decodes as `Int(0)`,
  not Java null.
- `gc/src/g1.rs:5288` reads boolean arrays as signed bytes, unlike the shared
  heap path at `gc/src/heap.rs:1527`, which zero-extends.
- `gc/src/g1.rs:1689` leaves humongous handling as a full-GC-only TODO.
- `gc/tests/loom_satb.rs:20` and `:52` keep SATB loom tests mostly ignored or
  model-only, and the cfg is not declared for check-cfg.
- `gc/README.md:33` contains stale API names.

Tests:

- `cargo test -p cratonvm-gc` passed unit/integration/property/doctests, with
  soak/loom coverage ignored.

### `native-awt`

High:

- EDT callbacks and peer event sources are not GC-rooted. `RunnableEntry` stores
  bare `ObjectRef` plus generation and skips `run()` if generation changed
  (`native-awt/src/edt.rs:69`, `:302`;
  `native-awt/src/natives.rs:2219`). Peer sources use the same raw-pointer
  pattern and can become null after GC (`native-awt/src/natives.rs:209`, `:225`).
  `NativeContext` has persistent global roots at
  `native-api/src/registry.rs:460`; use them for pending runnables and live peer
  sources.
- Java-controlled image/graphics sizes can abort the VM through huge
  non-overflowing allocations. `BufferedImage` accepts up to `32767x32767`
  (`native-awt/src/natives.rs:1611`) and `image.rs:92` allocates with
  `vec![fill; len]`. Component bounds also route large dimensions into
  `SoftwareRenderer::new` (`native-awt/src/natives.rs:821`, `:378`;
  `native-awt/src/renderer.rs:633`).
- `native-awt/src/renderer.rs:1539` `copyArea` allocates and iterates over
  caller-controlled huge temporary buffers even when the visible surface is
  tiny. Clip before allocation and cap temporary spans.

Medium/low:

- `native-awt/src/natives.rs:2085` registers `EventQueue.postEvent(AWTEvent)`
  as a no-op. Component/focus/action events are also not synthesized and can be
  dropped at `native-awt/src/natives.rs:1998`.
- `native-awt/src/renderer.rs:43`, `:1591`, `native-awt/src/edt.rs:813`, and
  `native-awt/src/peer.rs:124` have geometry overflow paths on extreme Java
  `int` coordinates.
- `native-awt/src/lib.rs:87` and `native-awt/src/natives.rs:524` register
  natives with default categories. Because the registry default is
  `SyntheticStub` (`native-api/src/registry.rs:2548`, `:2773`), real bridge
  behavior and placeholders are indistinguishable.
- `native-awt/Cargo.toml:15` sets `publish = false`, contradicting the root
  workspace comment that only `fuzz` is withheld.

Tests:

- `cargo test -p cratonvm-native-awt` passed.
- No module-local integration tests, benches, or examples exist.

### `craton-gpu`

High/medium:

- `craton-gpu/build.rs:70` reuses `OUT_DIR/classes` without clearing it. On
  missing `javac`, missing Java sources, or `javac` failure
  (`craton-gpu/build.rs:112`, `:141`, `:159`), stale `.class` files can remain
  visible through `ANNOTATIONS_DIR` and can be repackaged into jars.
- `craton-gpu/build.rs:219` resolves `../craton-gpu-java/src/main/java` from
  the package root, which does not match the sibling fallback documented in
  `craton-gpu/README.md:32` on fresh non-Windows checkouts unless
  `CRATON_GPU_JAVA_SRC` is set.
- `craton-gpu/README.md:49` tells users to run `cargo build -p craton-gpu`, but
  the package is `cratonvm-gpu`.

Tests:

- `cargo test -p cratonvm-gpu` passed but ran zero tests.

Readiness:

- `craton-gpu/Cargo.toml:21` intentionally sets `publish = false`.
- Before publication, choose a strict artifact policy: bundled annotation
  sources, checked-in generated artifacts, or hard failure when annotations
  cannot be built.

### `vm`

High:

- `vm/src/config.rs:262` exposes `jvmti_agent_options`, but no VM consumer was
  found. Separately, `vm/src/jvmti/agent.rs:127` returns `Ok(())` after load
  failures, missing entry points, or nonzero `Agent_OnLoad` return codes.
  `-agentlib` / `-agentpath` agents can silently fail open.
- `vm/src/runtime/agent_loader.rs:295` logs and continues for most
  `-javaagent` premain load errors, and `:420` treats thrown non-`Error`
  exceptions as nonfatal. Instrumentation/security agents can fail open.

Medium/low:

- `vm/src/config.rs:224` advertises container support and
  `vm/src/runtime/container.rs:333` documents a heap sizing hook, but
  `vm/src/vm/vm_init.rs:1087` passes `config.max_heap_size` directly.
- `vm/src/bin/bench_gate.rs:31` documents exit 2 for missing/corrupt baselines,
  but `:539` converts missing baselines to `Baseline::default()`. With
  `bench/baseline.json` and `bench/hotspot-baseline.json` absent, the default
  gate is non-enforcing.
- `vm/build.rs:45`, `:76`, and `:97` can panic on stale `javac` cache state and
  write generated `.class` files into `../../../vm/tests/resources`, making ordinary
  builds/tests non-read-only.
- `vm/Cargo.toml:33` enables `awt` by default while `vm/Cargo.toml:72` notes
  `cratonvm-native-awt` is unpublished. This blocks crates.io dependency
  resolution for downstream crates that use default VM features.
- `vm/Cargo.toml:19` excludes fixtures/classes but not tracked `../../../vm/target_bench`
  output; 304 generated files, about 2.4 MB, are tracked.
- `vm/benches/vm_benchmarks.rs:977`, `:1032`, `:1061`, and `:1090` include
  placeholder/lower-bound benchmark loops rather than the named hot paths.
- VM tests include many ignored/skipped/todo cases, including
  `vm/tests/wp4_6_chm_basic.rs:91`, `vm/tests/wp7_3_sql_types_datetime.rs:168`,
  and `vm/tests/t4_8_jdwp_conformance.rs:302`.

Tests:

- VM package tests were not run during the module review because `../../../vm/build.rs`
  mutates `../../../vm/tests/resources` when `javac` is available.

### `vm-cli`

High:

- `cargo test -p cratonvm-cli --bin cratonvm` fails. The test
  `hotspot_xx_non_gc_use_flags_not_mistaken_for_selector` at
  `vm-cli/src/main.rs:4428` still expects `-XX:+UseStringDeduplication` to be
  dropped, while implementation maps it to `--XX:StringDedup true` at
  `vm-cli/src/main.rs:1040`.
- `vm-cli/src/main.rs:2028` arms the default watchdog for every normal run and
  aborts after 120 seconds unless `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`. This
  breaks healthy long-running Java programs and services. The main run path can
  wait indefinitely at `vm-cli/src/main.rs:2644`, while abort is unconditional at
  `:2255`.

Medium/low:

- `vm-cli/src/main.rs:1049`, `:1238`, and `:1295` make
  `-XX:+HeapDumpOnOutOfMemoryError` and `-XX:HeapDumpPath=...` effectively
  ignored in real CLI normalization, even though direct extractor tests pass.
- `vm-cli/src/main.rs:1464` copies non-`.jar` `--jar` inputs such as WAR/EAR
  files to `%TEMP%` and never deletes them.
- `vm-cli/src/main.rs:132` declares `--enable-native-access` as optional-value
  without `require_equals`; `--enable-native-access Main` can parse `Main` as
  an option value instead of the main class.
- `vm-cli/tests/common/mod.rs:11` says tests do not need a system JDK, but
  `vm-cli/src/main.rs:1643` defaults to `VmConfig::with_host_jdk_default()`.
- `vm-cli/Cargo.toml:18` excludes `tests/resources/**` while integration tests
  use fixtures through `vm-cli/tests/common/mod.rs:24`.
- `vm-cli/README.md:10` says the crate ships both `cratonvm` and `java`, but
  `vm-cli/Cargo.toml:38` gates `java` behind `java-bin-alias`, off by default.

Tests:

- `cargo test -p cratonvm-cli --bin cratonvm` failed: 83 passed, 1 failed.

### `jfr`

High:

- `jfr/src/dump.rs:1413` `read_events` does not enforce
  `total_size >= size_len + event-header fields`, and header decodes use
  `&data[rpos..]` instead of a record-bounded slice. Malformed records can read
  into following bytes; zero-field event types can leave `pos` unchanged and
  hang.
- `jfr/src/builtin.rs:40` caches `EventTypeId` in global per-call-site
  `OnceLock`s, but ids are assigned per `EventTypeRegistry`
  (`jfr/src/event.rs:262`). Multiple recorders or changed registration order can
  write wrong type ids.

Medium/low:

- `jfr/src/dump.rs:756` drops unknown `extra_events` but still serializes
  unknown events already in the repository, creating files the crate's own
  reader can reject.
- `jfr/src/recording.rs:491` exposes `get_recording_mut`, allowing callers to
  mutate recording lifecycle without updating `running_ids` used by
  `FlightRecorder::record_event` at `jfr/src/recording.rs:362`.
- `jfr/src/repository.rs:1014` exposes `ThreadRingRegistry::new`, but
  `register_current_thread` uses one process-wide TLS slot
  (`jfr/src/repository.rs:952`). Multiple registries can lose events.
- `jfr/src/builtin.rs:3389` exposes `JfrProfile`, but `apply_to` at `:3844`
  leaves stacktrace/period mostly inert.
- `jfr/README.md:27` says JMC opens/renders files, while `jfr/src/dump.rs:481`
  says stock JMC/`jfr` cannot parse the custom metadata.
- `jfr/src/dump.rs:8` says the header is 68 bytes; `HEADER_SIZE` at
  `jfr/src/dump.rs:47` is 72.

Tests:

- `cargo test -p cratonvm-jfr` passed.
- Workspace clippy fails inside `jfr`.

### `libcratonvm`

High:

- `libcratonvm/src/lib.rs:731` stores object addresses in `by_addr` for dedup,
  but moving GC updates `JniGlobalRefs`, not this map. After GC, the same object
  can get duplicate tokens, or a new object at a stale address can inherit the
  old object's token.
- `libcratonvm/src/lib.rs:556` documents flat handles as freely
  create/destroyable, but `cratonvm_create` sets process/TLS JNI context once
  and `with_vm` does not restore per-handle context. Multiple flat VMs or mixed
  JNI/flat use can cross-wire calls; `libcratonvm/src/lib.rs:890` clears
  whichever TLS context is current.
- `libcratonvm/src/lib.rs:650` maps unknown `CratonValue` tags and stale object
  tokens to Java null. Bad C input should return `CRATON_TAG_ERROR`/`JNI_ERR`,
  not silently become null.

Medium/low:

- `libcratonvm/src/lib.rs:770` converts every distinct returned object into a
  JNI global ref, released only on VM destruction at `:795`. Long-running
  embedders need `cratonvm_release_ref` or local-frame APIs.
- `libcratonvm/src/lib.rs:261` ignores requested JNI version and
  `ignoreUnrecognized`; unsupported versions/options should produce
  `JNI_EVERSION`/`JNI_ERR`.
- `libcratonvm/cbindgen.toml:32` allowlists only three structs, while
  `libcratonvm/build.rs:21` claims regeneration is byte-compatible. The
  checked-in header also needs aliases, constants, opaque `CratonVm`, and all
  `cratonvm_*` declarations.
- `libcratonvm/Cargo.toml:30` depends on `cratonvm-vm` with default features,
  pulling `cratonvm-native-awt`, which is `publish = false`.
- `libcratonvm/README.md:54` says every exported `extern "C"` catches panics,
  but `cratonvm_thread_enter_native` and `cratonvm_thread_leave_native` do not.
- `libcratonvm/include/cratonvm_helpers.h:24` claims C++11 support, but
  variadic macros use C99 compound literals at `:119`, which are not standard
  C++.

Tests/readiness:

- `cargo test -p libcratonvm --lib` passed.
- `cargo package -p libcratonvm --list --allow-dirty` succeeded.
- C examples were not compiled because `clang`, `cc`, `cl`, and `cbindgen` were
  unavailable.

### `cratonvm-embed`

High/medium:

- `cratonvm-embed/Cargo.toml:18` depends on `cratonvm-vm` with default features.
  `vm/Cargo.toml:33` defaults include `awt`, pulling unpublished
  `cratonvm-native-awt`. `cargo package -p cratonvm-embed --allow-dirty` also
  failed because `cratonvm-vm 0.3.0` is not on crates.io yet.
- `cratonvm-embed/src/lib.rs:50` re-exports `Vm`, `SharedVm`, and `JvmThread`,
  while claiming a narrow curated facade at `cratonvm-embed/src/lib.rs:6`.
  Because `SharedVm` and `Vm` expose many internals, much of `cratonvm-vm`
  becomes part of the practical semver surface.
- `cratonvm-embed/src/lib.rs:61` documents `make_string_array` as building
  launcher args, but it uses `create_java_string` at `:71`, which interns
  strings. Normal launcher args should not necessarily compare identical to
  literals by reference. Allocation can abort on heap exhaustion even though
  the helper returns `Result<ObjectRef, VmError>`.
- `field_index_desc` is public at `cratonvm-embed/src/lib.rs:117`, but the
  README helper list omits it (`cratonvm-embed/README.md:37`), as do
  `docs/EMBEDDING.md:201` and
  `docs/book/src/embedding/rust-facade.md:37`.

Tests:

- `cargo test -p cratonvm-embed --lib` passed.
- `cargo test -p cratonvm-embed` passed in the module review.
- Tests are only compile pins; there is no live coverage for string arrays,
  interning identity, `read_string`, field accessors, descriptor-disambiguated
  fields, exception descriptions, or low-heap behavior.

Packaging:

- `cargo package -p cratonvm-embed --allow-dirty --list` passed.
- `cargo package -p cratonvm-embed --allow-dirty` failed because
  `cratonvm-vm` is not in the crates.io index.

### `cratonvm-difftest`

High/medium:

- Packaged crate tests fail. `difftest/Cargo.toml:17` excludes `seeds/**`, but
  a non-ignored unit test expects `CARGO_MANIFEST_DIR/seeds` to be populated at
  `difftest/src/harness.rs:560`. `cargo test` in the package copy fails 51/52.
- `difftest/src/main.rs:290` `--update-ledger` overwrites the ledger with a
  fresh one. `difftest/src/harness.rs:127` marks every divergence `New` and
  drops existing `known`/`fixed` statuses and `linked_doc` values.
- `difftest/src/runner.rs:421` treats `javac` exit 0 as success without
  verifying the expected `.class` exists. `difftest/src/main.rs:547` reuses the
  same predicate directory and class name in minimization, so stale `.class`
  output can falsely preserve a divergence.
- JDK pinning is documented in `difftest/README.md:75` and configured at
  `difftest/src/runner.rs:242`, but gate/run paths do not enforce JDK 25 or
  compare against the ledger JDK.
- Docs/CLI status are stale: `difftest/README.md:15` says `gen`/`min` remain
  stubs, while `difftest/src/lib.rs:14` says Steps 0-7 are complete.
  `difftest/src/main.rs:50` still says "Step 0 scaffold".
- The installed binary is named `difftest` (`difftest/Cargo.toml:24`). The repo
  instruction asks for unique binary names; prefer `cratonvm-difftest`.

Tests:

- `cargo test -p cratonvm-difftest --lib` passed.
- `cargo test -p cratonvm-difftest --test seed_diff -- --ignored --nocapture`
  passed locally, running 3 seeds with 2 agrees and 1 divergence.
- Agent-run `cargo test -p cratonvm-difftest` passed.
- Agent-run `cargo clippy -p cratonvm-difftest --all-targets -- -D warnings`
  passed.
- Agent-run `cargo package -p cratonvm-difftest --allow-dirty` passed, but
  tests in the packaged copy failed.
- `.github/workflows/ci.yml:194` makes the difftest gate advisory with
  `continue-on-error: true`.

### `fuzz`

High:

- `fuzz/Cargo.toml:33` depends on `cratonvm-native-builtins` feature
  `legacy-synthetic-crypto`, but `native-builtins/Cargo.toml:15` does not
  declare it. `fuzz/fuzz_targets/fuzz_tls_record.rs:31` imports the TLS module
  gated by that absent feature (`native-builtins/src/tls.rs:13`). This blocks
  every fuzz target from building.

Medium/low:

- `fuzz/fuzz_targets/fuzz_verifier.rs:34` lacks an input cap, unlike
  `fuzz_classfile.rs:33` and `fuzz_jimage.rs:31`. It can parse arbitrary-size
  class data, allocate boot vectors, create `ClassManager`, and call
  `define_class` at `:54`.
- `fuzz/Cargo.toml:47` through `:110` declare 11 bins, but
  `fuzz/README.md:18` lists only 7. It omits `read_class`,
  `fuzz_instruction`, `fuzz_descriptor`, and `fuzz_verifier`.
- `fuzz/README.md:127` OSS-Fuzz `TARGETS` sketch also copies only 7 targets.
- `fuzz/README.md:57` documents corpora and `fuzz/README.md:81` says minimized
  crashes should be committed, but no corpus/regression/dictionary/options/CI
  hook was found.
- Root `Cargo.toml:6` says fuzz is built via CI, but no `cargo fuzz` CI job was
  found.
- `fuzz/Cargo.toml:18` makes it a standalone workspace, so it does not inherit
  root authors/license metadata, although source SPDX headers are present.

Tests/tools:

- `cargo +nightly fuzz --version` failed because `cargo-fuzz` is not installed.
- `rustup toolchain list` showed nightly installed.
- `cargo +nightly build --manifest-path fuzz\Cargo.toml --bins --locked` failed
  on the missing `legacy-synthetic-crypto` feature.

## Tests review

The test suite is large but not currently adequate for a release claim or an
85 percent coverage claim.

Known passing package-level checks from this review:

- `cratonvm-types`
- `cratonvm-native-api`
- `cratonvm-native-collections`
- `cratonvm-native-io`
- `cratonvm-native-builtins` focused tests (`stub_ratchet`, `aes_gcm_kat`)
- `cratonvm-classloading`
- `cratonvm-jit-api`, including `gpu-lowering`
- `cratonvm-jit-cuda`
- `cratonvm-cuda-bridge`
- `cratonvm-gc`
- `cratonvm-native-awt`
- `cratonvm-gpu` technically passes but runs zero tests
- `cratonvm-jfr`
- `libcratonvm --lib`
- `cratonvm-embed`
- `cratonvm-difftest` source-tree tests, including ignored seed integration

Known failing checks:

- Workspace tests fail to compile in `jit/tests/ir_vs_singlepass.rs:1050`.
- `cratonvm-reader` tests abort by stack overflow.
- `cratonvm-jit` tests fail with the same compile error.
- `cratonvm-cli --bin cratonvm` fails one test.
- `cargo fmt --all -- --check` fails.
- Workspace clippy with `-D warnings` fails.
- `../../../fuzz` bins fail to build.
- `cratonvm-difftest` packaged-copy tests fail.
- `cratonvm-embed` package upload preparation fails until dependency packages
  exist on crates.io.

Adequacy gaps:

- No current whole-workspace coverage percentage exists.
- `../../COVERAGE.md` explicitly says coverage is informational and
  non-blocking.
- `vm` has many ignored/skipped/todo tests.
- `native-awt` has only inline unit tests and no integration/backend tests.
- `craton-gpu` has zero tests.
- `cuda-bridge` CUDA-feature tests run zero tests without CUDA.
- `../../../fuzz` has no corpus/regression assets and no CI smoke.
- Several package manifests exclude fixtures that package tests or integration
  tests still assume.
- Security-sensitive paths need regression coverage: TLS trust roots, agent
  fail-open, path confinement, UDP egress policy, C ABI invalid tags, moving-GC
  handle maps, JFR malformed records, JImage redirect overflow, legacy verifier
  `jsr`/`ret`, and async native memory copy lifetimes.

## Documentation and scripts review

### Global documentation drift

- `README.md:232`, `ARCHITECTURE.md:8`, and `BUILD_GUIDE.md:94` say the
  workspace has 19 member crates. `cargo metadata` reports 20 members. The book
  page `docs/book/src/internals/architecture.md:9` already says 20.
- `README.md:28`, `CONTRIBUTING.md:34`,
  `.github/pull_request_template.md:16`, `docs/PRESENTATION.md:53`,
  `docs/JIT_OPTIMIZATION.md:42`, and
  `docs/book/src/contributing/testing.md:21` still say 6,000+ tests. The static
  count is now much higher, but more importantly the workspace suite currently
  fails.
- `docs/PRESENTATION.md:53` says the codebase is warning-free. It is not.
- `docs/JIT_OPTIMIZATION.md:42` says 0 clippy warnings. Current clippy fails
  with `-D warnings`.
- `CHANGELOG.md:96` through `:98` still describes `../../../fuzz` as a workspace member
  and references 18 workspace members.
- `README.md:26` highlights coverage CI, but `../../COVERAGE.md` says the job is
  advisory and has no threshold.

### CI and scripts

- `.github/workflows/ci.yml:72` leaves synthetic-stub census advisory.
- `.github/workflows/ci.yml:194` leaves the difftest gate advisory.
- `.github/workflows/coverage.yml:39` leaves coverage advisory.
- `.github/_disabled-workflows/ci.yml` and
  `.github/_disabled-workflows/jvm-smoke.yml` remain disabled and should either
  be refreshed or removed from public release materials.
- There is no fuzz CI job despite root `Cargo.toml:6` saying fuzz is built via
  CI.
- Bench performance gates can bootstrap from missing baseline files and are
  non-enforcing by default.

### Release and package docs

- Root `Cargo.toml:12` and `:13` say the only crate withheld from publication
  is `fuzz`. Actual `publish = false` packages are `cratonvm-native-awt`,
  `cratonvm-jit-cuda`, `cratonvm-gpu`, `cratonvm-cuda-bridge`, and
  standalone `cratonvm-fuzz`.
- `../../../RELEASING.md` correctly describes several fenced-off crates, but the publish
  order/list should include newer publishable crates such as `libcratonvm`,
  `cratonvm-embed`, and `cratonvm-difftest`, and it should call out default
  feature edges to unpublished crates.
- `vm-cli` docs should consistently explain that `java` is opt-in through
  `java-bin-alias`; several install/build docs already do, but
  `../../../vm-cli/README.md` is misleading.
- `difftest` docs and CLI long help disagree about which steps are complete.
- `fuzz` docs omit 4 declared targets and describe corpora/regressions that are
  absent.
- `cratonvm-embed` docs omit `field_index_desc`.
- `jfr` docs disagree on stock JMC/JFR compatibility and header size.
- `native-builtins` docs describe undeclared/default-off legacy crypto features
  as default-on.
- `types` docs describe `StringPool` as lock-free despite an `RwLock` design.
- `craton-gpu` README uses the wrong Cargo package selector.

## GitHub/open-source and crates.io readiness

### Existing open-source scaffolding

The repository already has a strong public-project skeleton:

- `LICENSE`
- `NOTICE`
- `../../../THIRD-PARTY-NOTICES.md`
- `../../../SECURITY.md`
- `../../../SUPPORT.md`
- `../../../CONTRIBUTING.md`
- `../../../CODE_OF_CONDUCT.md`
- `../../../GOVERNANCE.md`
- `../../../MAINTAINERS.md`
- `../../../TRADEMARKS.md`
- issue templates
- PR template
- `../../../.github/CODEOWNERS`
- Dependabot
- DCO workflow
- release workflow

`../../../.github/CODEOWNERS` has a default owner of
`@craton-co/cratonvm-maintainers`. For GitHub to request reviews from a team,
that team must have repository visibility/write access and branch protection
must require CODEOWNERS review.

### Apache-2.0 and Craton ownership

Most Rust source files and crate manifests use SPDX `Apache-2.0` and
`Copyright 2024-2026 Craton Software Company`. Workspace metadata also carries
license/authors/homepage/repository values.

Recommended polish before public release:

- Keep the root `LICENSE` as the exact Apache License 2.0 text and put project
  copyright/notice material in `NOTICE` or file headers.
- Verify every package produced by `cargo package --list` includes the intended
  README, license/notice context, and no generated or confidential artifacts.
- Add a release checklist that runs `cargo package --list`, `cargo package`,
  and, for publishable crates, `cargo publish --dry-run` in dependency order.

### crates.io package state

Crates.io name checks returned 404/not-published for all current workspace
package names:

- `cratonvm-reader`
- `cratonvm-types`
- `cratonvm-native-api`
- `cratonvm-native-collections`
- `cratonvm-native-io`
- `cratonvm-native-builtins`
- `cratonvm-classloading`
- `cratonvm-jit`
- `cratonvm-jit-api`
- `cratonvm-gc`
- `cratonvm-native-awt`
- `cratonvm-jit-cuda`
- `cratonvm-gpu`
- `cratonvm-cuda-bridge`
- `cratonvm-vm`
- `cratonvm-jfr`
- `cratonvm-cli`
- `libcratonvm`
- `cratonvm-embed`
- `cratonvm-difftest`

That means names appear available or at least unpublished, but the project is
not ready to publish them:

- Publishing is permanent enough that Cargo docs recommend dry-runs and careful
  package inspection first.
- Default features pull unpublished crates (`cratonvm-vm` -> `awt` ->
  `cratonvm-native-awt`).
- Several crates with `publish = false` are still reachable from default
  dependency graphs.
- `cratonvm-embed` cannot package for upload until `cratonvm-vm` exists on
  crates.io or features are split.
- `cratonvm-difftest` packaged tests fail.
- `libcratonvm` default features pull unpublished `native-awt` through
  `cratonvm-vm`.
- `fuzz` is intentionally unpublished and currently does not build.
- `difftest` binary name is generic and violates the local "unique binaries"
  instruction.

## Performance and robustness improvement themes

Highest-leverage improvements:

- Replace ad hoc JIT stack estimation with verifier/reader `max_stack` data and
  bounds-checked spill layout.
- Add hard allocation caps and fallible allocation paths for AWT image/rendering
  surfaces, G1 arrays, field layouts, native pipes, and fuzz verifier inputs.
- Make moving-GC object handles identity-stable by updating every address-indexed
  map or removing address maps from public/native APIs.
- Make async native memory copy APIs unsafe or enforce lifetime ownership with
  pinned buffers/guards.
- Make security controls fail closed: TLS trust stores, custom trust anchors,
  `-javaagent`, JVMTI agent load, path confinement, and UDP egress policy.
- Turn advisory gates into blocking gates only after they are deterministic:
  fmt, clippy, workspace tests, coverage threshold, difftest, fuzz build smoke,
  and packaged-crate tests.
- Make build scripts write only to `OUT_DIR` and clear generated directories
  before generating new outputs.
- Add package-copy tests for crates that exclude fixtures.

## Suggested feature directions

Foundational/release:

- Create a release train that publishes only leaf crates first, then moves
  upward through `types`, `reader`, `native-api`, `gc`, `jit-api`, etc.
- Split default features so publishable crates do not depend on unpublished GPU,
  CUDA, or AWT crates by default.
- Add a public "known limitations" page generated from in-scope docs, not from
  excluded/private material.
- Add a nightly fuzz CI job that at least builds all fuzz bins and runs bounded
  smoke inputs for cheap targets.

Runtime/security:

- Harden TLS trust management around custom trust stores, root matching, and
  global trust mutation.
- Make agent loading semantics match HotSpot more closely and fail closed for
  security/instrumentation agents.
- Centralize sandbox/path/UDP egress checks so alternate native paths cannot
  bypass policy.
- Finish descriptor-aware and type-checked C/Rust embedding field helpers.

VM correctness:

- Complete precise root coverage for blocked threads, native locals, AWT event
  queues, and flat/native handles.
- Finish deopt virtual-object materialization or gate scalar deopt until it is
  safe.
- Retire legacy verifier fallbacks or make them instruction-boundary precise.
- Make JFR dump parsing fully bounded and stock-tool compatibility explicit.

Product/API:

- Rename `difftest` binary to `cratonvm-difftest`.
- Turn `cratonvm-embed` into a real stable wrapper API rather than re-exporting
  broad `cratonvm-vm` internals.
- Add a `run_main` convenience, static-field helpers, exception message
  extraction, and uninterned launcher-argument helpers.
- Finish AWT event delivery and decide whether the project is headless-only or
  will support real Win32/X11/Cocoa backends.
- Stabilize GPU annotation/source artifact handling before advertising GPU
  crates beyond experimental use.

## External references used

- Cargo publishing reference:
  https://doc.rust-lang.org/cargo/reference/publishing.html
- Cargo manifest license fields:
  https://doc.rust-lang.org/cargo/reference/manifest.html#the-license-and-license-file-fields
- GitHub CODEOWNERS reference:
  https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/about-code-owners
