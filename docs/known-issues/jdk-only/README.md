# JDK-only mode — open defects

**Status:** OPEN, **85 records**. Index rebuilt from the tree on **2026-08-12**
by W7-78-inherited-residual-closeout.md, on top of the reconciliation pass
W7-55-record-reconciliation.md. Six records left the directory that day —
four retired by RETIREMENT-20260812.md, and two (`W3-6`, `W5-2`) whose `git mv`
had simply never been done although both records and this index already said
RETIRED. Earlier reductions 2026-08-04, 2026-08-06 and 2026-08-11 (thirty
records, `RETIREMENT-20260811.md`). Filed 2026-07-31 from wave-1 implementation
findings.

> **Read this before you take any record from §2.**
>
> **Rule 1 — do not believe a "not applied".** Grep for a literal from the patch
> body first. `RETIREMENT-20260811.md` found fourteen records wrong about this;
> W7-55 found **eighteen more**, one of which was handed out as pending work
> *twice* after landing in full. `git log -S'<literal>' --oneline` settles it in
> one command.
>
> **Rule 2 — do not apply a patch block without checking §2.4.** Nine records
> prescribe a fix that is now wrong, and applying one does damage rather than
> merely wasting time. All nine are now marked DEAD at the patch block itself,
> not merely in this index — five of them only as of 2026-08-12.
>
> **Rule 3 — do not retire a record on a green headline.** `W4-2`'s
> urgent-looking rows were all stale while its quiet one (array classes report
> module `java.base`) was live. On 2026-08-12 the same shape appeared again:
> `W6-2` was nominated as fully closed, and the row that held it back was one
> nobody had ever recorded — see §2.2.
>
> **Rule 4 — a green vector closes a headline, not a record.** Several rows in
> §2.2 are residuals their own vector has never exercised, by construction.

## 1. What jdk-only mode is, and where the contract lives

`--jdk-only` (`CompatibilityMode::JdkOnly`) is the strict mode: CratonVM runs
the real JDK image's own bytecode and **refuses** the compatibility layer that
`--real-jdk` (`Compatible`, the default) admits. It is a *runtime* mode, not a
Cargo feature — `--features synthetic-jdk` is a third, separate configuration
that builds a VM with no class library at all. **Conflating the feature with the
mode is a defect species in its own right**: it produced all six of
W7-50-synthetic-jdk-strict-six.md's findings.

* **Normative contract:**
  [`docs/feature-designs/jdk-only-mode.md`](../../feature-designs/jdk-only-mode.md)
  — owned by the orchestrator; do not edit.
* **Read the mechanism facts before anything else:**
  [`docs/architecture/natives-over-real-jdk-classes.md`](../../architecture/natives-over-real-jdk-classes.md).
  How a native actually comes to run instead of real JDK bytecode (**not** the
  "four doors" rule several records still state), why a Cargo feature is not a
  runtime mode, `register()`'s last-registration-wins semantics, what a by-name
  field read cannot report, why a slot index against a real layout is heap
  corruption rather than a wrong answer, and the measurement rules.
* **This directory is the evidence base.** One record per defect. A record moves
  to the internal record tree **when it is fixed**, not when it is planned.
* Related non-known-issue docs:
  [`docs/jdk-only-runtime-services.md`](../../jdk-only-runtime-services.md),
  [`docs/jdk-only-native-review.md`](../../jdk-only-native-review.md),
  [`docs/jdk-only-migration.md`](../../jdk-only-migration.md).

### The corpus, and what a green corpus licenses

On 2026-08-11 the strict suite was remeasured and closed at **68 passed, 0
failed**; Compatible was 41/0. On 2026-08-12 five inherited records' vectors were
re-run individually on the dev binary at `ba65f1a19` and all five pass in
**both** runtime modes: `RJdkSecurity` 61 checks (L8, W4-3), `RJdkFailure` 43
(L16), `RJdkJni` 35 (W5-1, W6-6), `RJdkModule` 44 (W2-3, W4-2, W6-2). A
`--features synthetic-jdk` binary was also built and measured for the first
time — **63 passed / 7 failed** under `--jdk-only`
(W7-50-synthetic-jdk-strict-six.md).

**What that does and does not license.** A record whose corpus vector is in
`JDKONLY_CLASSES` (`regression-suite/run.sh`) has its "unverified by execution"
caveat discharged **at the vector level**. It is not a per-assertion audit. Four
worked examples of the gap, all from this directory:

* **W2-3** is at 44/44 and its `isAutomatic()` check passes **vacuously** —
  `isAutomatic()` is a hardcoded `false` and the vector never asserts otherwise.
* **W6-8**'s headline fix is exercised by **no vector at all**.
* **W6-2** is at 44/44 while a subtype check is missing on one of its two
  provider paths, because the fixture's provider is a *correct* subtype and only
  the positive case is ever walked (§2.2).
* **L15**'s field narrowing was landed and unexercised until 2026-08-12, because
  every field assertion in `RJdkReflect` called `setAccessible(true)` first.

**If you re-measure, pass `--java-home`.** `run.sh` gives every CratonVM
invocation one; a hand-run that omits it measures the host's default JDK instead
of the JDK 25 image, which on this host inverted the per-mode verdict for
`RJdkModule`. Trace the real command rather than reconstructing it:
`ONLY="RJdkModule" bash -x regression-suite/run.sh 2>&1 | grep <binary>`.

---

