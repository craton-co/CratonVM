# JDK-only mode — blocker inventory by service area

| | |
|---|---|
| **Status** | OPEN. Nothing in the P0 table is closed. Wave 1 is measurement; see [`feature-designs/jdk-only-mode.md`](feature-designs/jdk-only-mode.md) §10. |
| **Normative source** | [`feature-designs/jdk-only-mode.md`](feature-designs/jdk-only-mode.md). This file tracks *work*, not semantics. |
| **How rows are generated** | The evidence column cites code that exists today. The per-JDK exhaustive list is machine-generated — see [`jdk-only-audit.md`](jdk-only-audit.md) §6. |

No static review can produce a genuinely exhaustive missing-method list across
"any JDK": native surfaces and class-library internals vary by feature version.
The tables below are the **confirmed inventory visible in this repository**, and
therefore the *minimum* scope that must be resolved or explicitly excluded. They
are not a ceiling.

## Closure rule

An entry closes only when it is one of:

1. implemented as a **reviewed bridge** (`NativeKind::Bridge`, reviewed per
   [`jdk-only-native-review.md`](jdk-only-native-review.md));
2. implemented as a **reviewed intrinsic** (`NativeKind::Intrinsic`, with
   differential parity evidence);
3. **executed from real bytecode** (the native is deleted from the strict path);
4. implemented as **legitimate generated class bytes** with a non-`CompatibilityStub`
   `ClassOrigin`; or
5. **explicitly out of scope**, failing with a specification-consistent error
   (`ClassNotFoundException` / `NoClassDefFoundError` / `UnsupportedOperationException`
   / a documented platform error) rather than a fabricated success.

"It stopped appearing in the census" is **not** closure. A symptom can disappear
because a ban was added, a call path changed, or the vector stopped exercising
it. Closure requires naming which of the five above applies, and a test.

### Status vocabulary

| Status | Meaning |
|---|---|
| `OPEN` | Not started, or in progress with no landed change. |
| `MEASURING` | Instrumented; the census names the instances but nothing enforces. |
| `ENFORCING` | `--jdk-only` refuses it; `--real-jdk` unchanged. |
| `CLOSED(n)` | Closed under closure rule *n* above, with the test named. |

---

## P0 — strict-mode blockers

These prevent `--jdk-only` from meaning what it says. All are `OPEN` or
`MEASURING` as of wave 1.