## 2. What is still open — naming the LIVE residual, not the headline

Grouped by what a taker needs.

### 2.0 Records that left this directory on 2026-08-12

> The section that used to sit here — *"Records that are FULLY CLOSED and should
> be retired"* — **was corrupted by a bad merge** and listed genuinely-live
> records as retirable, with a "Why it is closed" column that actually described
> an open residual. `W6-2` appeared in it twice, once as open and once as
> closed. It has been deleted. If you are working from a remembered row of it,
> re-read §2.1/§2.2; the row you remember was probably live.

| Record | Where it went, and why |
|---|---|
| `W7-4-differential-probe-widening-round-2.md` | RETIRED — deliverable discharged by W7-32 and acted on by W7-33/36/37/40/42. **Its 540-line HotSpot oracle is stale; diffing against it manufactures divergence.** |
| `W7-11-strict-baseline-remeasured.md` | RETIRED — closed at 68/0 the day it was written; all four named defects landed. |
| `W7-28-preview-classfile-gating.md` | RETIRED — all four handback parts applied. Its part D was settled by finally running the command it named: both `bench-tornado` class files are `ca fe ba be 00 00 00 45`, i.e. **not** preview-stamped. |
| `W7-32-round-2-differential-run.md` | RETIRED — pure measurement, superseded (by **W7-42**, not W7-40, which is itself superseded). |
| `W3-6-processimpl-missing-natives.md` | Already self-declared RETIRED (W7-46) and indexed as such, but never moved. Move completed; its last named live row (route Windows `destroy()` through `signal_pid`) was re-checked and **is** routed, `native-io/src/process.rs:1830` → `:1029`. |
| `W5-2-two-silently-skipped-process-checks.md` | Same — declared RETIRED (W7-46), never moved. Move completed. This is what makes `W6-10`'s "its target record was retired out of this directory" row true at last. |

Full evidence per record: `RETIREMENT-20260812.md`.

### 2.1 Out-of-file patches that are GENUINELY still unapplied

Re-grepped 2026-08-12. Each of these is a real, appliable change. **Check §2.4
before applying anything from a record that also appears there.**

| Record | The unapplied work |
|---|---|
| `W6-8-method-invoke-exports-gate.md` | `Field.get`/`Field.set` over-deny public fields of exported-but-not-opened packages; the entire `Lookup.unreflect*`/`find*` family has no **module** check (the *mode* check landed in `3644142d5`). HotSpot throws there — measured. |
| `W2-3-module-descriptor-answers-empty-sets.md` | All four parts, nothing exists: `main_class` on `classloading::module::ModuleDescriptor` + `ModuleMainClass` parsing; six `NativeContext` accessors; their `ModuleRegistry` impls; consuming them in `build_module_descriptor`. `modifiers()`, `Requires.compiledVersion()`, `version()`, `rawVersionString()`, `mainClass()` have **no data source**, and `RJdkModule` asserts none of them. |
| `W2-1-strict-refuses-the-synthetic-stream-stack.md` | The stream stack is **SPLIT**. The record carries the inventory and a four-step staged path to a real `java.util.stream`; step 1 has since landed, so the path is now walkable. |
| `L8-securerandom-provider.md` | Delete the three shadowing `SecureRandom` registrations in `native-builtins/src/crypto_impl.rs` whose no-op bodies undo SHA1PRNG **reseeding**. Synthetic-jdk only. **The record's older "Out of scope" framing blames the discarded constructor seed — that is the wrong row; see §2.4.** |
| `W2-2-blocked-reader-async-close-wakeup.md` | Make `poll_stream_readable` `pub` (`native-io/src/net.rs:2421`) and collapse the three duplicated `re1_socket_poll_readable` arms. Idiom cleanup, no behaviour change. |
| `W4-3-security-getalgorithms-short-list.md` | Patches A (unmodifiable sets), B (`MD2` advertised, unimplemented), C (SHAKE), D (the silent SHA-256/32-byte digest defaults), F (`SUN`/`KeyFactory`/`ML-DSA`). **Same five defects as W7-29's residuals 1–5 — fix once. Patch E is DEAD, §2.4.** |
| `W5-1-loadlibrary-allowlist-too-wide.md` | Arm `BootLoader.loadLibrary` (`native-builtins/src/lib.rs:14049`; `record_boot_loader_library` at `lang_system.rs:3183` has **zero** callers); fix the Compatible-mode `Runtime.load0`/`loadLibrary0` argument index; return the resolved path from `load_native_library`. **The arming needs a measurement first — §2.6.** |
| `W6-10-process-enumeration-syscall-cost.md` | One inventory row. Its target record left the directory on 2026-08-12, so the row now belongs to `W7-46-process-cluster.md`. |
| `W7-5-registrars-that-never-shipped.md` | The §6.3 wiring ratchet test was never written, so the regression that produced this record can recur silently; `register_concurrent_skip_list_map_natives` is still neither wired nor deleted. |
| `W7-9-minted-interface-abstract-methods.md` | §8.1 (delete the class-blind `forEachOrdered` hack — correctly still blocked on its precondition), §8.2 `Selector.provider()` (blocked on a `NativeContext::invoke_static`), §8.3 the four `DatagramChannel` residuals (blocked on unifying two synthetic layouts). |
| `W7-10-processhandle-interface-stub-bodies.md` | §7.3: add `commandLine` to the `ProcessHandle$Info` arm of `classloading/src/class_manager.rs`, without which §4's registration is real-JDK-only. |
| `W7-14-fjp-common-factory-bound-by-name.md` | An explicit **human decision**, not a patch: under `--real-jdk`, `commonPool().getFactory().getClass().getName()` still answers a class JDK 25 does not declare. |
| `W7-15` · `W7-21` (crypto) | The two 2-arg `KeyGenerator.getInstance` overloads still hardcode 128 bits and admit any name, diverging from the fixed 1-arg path; `SecretKeySpec` still accepts an empty/null key. |
| `W7-18-structured-task-scope-jep505.md` | Patches B and C. **B is the dangerous one:** `native-builtins/src/jdk25_concurrency.rs` still models the JDK-21 shape, runs last, and owns every shared triple, so it can silently re-impose that shape over this record's fix. **Patch A's "preferred" form is DEAD — §2.4.** |
| `W7-20-refusal-laundered-into-wrong-answer.md` | **Half settled by W7-62.** The kind map is re-frozen. `jdk-only-bridge-ratchet.json` still fires and is deliberately left firing with its derived movement in its `note`; it needs a census. A **third** stale ratchet nobody had recorded, `native-builtins/tests/stub_ratchet.rs` (+6), is also firing. |
| `W7-22-shadow-retirement-logging-and-time.md` | The 7-row `java/io/Print*` retirement never landed — `native-api/src/retired_shadow.rs:176` is still `java/util/logging/`-only. 29 `PrintStream` rows stay blocked on real `PrintStream` state. **§4's named cause and its repair are DEAD — §2.4.** |
| `W7-27-thread-exit-java-cleanup.md` | §10C: the main/primordial thread never gets `Thread.exit()` — `run_thread_exit_shared` has only two call sites, both worker-death paths. |
| `W7-29-jca-advertise-implement-gaps.md` | Residuals 1–5. **Same five as W4-3's A/B/C/F seen from the other end.** |
| `W7-30-stub-ratchet-boot-path-scope.md` | Two follow-ups, neither started: move the gate to `vm/tests/stub_ratchet.rs` (no such file), and collapse the duplicated boot-path model into a shared `native-builtins/tests/common/` (no such directory). |
| `W7-34-formatter-family-residuals.md` | The Formatter-locale patch: **both** registrars unchanged. Note the record's own trap — `java/util/Formatter` has **two** registrars and the last one wins, so any patch here needs a `--dump-native-registry` before/after diff. |
| `W7-37-differential-throwable-and-vm.md` | Four out-of-file items: carry two `ClassId`s on `RuntimeError::ClassCastException` instead of a pre-rendered string; route `vm/src/jit/helpers.rs`'s direct `ArrayStoreException` mint through the funnel; stop the four raise sites rebuilding text downstream. |