| Pri | Item | Status | Current evidence | Required resolution |
|---|---|---|---|---|
| P0 | **Residual synthetic native set** | MEASURING | `native-builtins/tests/stub_ratchet.rs` freezes `BASELINE_SYNTHETIC_STUBS = 157` in the default real-JDK registry. `vm/src/vm/vm_init.rs` records that a 2026-07-14 attempt (`d8092acb`) to drop them all was reverted the same day: several `register_*` clusters tagged `SyntheticStub` are **permanent bridges carrying the wrong tag**. | Classify every entry as bridge, intrinsic, compatibility shim or dead code. Strict final count must be **zero**. One subsystem per PR. Do not repeat the global drop. |
| P0 | **Synthetic class fallback policy** | OPEN | `ClassManager::load_class` (`classloading/src/class_manager.rs`) falls back to fabricating an empty class after lookup failure, gated by `is_enterprise_stub_prefix` (`org/jboss/`, `io/quarkus/`, `io/smallrye/`, …). Its own comments record the two failure modes this causes: false-positive `Class.forName` capability probes, and runtime-generated `$$` classes being blocked by a bogus stub already in the store. | Under `JdkOnly`, return the specification-appropriate `ClassNotFoundException`/`NoClassDefFoundError` and record `CompatibilityClassRequested`. Never fabricate a non-array class. Categorical, not prefix-by-prefix. |
| P0 | **Direct `ensure_synthetic_class` calls** | OPEN | `ClassManager::ensure_synthetic_class` creates shim classes that dispatch through native registrations. It also carries an in-place *upgrade* path because objects may already have been allocated against an undersized synthetic layout. | Split legitimate generated classes from compatibility stubs at the API boundary. The upgrade path must call `Class::set_origin`. Reject compatibility creation under strict policy. |
| P0 | **Native-first dispatch and hard-coded overrides** | OPEN | `invoke_or_native` (`vm/src/vm/vm_exec.rs`) consults the registry before ordinary bytecode dispatch. `real_protected_stub_class` (`vm/src/runtime/interpreter/invoke.rs`) is a hand-maintained class allowlist deciding when a `SyntheticStub` must yield to real bytecode — `ReentrantLock`, `LinkedBlockingDeque`, `AtomicBoolean`, `EnumSet`, `Instant`, `ZonedDateTime`, `FileInputStream`, `Cleaner`, `Cleaner$Cleanable`, `ManagementFactory`. | Make dispatch structural via `resolve_dispatch` (contract §7): concrete bytecode wins unless the entry is a reviewed intrinsic; `ACC_NATIVE` binds to a bridge. Delete the class-name lists. Wave 1 funnels them through the resolver and tags each `// JDK-ONLY-WAVE2:`. |
| P0 | **Duplicate dispatch implementations** | OPEN | Two independent override gates exist and **disagree**. `force_native_over_real_jdk_bytecode` (`vm/src/runtime/interpreter/invoke.rs`) returns `false` early for `java/lang/String` except for a short regex/`substring`/charset-ctor list. `invoke_or_native` (`vm/src/vm/vm_exec.rs`) carries the *opposite* rule: a 21-method `java/lang/String` force-native list. The comments in each acknowledge the other and ask that they be "kept in sync" by hand. | Centralise all native resolution in one policy-aware resolver used by interpreter, JIT, reflection, JNI and method handles. A lint/test must forbid direct registry lookup outside it. |
| P0 | **Class-origin observability** | OPEN | `Class` carries a single `is_synthetic_stub: bool` (~165 read sites across the tree). It cannot distinguish boot-image bytes, application bytes, arrays, hidden classes, generated proxies and fabricated skeletons. | Land `ClassOrigin` (contract §5) with `is_synthetic_stub` as a derived mirror. Expose via `--dump-class-origins`. Do **not** delete the bool this wave. |
| P0 | **`Function.identity()`** | OPEN | `classloading/src/class_manager.rs` contains a dedicated `java/util/function/Function$Identity` stand-in with its own field table and an assignability test. `vm/src/vm/vm_init.rs` records that dropping it yields `UnsatisfiedLinkError` because **there is no real bytecode to fall back to** — the real `Function.identity()` is a lambda, so this is a hole in the metafactory path, not in `Function`. | Fix the lambda/metafactory path to produce a real generated implementation, or represent the method-handle-backed lambda through a distinct legitimate generated-class origin. Not a general lambda solution today. |
| P0 | **JMX real path** | OPEN | `vm/src/vm/vm_init.rs` documents the confirmed regression: with stubs dropped, WildFly's first `ManagementFactory.getPlatformMBeanServer()` NPEs deep inside real `javax.management` bytecode with `ObjectName._ca_array` null. The whole `native-builtins/src/jmx.rs` surface (`register_jmx_natives`, `register_vm_management_impl`, `register_management_factory_platform_server_stub`, `register_mbean_server_factory_synthetic`, `register_thread_impl`, `register_class_loading_impl`) is affected. | Run real `java.management` / `javax.management` bytecode; implement only the required VM-native leaves plus reflection and class-definition services. Reclassify the genuine bridges to `NativeKind::Bridge` rather than deleting them. |
| P0 | **Real boot-image requirement** | OPEN | The launcher's real mode already requires a valid JDK and does **not** silently fall back — `--java-home` pointing at a nonexistent path fails loudly (`vm-cli/src/main.rs`, and its own tests assert this). | Reuse `require_real_jdk`. Under `--jdk-only` the error must additionally name `--jdk-only`, the searched paths, and the accepted JDK layout (contract §8). |

---

## P1 / P2 — runtime services preventing broad real-class execution