### 2.2 Live residuals inside an otherwise-fixed record

The headline is closed and the vector passes; a specific sibling case is not.

| Record | The live residual |
|---|---|
| `L15-nestmate-access-field-and-constructor.md` | `Constructor.newInstance` (`native-builtins/src/lang_class.rs:11017`) has **no member-modifier gate at all**, so a private constructor is reachable without `setAccessible(true)` — closing it is a **narrowing** whose blast radius is every reflective instantiation in the corpus. Also: hidden classes are never nestmates (`NativeContext::is_hidden_class` does not exist), which fails **closed**. **The missing vector is closed as of 2026-08-12** — four nestmate field checks added to `RJdkReflect`, 60 → 64, HotSpot-verified, never yet run on CratonVM. |
| `L16-classnotfound-vs-noclassdeffound-shapes.md` | Our `ClassNotFoundException` names the array **descriptor**; HotSpot names the **element**. `native_class_for_name` still hands descriptors to `loadClass`. `RJdkFailure` does not assert the message, which is why 43/43 does not close it. |
| `W2-1-strict-refuses-the-synthetic-stream-stack.md` | Seven `NO_IMAGE_JDK_RECEIVERS` names minted outside `native-collections` were never probed; `StreamChainCollector` is still an unguarded `try_alloc_synthetic`. **Residual 2 is CLOSED and its diagnosis was wrong — §2.4.** |
| `W2-2-blocked-reader-async-close-wakeup.md` | A **fourth** surface of the same species, PLAUSIBLE not confirmed: `native-builtins/src/phases_early.rs:18236-18320` parks in a bare `read_retry_eintr` with no close-awareness and maps `Ok(0)` to `-1`. Probably synthetic-jdk-only; one `--dump-native-registry` settles it. Superseded as a *family* by `W7-53-blocking-close-family.md`. |
| `W3-4-forkjointask-status-flags-and-the-eager-default.md` | The eager-fork flip's blast radius on the Spring/H2 slice is unverified — and the two Rust guards for the now-non-default lazy path are **vacuous** (`apps/fjp_probe/` does not exist, so both tests early-return). |
| `W4-1-publiclookup-allowedmodes-never-checked.md` | Four unit tests aimed at code no live path reaches (`native-builtins/src/classloader.rs`), green and guarding nothing. Its dead 332-line block was deleted by W7-62. **Two of its own claims are struck — §2.4.** |
| `W4-2-unnamed-accessor-bypasses-encapsulation.md` | Array classes report module `java.base` regardless of component type — `classloading/src/class_manager.rs:9568`, unconditional. **The only live item, and `RETIREMENT-20260811.md` does not mention it.** |
| `W6-2-module-serviceloader-provider-factory.md` | **Two rows, both found 2026-08-12 while adjudicating this record for retirement.** (1) `service_accepts_type` (`native-builtins/src/service_loader.rs:1677`) has exactly **one** caller, at `:1897` — the **iterator** path. The **stream** path (`:2401-2420`) computes `factory_return_type` and never asks it, so an illegal module-declared factory provider raises `ServiceConfigurationError` from `iterator()` and is quietly handed out by `stream()`. 44/44 cannot see it: the fixture's provider returns a *correct* subtype. (2) The constructor-form subtype check is absent on both constructor paths — downgraded from "argued refusal" to "deferred for want of a measurement", since its stated reason is that the lane could not measure it. |
| `W6-6-nativelibraries-load-fabricated-success.md` | The boot-loader case cannot fire on either road, because `BootLoader.loadLibrary` is a no-op and `record_boot_loader_library` has zero callers. W5-1 owns the arming, and it **needs a measurement** (§2.6). Recorded 2026-08-12: that no-op's `NativeKind` is **ambient** and must stay `Bridge` — a `SyntheticStub` there would be dropped under `--jdk-only`, restoring the Linux boot-class native-library lock the short-circuit exists to avoid. |
| `W6-8-method-invoke-exports-gate.md` | `unreflectSetter` on a trusted-final field is unchecked; the module half of `find*`/`unreflect*` is absent by design; `unreflectSpecial`'s `specialCaller` conjunct is unenforced; **and no vector asserts the positive**. |
| `W6-12-stampedlock-split-brain.md` | The `Collections` fidelity residual, **structurally confined to `--synthetic-jdk` by construction** — `phases_early`'s identity bindings reach the registry only through `lib::register_synthetic_overrides`, which is a `#[cfg(not(feature = "synthetic-jdk"))]` no-op shim (`vm/src/native/builtins.rs:29`), so they are *compiled out* of both shipping binaries rather than out-voted. Order would not have saved it — their ambient kind is `Intrinsic`, which `JdkOnly` does **not** drop. A feature binary now exists (W7-50, 63/7); what has never been run is one in the `--synthetic-jdk` **mode**. The `Phaser` fix is unproven for the same reason. |
| `W7-1-treemap-views-and-iterator-remove-contract.md` | Families 3 and 4 untouched; `sort`/`replaceAll` do not bump `modCount`; `native_map_key_itr_next` returns null past the end instead of `NoSuchElementException`; the view cache has no version stamp. |
| `W7-2-primitive-stream-terminal-surface.md` | §7.2's `DoubleStream`/`LongStream` holes — `anyMatch`, `reduce`, `findFirst`/`findAny`, `sorted`, `distinct`, `spliterator` — never written. |
| `W7-3-format-conversions-and-stringbuilder-bounds.md` | `append(CharSequence,int,int)` still clamps (and a test **pins** the clamp); `appendCodePoint` truncates; three `insert` overloads have no native; `%a` with the `0` flag and a width is wrong. |
| `W7-8-fabricated-success-io-sweep.md` | `FileChannel` natives lack the real-instance guard `close`/`isOpen` carry (**check this first when the branch is built**); `RandomAccessFile.writeUTF` writes plain UTF-8, not modified UTF-8; two `.max(0)` timeout laundering sites; `Files.isSameFile` ≈ `Path.equals`. |
| `W7-12-strict-annotation-proxy.md` | R2, the resolution-1 redesign. |
| `W7-13-strict-mh-insert-wrapper.md` | A stale §9 row in `docs/architecture/natives-over-real-jdk-classes.md`; the neighbouring `classloader.rs::lk_previous_lookup_class` slot-2 claim was never checked. |
| `W7-16-arraydeque-and-linkedlist-residuals.md` | **CLOSED IN SOURCE by W7-62, not rebuilt.** The `jdk_interfaces` arm is applied and CLOSES the `ClassCastException` rather than moving it. Instrument: `probes/ListItrInterfaceProbe.java` plus a HotSpot 25 control transcript. |
| `W7-17-vm-internal-door-sweep.md` | §8's "what this record does not fix", plus the optional `fabricated_origin_for_name` arm for `CratonVM$…` names. |
| `W7-19-methodhandles-compatible-residuals.md` | `bindTo` does not raise `ClassCastException` for a wrong reference type; `isVarargsCollector()` answers `false`; `type()` is not narrowed after a getter/array-getter bind. |
| `W7-23-thread-container-registration.md` | The interlock is still `false` by default (`native-builtins/src/shared_secrets_bridge.rs:745`). Its blocker is **gone** — the de-registration half landed via W7-27 — so this is now flip-and-measure, not blocked. |
| `W7-24-httpserverloop-and-strict-fallbacks.md` | `cratonvm/net/HttpBodyReplaySubscription` (§4, left loud) and the two `SSLSocket*Stream` sites with their twins in `phases_late/ssl_security.rs`. |
| `W7-25-jul-getlogger-regression.md` | The `Supplier` convenience overloads evaluate a suppressed supplier; `LogManager.getLogger` demand-creates for an undemanded name; `log(LogRecord)` is not level-gated. |
| `W7-26-getannotation-swallowed-exception.md` | Twelve loader ladders still catch **any** exception from a user loader's `loadClass`; five sites re-raise as the wrong type; the two swallow-shape scans covered one file only. |
| `W7-31-enable-preview-wiring.md` | Only the `<Unknown>` vs `""` nameless-define distinction, which §3.1 argues is not worth doing. |
| `W7-33-differential-dead-sections.md` | The synthetic-mode `EmptyStackException` follow-up in `classloading/src/class_manager.rs`. |
| `W7-35-jul-supplier-and-payload-residuals.md` | One survivor: the `--jdk-only` half of #59 — see `jul-logrecord-infercaller-is-inert-under-jdk-only-20260812.md` and W7-56. |
| `W7-36-differential-view-families.md` | The synthetic-mode `EmptyStackException` follow-up; `native_tm_get_or_default`; five TreeMap/TreeSet null-and-bound type checks that return where the JDK refuses. (`stream.reuseThrows` is **closed** by W7-65.) |
| `W7-38-crypto-trio-verified.md` | Of its seven refusing rows, four are closed in source; **ChaCha20-Poly1305 remains gated** on a Poly1305 with its RFC 8439 §2.5.2 vector. |
| `W7-39-jca-missing-algorithms.md` | Advertised-vs-implemented is reconciled but not identical; the second SPI driver is recorded design debt; key-length checks moved to `init` rather than `doFinal`. |
| `W7-40-differential-at-14.md` | **Itself superseded by W7-42** — five of its fourteen were the instrument. Do not work from its number. |
| `W7-21-keygen-and-the-synthetic-secretkeyspec-twin.md` | Beyond §2.1: the synthetic `KeyGenerator` serves a wider algorithm set than the provider seed advertises; `init(AlgorithmParameterSpec)` accepts and ignores; `java/security/Key.getAlgorithm` hardcodes `"AES"`. |
| `W7-46-process-cluster.md` | Two recorded-not-fixed, both stated rather than deferred: on **Linux**, one `isAlive0` still reads `/proc/<pid>` then `/proc/<pid>/stat` — the same two-probe pid-recycle hole the Windows arm just lost, on an arm this host cannot compile; and four `java/lang/ProcessBuilder` triples registered by **both** `register_phase57_process` (`SyntheticStub`) and an untagged block in `register_enterprise_natives`, colliding only under `synthetic-jdk`. |
| `W7-53-blocking-close-family.md` | Seven named rows where a thread blocked in a native read/write/accept still cannot observe another thread's `close()`. The four TLS stream sites are the substantial group: the wakeup has to be expressed against the underlying socket while the record assembler keeps its state. Carries `probes/AsyncCloseProbe.java`. |
| `W7-60-harness-extract-blindness.md` | **The instrument, not a defect in the VM.** The blind population is now measured at **zero** across all 70 scheduled vectors, held by four mutation-checked guards. Recorded-not-fixed: `RPriorityQueueGc` and `RTreeRangeGc` publish no check count and are baselined in `regression-suite/harness-uncounted.txt`. **Read its §6 before the next suite run — the pass count is expected to move, and downwards is the good direction.** |
| `W7-61-sslengine-layout-and-tls-blocking.md` | (1) `javax/net/ssl/SSLEngine` carries a 7-slot map and a 14-slot map on ONE object under `synthetic-jdk`; the 8 triples p68 does not re-register index slots 7–13 past the end of a 7-wide allocation. The Compatible-mode half of W7-49's "LIVE, 7 vs 2" row is a **false positive** and the record shows why. (2) The **Windows** half of the TLS read wakeup: Winsock has no `shutdown` that aborts a pending blocking call. |
| `W7-65-stream-reuse-throws.md` | Six residuals, each left on purpose and each with its cost stated: primitive streams (25 call sites swallow the error via `int_stream_elements`' `.unwrap_or_default()`, so a throw there would silently empty the stream); the deferred intermediate op in Compatible mode only; `close()`; `onClose()`; the `GathererOp` stage-replacement constructor; and every stream minted with fewer than five slots, which includes the whole `StreamSupport.stream(realSpliterator, false)` family. |
| `jul-logrecord-infercaller-is-inert-under-jdk-only-20260812.md` | Under `--jdk-only`, `LogRecord.inferCaller()` yields `null/null` with every HotSpot precondition present. Compatible is byte-identical to HotSpot. See W7-56 for the root cause found since. |
| `W7-77-guarded-slot-maps.md` | The four guarded rows are dispositioned and gated, and **none is renumbered** — on the fabricated class each map IS the layout, so a renumber would break the only receiver that exists. Open: `verify_declared_slot_maps` still has no caller, so four more `SlotMap`s are published to a sweep nobody invokes; the dead `util_time.rs` `java/time/Month` twin is documented, not deleted; the `StringJoiner` row cannot leave the census while the fabricated stub shares the real class's binary name. Corrects W7-69 on three counts — `Month`'s Int-in-a-reference is **not** collector-visible on either layout, `Thread` is four disagreeing slots not one, and `java/time/Month` has two slot maps of which one is dead. |

### 2.3 Filed 2026-08-12, source landed, NOT yet verified against a binary

These carry no known-stale rows; what they need is a build and the verification
command each one names in its own final section. **Do not re-derive them.**

`W7-42-differential-instrument-holes.md` (the live differential figure: **9**) ·
`W7-48-fjp-unapplied-patches.md` · `W7-49-slot-index-recensus.md` ·
`W7-50-synthetic-jdk-strict-six.md` (the 63/7 baseline; supersedes the tracked
48/6) · `W7-51-vacuous-sweep-round-2.md` ·
`W7-54-strictmath-fdlibm-family.md` · `W7-56-infercaller-strict.md` ·
`W7-57-close-flush-swallow-sweep.md` · `W7-58-bytebuffer-direct-arm.md` ·
`W7-59-layout-detector-coverage.md` · `W7-62-ratchets-and-dead-code.md` ·
`W7-63-jca-advertise-vs-serve.md` · `W7-64-printstream-trouble-and-errormanager.md` ·
`W7-66-live-over-allocations.md` · `W7-67-host-default-locale.md` ·
`W7-68-live-under-allocations.md` · `W7-69-read-side-alias-instrument.md` ·
`W7-70-printstream-close-noop.md` ·
`W7-71-jca-exception-types-and-line-separator.md` ·
`W7-73-short-object-blind-spot.md` · `W7-74-short-object-repairs.md` ·
`W7-72-ssc-socket-and-filechannel.md` (inverts W7-68's registrar reading for
`FileChannel.isOpen`; the copy that reads a private slot is compiled only into
the synthetic build and overwritten even there) ·
`W7-76-bytebuffer-alias-residuals.md` (`HeapByteBuffer`'s `6` is a LAYOUT
WITNESS, not a width — widening it darkens every indexed fallback in the one
mode with nothing to fall back to) ·
`W7-79-loadlibrary-compatible-arm.md` (the name is `args[2]`, measured on a
running VM, not `args[1]` as recorded) ·
`W7-81-write-route-three-way.md` (its 23 call sites were a red herring; the
defect was one helper mapping three inputs onto two outputs differently per
branch)

Also filed 2026-08-12 and in the same state:
`W7-41-format-exception-subclasses.md` ·
`W7-43-formatmessage-substitution.md` ·
`W7-44-numberformat-enum-and-double-tostring.md` — the three that split out of
the differential's value divergences.

**Owned by other running lanes — do not start these:**
`W4-4-slot-index-species-sweep.md`, `W6-5-vacuous-tests.md`,
`W6-9-complete-erases-the-abnormal-record.md`, and anything numbered `W7-41`
through `W7-54`. W7-55 deliberately did not adjudicate `W4-4`, `W6-5`, `W7-41`
or `W7-43`, so no row above speaks for them.

`W7-55-record-reconciliation.md`, `W7-78-inherited-residual-closeout.md` and
`RETIREMENT-20260811.md` / `RETIREMENT-20260812.md` are bookkeeping records, not
defect records. They stay because the rest of the directory cites their method.
`W7-78` is the only one of the four that changed a file the suite runs — the
four nestmate checks in `RJdkReflect`, listed in §2.6.

`W7-15-cipher-silently-wrong-algorithm.md` was in no index row at all. It is
the record for the ChaCha20-served-as-AES-256-ECB defect (tampered ciphertext
decrypted cleanly). Its headline is FIXED and verified at full parity by
`W7-38-crypto-trio-verified.md`; it is kept for the SHAPE — a dispatch line
that validates a `_name` and then keys on something else — which
`W7-63-jca-advertise-vs-serve.md` and
`W7-71-jca-exception-types-and-line-separator.md` both cite.

### 2.4 Prescribed fixes that are WRONG or SUPERSEDED — do not apply

The observation was right in every case below; the prescription was not. **All
nine are now marked DEAD at the patch block itself** — four already were; the
other five were marked on 2026-08-12, one of which (`W7-22`) had no marker
anywhere in its file while its section was still titled *"Live defect"*.

| Record | The dead prescription | What to do instead |
|---|---|---|
| `W4-3` Patch E | "Remove the `CHACHA20`/`CHACHA20POLY1305` arms and drop `AES/KW`, `AES/KWP` from the seed list." | Nothing. All four were implemented for real on 2026-08-11 (`29429b755` and neighbours). Applying it would delete working RFC 8439 / RFC 5649 code and break the ratchet `every_advertised_sunjce_cipher_is_serviceable` in `native-builtins/src/jca/provider_chain.rs`. |
| `W3-6` out-of-file patch | Make five `native-io` items `pub`, add `p60_handle_stream`, rewrite four bodies. | Superseded by delegation to the real `ProcessHandleImpl` (`0ab1067ec`). `p60_handle_stream` has zero hits tree-wide. Record now in the internal tree. |
| `W6-12` out-of-file patch | Two variants for `alloc_common_factory`. | Both **rejected on the merits**; the fix reads the image's public static field (`46bb0ad2e`). |
| `W6-8`, the `Field.get` row | "Add the export helper as a widening disjunct on the public arm." | That trades an over-deny for an under-deny. `dcfe77cb8` **dissolved** the `is_public` split instead. |
| `W2-1`, residual 2 | `ArrayDeque.stream().count()` blamed on "the unwritten `tail`". | It was the missing spare ring-buffer slot (`fddf67650`, `ad_ensure_capacity`). Do not chase `tail`. |
| `L8`, the "Out of scope" framing | The residual blamed on the discarded constructor seed. | That discard matches HotSpot — `new SecureRandom(seed)` selects DRBG, whose `engineSetSeed` reseeds. The defect is the two `setSeed` no-ops undoing SHA1PRNG reseeding. |
| `W7-18` patch A, preferred form | "Delete both registrations and the function." | Measured to **hang**. The gated fallback form is what landed (`4c9482908`); both registrations must stay. |
| `W7-22` §4's named cause | "Build the singleton through its real constructor." | Written, measured **inert in both modes**, reverted. W7-25 found the real mechanism: a retired shadow reinstated by another registrar holding the same triple. |
| `W4-1`, two struck claims | The layout-discriminator rationale, and "`allowedModes == 0` → allow". | Both false today. A zero-mode `Lookup` now refuses (`6dd552ce2`); only the *unreadable* case still allows. |

### 2.5 `RETIREMENT-20260811.md` is stale on three of its kept rows

Its stated *reasons for keeping* are what put a record in front of the next
reader. Three were already wrong when that audit ran: **W4-1** (both hardening
patches landed 2026-08-07 in `dcfe77cb8`), **W4-2** (the `ServiceLoader` half
landed in `b3aca74c8`; the second is adjudicated unreachable; the record's
*actual* live item is not mentioned), and **W6-8** (first half wrong, second half
half-wrong — only the *module* check remains, deliberately).

### 2.6 What no source read can settle — hand these to a run

Listed so nobody guesses. **Nothing below should ever be written into a status
line as "probably fixed".**

| Question | The run |
|---|---|
| Does the new nestmate field block pass on CratonVM? (L15) | `cratonvm --java-home "<jdk-25>" --real-jdk -cp regression-suite/build RJdkReflect`, and again with `--jdk-only`. Expect `PASS RJdkReflect (64 checks)` on both; HotSpot 25 already gives it. **A red here means the landed `check_field_access` narrowing is inert** — which is exactly what nothing had ever asked. |
| Does arming `BootLoader.loadLibrary` flip `RJdkJni`'s `net` probe? (W5-1, W6-6) | `cratonvm --java-home "<jdk-25>"` on `RJdkJni` in both modes, **with and without** the one-line arming, diffing the `CK RJdkJni loadedLibrary=` line, against `java -cp regression-suite/build RJdkJni`. The arming can only turn a success into an `UnsatisfiedLinkError`, and the first library it claims is `net` — which `is_vm_provided_jdk_library` still carries *because* the dynamic rule cannot fire. |
| Does W6-2's stream path really hand out an illegal provider? | Add a fourth provider to `regression-suite/modules/cratonvm.jdkonly.svc` whose `provider()` returns a non-`Greeter`, assert `ServiceConfigurationError` from **both** `iterator()` and `stream()`, then `cratonvm --java-home "<jdk-25>" --module-path regression-suite/build-modules --add-modules cratonvm.jdkonly.svc -cp regression-suite/build RJdkModule` on both arms. **The module flags are required** — omitting them produces a harness error already misread once as a VM defect. |
| Do the `--synthetic-jdk`-**mode** residuals reproduce? (W6-12, W7-10, L8, W7-63 §8) | A `--features synthetic-jdk` binary now **exists** (W7-50, 63/7 under `--jdk-only`). What has never been run is that binary in `--synthetic-jdk` **mode**, which is the only configuration these residuals live in. Feature ≠ mode. |
| Do the Linux / non-Windows `process.rs` arms even type-check? (W6-10) | A Linux host build, plus the advisory macOS CI job (`.github/workflows/cross-platform.yml`). W6-10's finding 4 widened five signatures across arms that **have never been compiled by any lane that edited them**. |
| W7-28's falsifier, carried forward after its retirement | `javac --release 25 --enable-preview` a 69.65535 class `P` and a 69.0 class `Q`; `cratonvm -cp . P` must fail with HotSpot's wording, `--enable-preview -cp . P` must run, `-cp . Q` must be unchanged, and the over-deny canary `B55p` must still run. |
| Is `RJdkProcess` back to `checks=53`? (W7-46) | `cratonvm --jdk-only -cp regression-suite/build RJdkProcess` vs HotSpot. **A control binary pre-dating the 2026-08-12 merges fails this identically, so the failure is pre-existing.** |
| Does `RJdkLogging` pass? (W7-25, W7-35, W7-56) | `CRATONVM_ARGS=--jdk-only SUITE=all bash regression-suite/run.sh`. **Same control-binary caveat.** |
| Does `StructuredTaskScope` behave? (W7-18) | `probes/StructuredTaskScopeProbe` on both arms, three consecutive byte-identical runs, watching for `join()` hanging. |
| Does the `duplicate_registration_gate` still hold? (W6-9) | `cargo test -p cratonvm-native-builtins --test duplicate_registration_gate`. **W6-9 explicitly forbids re-seeding that number without a real run.** |
| Are W7-20's baselines correct now? (W7-62) | `bash regression-suite/bridge-ratchet.sh` — one census, both gates. Anything other than the movement W7-62 predicts is a finding, not a re-freeze. |
| Is `stub_ratchet` where W7-62 says? | `cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture`. Expected to FIRE. **Paste what it prints; do not cite a remembered number.** |
| Does `probes/ListItrInterfaceProbe` go green in both modes? (W7-62, W7-16) | `javac -d <out> probes/ListItrInterfaceProbe.java`, then both arms with `--java-home`. Expect 16/16 in each; `al.*` red means broken instrument, not finding. |
| Is the differential still at 9? (W7-42) | Re-run `ShadowDifferentialProbe` against a binary built from current `dev`, **one compile, both sides**. Diff against W7-42's transcript and `PROBE-MANIFEST-DIGEST`, never against W7-4's retired 858-line oracle. |

---

## 3. Standing constraints for anyone working this list

* **Before believing a record that says a patch was never applied, grep for the
  patch's token.** The single most expensive mistake made in this directory:
  fourteen instances found in 2026-08-11's audit, eighteen more in W7-55's.
  **If you land a hand-off patch, edit the originating record in the same
  commit.**
* **A green vector closes a headline, not a record.** Read §2.2 first. And when
  a check exists on one path and not its sibling, the vector cannot see it —
  W6-2 is the worked example.
* `native-builtins/tests/stub_ratchet.rs` asserts `BASELINE_SYNTHETIC_STUBS`
  **exactly**, with `SLACK = 0` (the constant carries its own re-freeze history —
  read the figure there, not here), and separately asserts only
  `total >= 8_000` as a vacuity floor. The floor is not a claim about the exact
  total. The strict-mode siblings assert zero `SyntheticStub` registrations and
  `strict_total >= 7_500`; that second number is a collapse detector.
* `Compatible` mode must remain byte-for-byte unchanged (contract §5, §10),
  except for genuine HotSpot-parity fixes, which must be stated as such per
  change. Most of the dangerous mistakes catalogued here are `Compatible`-mode
  behaviour changes made while intending to fix strict mode.
* No process globals for this feature's state (contract §2).
* **Do not size anything here from an `rg` count.** The grep-derived sizes in
  these records are systematically wrong and always in the same direction. Take
  the census from the workload you care about instead — `requested_by`,
  `kind_stated`, `image_declaring_method`, and `scripts/jdk-only-adjudicate.py`
  reads all three.
* **`NativeKind` is ambient — check the enclosing `set_category`, not the
  call.** A bare `registry.register(...)` inherits whatever window it sits in,
  and moving a registration across a window boundary silently changes which
  modes it survives in. W6-6's `BootLoader.loadLibrary` no-op is the worked
  example: it is correct only because its window is `Bridge`.
* **Two registrars can own one class, and the last one wins.** W7-34 names the
  live instance (`java/util/Formatter`). A patch to the losing registrar is
  invisible; prove yours took effect with a `--dump-native-registry` diff.
* **Anchor a `file:line` on the marker tag, not the number.** `// JDK-ONLY-WAVE2:`,
  `// JDK-ONLY-NOTE:`, `// JDK-ONLY-CLASSIFY:`, `// JDK-ONLY-LAYOUT:` are stable;
  the numbers are not. Line citations in these records have rotted by thousands
  of lines — L8's were the worst found, and W6-6's and W4-3's rotted again
  between 2026-08-11 and 2026-08-12.
* **Run the probe under both modes with a HotSpot control before trusting any
  strict-mode claim in this directory.**

---

## 4. The historical passes, compressed

Read this only to understand how the directory got here. Nothing below is work.

**2026-08-04 — the instruments, and three closures.** The observability surface,
the `System.exit` census and the real-protected-stub allow-lists all closed. The
forced-native `String` policy closed the same day — all four copies removed after
being MEASURED inert, with the policy moved to registration.

**2026-08-05/06 — the strict boot, and the step-1 experiment.** Strict boot's
refusal of five classes closed. The `bytecode_available`-at-step-1 proposal was
implemented and **measured**: it took the corpus from 32/17 to **3/46**, and was
reverted. §1.4's lever is registration, not dispatch. What survived is a dial,
`CRATONVM_ENFORCE_NATIVE_SHADOW=1` — which W7-22 has since shown is a **blind
instrument**.

**2026-08-10 — the layouts, the bridges, the census.** Fabricated object layouts
RETIRED; `ensure_synthetic_class` deleted outright; 246 `Bridge`-on-an-undeclared-
receiver rows reclassified against six images — and the 791-row deletion list
handed off with them turned out to be a list of registrations **nobody had
exercised**, not dead ones. 1,939 of 2,542 "method not declared" rows are
actually **inherited**.

**2026-08-11 — the bridge wave, and the retirement audit.** The reclassification
question closed into five slack-free ratchets with committed baselines, scored by
`regression-suite/bridge-ratchet.sh`. What remains open there is the 6,066-row
shadow population itself, for the reason already established — a class's state
has to become real before its shadow can be retired. Then this directory's own
retirement audit: 30 records moved, 22 kept, in `RETIREMENT-20260811.md` — whose
kept-list reasons are themselves stale for three records (§2.5).

**2026-08-12 — the reconciliation, and the close-out.** Every record's status
line was checked against the tree rather than copied forward: eighteen carried a
stale "not applied", nine carried a superseded prescription, and **all thirty
wrong status lines were wrong in the same direction — they overstated how much
was open** (W7-55-record-reconciliation.md). The close-out then retired four of
the five records that reconciliation nominated, completed two `git mv`s that had
been declared but never done, marked the five under-marked dead prescriptions,
and held `W6-2` back on a row nobody had recorded
(W7-78-inherited-residual-closeout.md, `RETIREMENT-20260812.md`).

---

`docs/known-issues/` holds **unfixed** issues only. A record moves to the
internal record tree when it is fixed, not when it is planned. Internal records
are cited here **prefix-less and as plain text** — `fixed-bugs/foo-FIXED.md`,
`retired/bar-RETIRED.md` — and never as a markdown link: the internal tree is
being stripped from public git history before release, so a link into it would
dangle for every public reader, and `types/tests/doc_citation_paths.rs` fails on
one. It scans Rust comments too.