| Pri | Service area | Status | Confirmed gap or overlay | Concrete target |
|---|---|---|---|---|
| P1 | **Core `String` dispatch** | OPEN | `vm/src/vm/vm_exec.rs` carries a comment-dated (`RKC16N.6 RECON, Session 94`) forced-native list for `charAt`, `length`, `isEmpty`, `equals`, `hashCode`, `indexOf`, `lastIndexOf`, `substring`, `startsWith`, `endsWith`, `trim`, `toString`, `concat`, `replace`, `toLowerCase`, `toUpperCase`, `compareTo`, `compareToIgnoreCase`, `equalsIgnoreCase`, `contains`, `split` — because real `java/lang/String` bytecode resolution was failing during JDK clinits such as `StandardCharsets.<clinit>`. The comment says "Drop when RKC16N.6 lands a permanent fix." A separate `java/lang/StringUTF16.getChars([BII[CI)V` override exists for compact-string copy cost. | Repair method resolution and compact-string layout handling. Retain only measured, semantics-preserving string intrinsics. Remove one method cluster per PR, grouped by root cause. |
| P1 | **Thread and executor semantics** | OPEN | `invoke_or_native` special-cases `java/util/concurrent/ThreadPoolExecutor.execute` on the **shape of the receiver** — whether a synthetic/native worker representation is present — to decide whether real bytecode runs. The companion check appears again further down the same file and in `check_override`. | Give real `ThreadPoolExecutor` objects correct Java field initialisation and execute their bytecode. No receiver-shape heuristics. |
| P1 | **ForkJoin worker execution** | OPEN | `native-builtins/src/lib.rs`: "ForkJoinPool worker threads do not run Java bytecode", with an eager-inline policy for `ForkJoinTask.fork`/`invoke`. `native-builtins/src/concurrent_extras.rs` eagerly invokes `runnable.run()` for `ForkJoinPool.execute`, and `phases_early.rs` computes `submit(ForkJoinTask)` inline. `concurrent.rs` returns a synthetic `ForkJoinWorkerThreadFactory`. | Real VM thread attachment, task execution, parking, interruption, work-stealing coordination, and GC-safe roots for real worker threads. Eager-inline changes observable ordering and is not a bridge. |
| P1 | **Concurrency + GC cooperation** | OPEN | `regression-suite/run.sh` excludes `RConcurrent` from the default set by name, with the reason inline: heavy multi-threaded execution intermittently trips cross-thread JIT-frame root scanning at a STW GC pause. `ROADMAP.md` lists `java.util.concurrent` parity as unresolved. | Complete cross-thread safepoints and root scanning, monitor/park semantics, AQS/ForkJoin/Phaser behaviour, and non-daemon worker shutdown. Then move `RConcurrent` into the default set — that move is the closure test. |
| P1 | **`ProcessHandle`** | OPEN | `is_native_backed_jdk_stub` (`classloading/src/class_manager.rs`) explicitly allows `java/lang/ProcessHandle` and `java/lang/ProcessHandle$Info` to be fabricated when boot bytes are unavailable, with a hand-written method table. | Under strict mode load the real classes and supply the platform-native leaves: enumeration, PID, liveness, parent/children, timing, termination. |
| P1 | **Reflection and generated accessors** | OPEN | `classloading/src/loaders.rs` holds privileged-package guards with documented namespace exemptions so genuine JDK-generated reflection/serialization accessors can be defined. `README.md` still describes reflection and JNI as covering "the common paths" with partial edge cases. | Complete `Lookup.defineClass`, hidden classes, reflection inflation/accessors, serialization constructors, access checks and loader identity. Origin-based authorisation, not prefix-only rules. |
| P1 | **JNI and native binding** | OPEN | `README.md`: JNI edge cases (C-varargs forms, full foreign-thread attach) are partial. Strict mode depends on correct native *binding* rather than method substitution. | Complete `RegisterNatives`, symbol resolution, local/global references, exceptions, thread attach/detach, critical arrays and strings, native library lifecycle. |
| P1 | **JPMS / module semantics** | OPEN | The CLI exposes module-path and add-reads/exports/opens options; `ROADMAP.md` lists "Module system: full JEP 261 resolution semantics" as unresolved. | Boot-layer construction, readability, exports/opens, module ownership, resources, services, loader/module identity. `--dump-missing-natives-grouped` already groups by module and is the natural progress metric. |
| P1 | **NIO, files, networking** | OPEN | `vm/tests/synthetic_diff.rs` covers selected RandomAccessFile and socket subsystems via per-subsystem `REAL_*` gates; broader I/O remains a mix of real bytecode and bridges. | Stabilise file descriptors, mapping, selectors, asynchronous close, interrupts, socket options, DNS, and Windows/Unix provider differences. |
| P2 | **JDBC / `java.sql`** | OPEN | `README.md`: "JDBC / `java.sql` is not wired yet." | Load the real module; support driver discovery and the networking/filesystem/JNI behaviour it needs. No synthetic driver or API classes. |
| P2 | **Crypto provider completeness** | OPEN | `README.md`: cryptography is "best-effort and not constant-time everywhere"; see [`CRYPTO_STATUS.md`](CRYPTO_STATUS.md). | Real provider discovery and class code; entropy, native library and platform keystore boundaries as bridges; differential-test algorithms *and* exception types. |
| P2 | **Headful AWT** | OPEN | `README.md`: "AWT/Swing are headless (no on-screen rendering); JavaFX is out of tree." | Declare JDK-only scope as **headless** initially (closure rule 5), or separately implement platform windowing/font/input services. Never fabricate AWT classes. |
| P2 | **Verifier coverage** | OPEN | `ROADMAP.md`: bytecode verifier completeness for pre-Java-7 class files is an open hole; reaching 100 % of the split-verifier corpus without `--noverify` is a listed goal. | Declare supported class-file versions explicitly and reject unsupported bytecode accurately. Verification must never be bypassed by routing through a synthetic class. |

---

## Working notes

**The 157 are not homogeneous.** This is the single most important fact in this
document and it is a repository claim, not a projection: `vm/src/vm/vm_init.rs`
records a same-day revert of a global stub drop, naming JMX and
`Function$Identity` as permanent bridges that merely carry the wrong tag. Any
plan that treats "157 → 0" as a deletion exercise will reproduce that
regression. The path is **reclassification first**, deletion second.

**Compatible mode must be byte-for-byte unchanged.** Every row above is scoped
to `--jdk-only`. `--real-jdk` (the default) keeps today's behaviour, and that is
checked by the existing regression suite plus the stub ratchet.

**Wave-1 enforcement is narrow.** Only class fabrication (contract §5) and stub
registration (contract §4) enforce. Everything else in these tables is recorded
and counted. Do not read a green `--jdk-only` wave-1 run as evidence that a row
is closed.

**Per-JDK generation.** The authoritative instance list for a given image comes
from `target/jdk-only-audit/jdk-<feature>-missing-natives.json` and
`…-synthetic-dependencies.json`. See [`jdk-only-audit.md`](jdk-only-audit.md) §6.
