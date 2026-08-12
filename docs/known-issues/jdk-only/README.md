# JDK-only mode — open defects

<!-- merge: both sides kept; separate lanes' index rows are complementary -->
<!-- merge: both sides kept; the lane's finding and the reconciliation's commit attribution are complementary -->
**Status:** OPEN, **21 records** (W3-6 and W5-2 retired 2026-08-12 by W7-46, W7-46 filed), reduced 2026-08-04, 2026-08-06, and
**Status:** OPEN, **22 records** (W3-6 and W5-2 retired 2026-08-12 by W7-46; W7-46 and W7-60 filed), reduced 2026-08-04, 2026-08-06, and
**2026-08-11** (thirty records retired — see `RETIREMENT-20260811.md`). Filed
2026-07-31 from wave-1 implementation findings.
**Status:** OPEN, **61 records**, index rebuilt from the tree on **2026-08-12**
(reconciliation pass — W7-55-record-reconciliation.md). Earlier reductions
2026-08-04, 2026-08-06 and 2026-08-11 (thirty records retired — see
`RETIREMENT-20260811.md`). Filed 2026-07-31 from wave-1 implementation findings.

> **Read this before you take any record from §2.** The index below was rebuilt
> by checking every record's status line **against the tree**, not by copying the
> status lines forward. The previous index did the latter, and the cost was
> measured: on 2026-08-12 three separate agents each spent most of a run
> re-deriving work that was already done. Every row in §2 states **the live
> residual**, not the headline the record is named after. A record's headline
> going green does **not** empty the record — several rows below are residuals
> their own vector has never exercised.
>
> **The counter-rule matters just as much.** Do not retire a record because its
> headline went green. W4-2's urgent-looking rows were all stale while its
> quiet one (array classes report module `java.base`) was live; retiring on the
> headline would have buried it.

## 1. What jdk-only mode is, and where the contract lives

`--jdk-only` (`CompatibilityMode::JdkOnly`) is the strict mode: CratonVM runs
the real JDK image's own bytecode and **refuses** the compatibility layer that
`--real-jdk` (`Compatible`, the default) admits. It is a *runtime* mode, not a
Cargo feature — `--features synthetic-jdk` is a third, separate configuration
that builds a VM with no class library at all.

* **Normative contract:**
  [`docs/feature-designs/jdk-only-mode.md`](../../feature-designs/jdk-only-mode.md)
  — owned by the orchestrator; do not edit.
* **Read the mechanism facts before anything else:**
  [`docs/architecture/natives-over-real-jdk-classes.md`](../../architecture/natives-over-real-jdk-classes.md).
  Eight facts every lane in this campaign rediscovered at cost — how a native
  actually comes to run instead of real JDK bytecode (**not** the "four doors"
  rule several records still state), why a Cargo feature is not a runtime mode,
  `register()`'s last-registration-wins semantics, what a by-name field read
  cannot report, why a slot index against a real layout is heap corruption
  rather than a wrong answer, what the registration censuses are scoped to, and
  the measurement rules. Its §9 lists the records it corrected and the claims it
  could not correct from `docs/`.
* **This directory is the evidence base** — what is broken, how it was measured,
  what the blast radius is. One record per defect. A record moves to the
  internal record tree **when it is fixed**, not when it is planned.
* Related non-known-issue docs:
  [`docs/jdk-only-runtime-services.md`](../../jdk-only-runtime-services.md),
  [`docs/jdk-only-native-review.md`](../../jdk-only-native-review.md),
  [`docs/jdk-only-migration.md`](../../jdk-only-migration.md).

### The corpus, and what a green corpus licenses

On 2026-08-11 the strict suite was remeasured and closed at **68 passed, 0
failed** (W7-11-strict-baseline-remeasured.md); Compatible was 41/0. On
2026-08-12 five inherited records' vectors were re-run individually on the dev
binary at `ba65f1a19` and all five pass in **both** runtime modes: `RJdkSecurity`
61 checks (L8, W4-3), `RJdkFailure` 43 checks (L16), `RJdkJni` 35 checks
(W5-1, W6-6), `RJdkModule` 44 checks (W2-3, W4-2, W6-2). `RJdkModule` needs
`--module-path regression-suite/build-modules --add-modules
cratonvm.jdkonly.svc`, which `run.sh` supplies via `class_args`; without them it
fails on a **harness** error, not a VM defect.

**What that does and does not license.** A record whose corpus vector is in
`JDKONLY_CLASSES` (`regression-suite/run.sh`) has its "unverified by execution"
caveat discharged **at the vector level**. It is not a per-assertion audit. Two
worked examples of the gap, both from this directory:

* **W2-3** is at 44/44 and its `isAutomatic()` check passes **vacuously** —
  `isAutomatic()` is a hardcoded `false` and the vector never asserts otherwise.
* **W6-8**'s headline fix is exercised by **no vector at all**; the positive
  assertion it asks for was never added to `RJdkModule`.

**If you re-measure, pass `--java-home`.** `run.sh` gives every CratonVM
invocation one; a hand-run that omits it measures the host's default JDK instead
of the JDK 25 image, which on this host inverted the per-mode verdict for
`RJdkModule`. Trace the real command rather than reconstructing it:
`ONLY="RJdkModule" bash -x regression-suite/run.sh 2>&1 | grep <binary>`.

---

## 2. What is still open — one line per record, naming the LIVE residual

Grouped by what a taker needs, not by danger.

### 2.0 Records that are FULLY CLOSED and should be retired

Everything in them is closed and evidenced. Left in place rather than `git mv`d
because retirement is the orchestrator's call, not a reconciliation pass's.

| Record | Why it is closed |
|---|---|
<!-- merge: both sides kept; the lane's finding and the reconciliation's commit attribution are complementary -->
| `L8-securerandom-provider.md` | `crypto_impl.rs` registers `SecureRandom.<init>([B)V` to a no-op that stamps neither `algorithm` nor `provider`, and wins under synthetic-jdk. The clean fix is one deletion; the record names it. |
| `L15-nestmate-access-field-and-constructor.md` | `Constructor.newInstance` still has the caller-step gap `Method.invoke` and the field paths had. Route it through `caller_may_access_member`. |
| `L16-classnotfound-vs-noclassdeffound-shapes.md` | Our `ClassNotFoundException` names the array **descriptor**; HotSpot's `Class.forName` names the **element**. Closing it means teaching `native_class_for_name` to strip `[`s and resolve the element itself. |
| `W2-2-blocked-reader-async-close-wakeup.md` | **Superseded as a residual by `W7-53-blocking-close-family.md`.** The `net_phase_e.rs::re1_socket_read_stream` reader this row named was fixed 2026-08-11; the census in `W7-47-w2-cluster.md` then found the family around it (~20 blocking sites), of which 19 are fixed on 2026-08-12. W7-53 also carries `probes/AsyncCloseProbe.java` — the instrument W2-2's 2026-08-11 measurement cites and which was never in the tree. |
| `W7-53-blocking-close-family.md` | Seven named rows where a thread blocked in a native read/write/accept still cannot observe another thread's `close()`. The four TLS stream sites are the substantial group: a close-aware loop must not abandon a read mid-record, so the wakeup has to be expressed against the underlying socket while the record assembler keeps its state. |
| `W4-2-unnamed-accessor-bypasses-encapsulation.md` | `ServiceLoader` + an encapsulated module-path provider: the caller-sensitive `setAccessible` is refused, so `newInstance` is too. Also `is_package_exported_to` fails closed where `check_module_access` allows. |
| `W5-1-loadlibrary-allowlist-too-wide.md` · `W6-6-nativelibraries-load-fabricated-success.md` | The same residual from two roads: there is **no class-loader-scoped `loadedLibraryNames` bookkeeping in this VM**, so the JDK's dynamic "already loaded elsewhere" rule cannot fire. A static allowlist cannot model it. |
| `W4-3-security-getalgorithms-short-list.md` | `Security.getAlgorithms(type)` answers a plain `HashSet`, not `Collections.unmodifiableSet` — a caller asserting `UnsupportedOperationException` sees the divergence. Two real SHAKE digests are deliberately not advertised, because `message_digest::algorithm_supported` does not implement them. |
| ~~`W5-2-two-silently-skipped-process-checks.md`~~ | **RETIRED 2026-08-12 (W7-46).** Both residuals were closed 08-11, and the *detector* — a `checks=` count nothing asserted — was converted into an assertion plus a printed `skipped=` list. See `W7-46-process-cluster.md`. |
| `W6-2-module-serviceloader-provider-factory.md` | The constructor-form provider **subtype** check is absent — adding it could hard-fail every module-declared service in the JDK's own boot modules, and that was unmeasurable from the lane. |
| `W6-12-stampedlock-split-brain.md` | `java/util/Collections` is served by two registrars at different fidelities, so `unmodifiableSet(s).add(x)` throws while `unmodifiableList(l).add(x)` succeeds. Synthetic-jdk only. |
| `W7-46-process-cluster.md` | Two recorded-not-fixed, both stated rather than deferred: on **Linux**, one `isAlive0` still reads `/proc/<pid>` and then `/proc/<pid>/stat` — the same two-probe pid-recycle attribution hole the Windows arm just lost, on an arm this host cannot compile; and the four `java/lang/ProcessBuilder` triples registered by BOTH `register_phase57_process` (as `SyntheticStub`) and an untagged block in `register_enterprise_natives`, which collide only in **synthetic-jdk** mode. |
<!-- merge: both sides kept; separate lanes' index rows are complementary -->
| `W6-2-module-serviceloader-provider-factory.md` | Headline verified 44/44 in both modes; no out-of-file patch was ever needed; its two "deliberately NOT done" items are argued refusals, and the question it left open ("where does `--jdk-only` stop next") is answered — it does not stop. |
| `W7-4-differential-probe-widening-round-2.md` | Its entire deliverable (run the CratonVM side of the widened probe) was discharged by W7-32, then W7-33/36/37/40. |
| `W7-11-strict-baseline-remeasured.md` | Closed the day it was written, at 68/0. |
| `W7-28-preview-classfile-gating.md` | All four handback parts applied (`de9bedeef`, `6ce65f98a`); nothing pending. Wants one confirming run. |
| `W7-32-round-2-differential-run.md` | Pure measurement, superseded by W7-40 (96 → 43 → 14). |
| `W7-60-harness-extract-blindness.md` | **The instrument, not a defect in the VM.** `regression-suite/run.sh`'s `extract()` deleted the evidence of three scheduled vectors, which therefore could not fail; the blind population is now measured at **zero** across all 70 scheduled vectors and held there by four mutation-checked guards. Recorded-not-fixed: `RPriorityQueueGc` and `RTreeRangeGc` publish no check count and are baselined in `regression-suite/harness-uncounted.txt`, because a count for them is only meaningful measured under the `--nojit --Xmx 64m` reproduction flags this lane could not run. Read its §6 before the next suite run — the pass count is expected to move, and downwards is the good direction. |

### 2.1 Out-of-file patches that are GENUINELY still unapplied

Re-grepped 2026-08-12. Each of these is a real, appliable change.

| Record | The unapplied work |
|---|---|
<!-- merge: both sides kept; the lane's finding and the reconciliation's commit attribution are complementary -->
| `W6-8-method-invoke-exports-gate.md` | Two live **OPEN** rows in its own inventory: `Field.get`/`Field.set` ask the `opens` question unconditionally and so **over-deny** public fields of exported-but-not-opened packages; and the entire `Lookup.unreflect*` / `find*` family has no module check of any kind. HotSpot throws there — measured. |
| `W2-3-module-descriptor-answers-empty-sets.md` | `ModuleDescriptor.modifiers()`, `Requires.compiledVersion()`, `version()`, `rawVersionString()` and `mainClass()` still have **no data source**: the bits are dropped at parse time or never surfaced through `NativeContext`. `RJdkModule` asserts none of them, which is exactly why the green corpus does not close this. |
| ~~`W3-6-processimpl-missing-natives.md`~~ | **RETIRED 2026-08-12 (W7-46).** Both stated residuals were already false: Windows `start_time`/`info0` were filled 08-11, and the `ProcessHandle` interface stubs were rerouted at a real measurement by W7-10. All ten `ProcessImpl` natives are registered, none is shadowed by a second registrar. The live successors are in `W7-46-process-cluster.md`. |
| `W2-1-strict-refuses-the-synthetic-stream-stack.md` | The stream stack is **SPLIT**: some sources divert into the `cratonvm/*` synthetic model, some run real `java.util.stream` bytecode. The record carries the inventory and a four-step staged path to a real `java.util.stream`; step 1 (the real path's own `ForkJoinTask.invoke()` defect) has since landed, so the path is now walkable. |
| `L8-securerandom-provider.md` | Delete three shadowing `SecureRandom` registrations in `native-builtins/src/crypto_impl.rs` (`:1408`, `:1414`, `:1420`) whose no-op bodies undo SHA1PRNG **reseeding** — the one replay guarantee the JDK gives a `SecureRandom`. Synthetic-jdk only. **The record's older framing blames the discarded constructor seed; that is the wrong row.** |
| `W2-2-blocked-reader-async-close-wakeup.md` | Make `poll_stream_readable` `pub` (`native-io/src/net.rs:2421`) and collapse the three duplicated `re1_socket_poll_readable` arms. Idiom cleanup, no behaviour change. |
| `W2-3-module-descriptor-answers-empty-sets.md` | All four parts: `main_class` on `classloading::module::ModuleDescriptor` + `ModuleMainClass` parsing; six `NativeContext` accessors; their `ModuleRegistry` impls; and consuming them in `build_module_descriptor`. Nothing exists. |
| `W3-6-processimpl-missing-natives.md` | Only one line survives: route Windows `destroy()`/`destroyForcibly()` through `signal_pid`, which now exists (`native-io/src/process.rs:1061`) but is still private. **The rest of that section is superseded — see §2.4.** |
| `W4-1-publiclookup-allowedmodes-never-checked.md` | Delete the dead `classloader.rs` lookup block (`lk_public_lookup` `:8436`, `enforce_lookup_access` `:9151`, nine `lk_find_*`) **and re-point four unit tests** (`:13004`, `:13028`, `:13110`) that currently assert against dead code. |
| `W4-3-security-getalgorithms-short-list.md` | Patches A (unmodifiable sets), B (`MD2` advertised, unimplemented), C (SHAKE), D (the silent SHA-256/32-byte digest defaults), F (`SUN`/`KeyFactory`/`ML-DSA`). **Same five defects as W7-29's residuals 1–5 — fix once.** |
| `W5-1-loadlibrary-allowlist-too-wide.md` | Arm `BootLoader.loadLibrary` (`native-builtins/src/lib.rs:13813`; `record_boot_loader_library` at `lang_system.rs:3183` has zero callers); fix the Compatible-mode `Runtime.load0`/`loadLibrary0` argument index (`lang_system.rs:1547`, `:1569`); return the resolved path from `load_native_library`. |
| `W6-10-process-enumeration-syscall-cost.md` | One inventory row — **and its target record was retired out of this directory**, so decide where the row belongs first. |
| `W7-5-registrars-that-never-shipped.md` | The §6.3 wiring ratchet test was never written, so the regression that produced this record can recur silently; `register_concurrent_skip_list_map_natives` is still neither wired nor deleted. |
| `W7-9-minted-interface-abstract-methods.md` | §8.1 (delete the class-blind `forEachOrdered` hack at `vm/src/runtime/interpreter.rs:1007` — correctly still blocked on its precondition), §8.2 `Selector.provider()` (blocked on a `NativeContext::invoke_static`), §8.3 the four `DatagramChannel` residuals (blocked on unifying two synthetic layouts). |
| `W7-10-processhandle-interface-stub-bodies.md` | §7.3: add `commandLine` to the `ProcessHandle$Info` arm of `classloading/src/class_manager.rs:15165-15169`, without which §4's registration is real-JDK-only. |
| `W7-14-fjp-common-factory-bound-by-name.md` | An explicit **human decision**, not a patch: under `--real-jdk`, `commonPool().getFactory().getClass().getName()` still answers a class JDK 25 does not declare. |
| `W7-15` · `W7-21` (crypto) | The two 2-arg `KeyGenerator.getInstance` overloads (`native-builtins/src/jca/cipher.rs:3300`, `:3312`) still hardcode 128 bits and admit any name, diverging from the fixed 1-arg path; `SecretKeySpec` still accepts an empty/null key (`cipher.rs:3936-3949`). |
| `W7-18-structured-task-scope-jep505.md` | Patches B and C. **B is the dangerous one:** `native-builtins/src/jdk25_concurrency.rs` still models the JDK-21 shape (49 mentions), runs last, and owns every shared triple — so it can silently re-impose that shape over this record's fix. |
| `W7-20-refusal-laundered-into-wrong-answer.md` | **Build-blocking.** Two frozen baselines are stale against the tree: nine `LinkedListSnapshotListItr` rows in `scripts/baselines/jdk-only-kind-map-25-linux.tsv:283-291` must flip `synthetic-stub` → `bridge`, and `jdk-only-bridge-ratchet.json` has no note. The ratchet is slack-free. |
| `W7-22-shadow-retirement-logging-and-time.md` | The 7-row `java/io/Print*` retirement never landed — `native-api/src/retired_shadow.rs:176` is still `java/util/logging/`-only. 29 `PrintStream` rows stay blocked on real `PrintStream` state. |
| `W7-27-thread-exit-java-cleanup.md` | §10C: the main/primordial thread never gets `Thread.exit()` — `run_thread_exit_shared` has only two call sites, both worker-death paths. |
| `W7-29-jca-advertise-implement-gaps.md` | Residuals 1–5. **Same five as W4-3's A/B/C/F seen from the other end.** |
| `W7-30-stub-ratchet-boot-path-scope.md` | Two follow-ups, neither started: move the gate to `vm/tests/stub_ratchet.rs` (no such file), and collapse the duplicated boot-path model into a shared `native-builtins/tests/common/` (no such directory). |
| `W7-34-formatter-family-residuals.md` | The Formatter-locale patch: **both** registrars unchanged. Note the record's own trap — `java/util/Formatter` has **two** registrars and the last one wins, so any patch here needs a `--dump-native-registry` before/after diff to prove it took effect. |
| `W7-37-differential-throwable-and-vm.md` | Four out-of-file items: carry two `ClassId`s on `RuntimeError::ClassCastException` instead of a pre-rendered string; route `vm/src/jit/helpers.rs`'s direct `ArrayStoreException` mint through the funnel; stop the four raise sites rebuilding text downstream. |

### 2.2 Live residuals inside an otherwise-fixed record

The headline is closed and the vector passes; a specific sibling case is not.

| Record | The live residual |
|---|---|
| `L15-nestmate-access-field-and-constructor.md` | `Constructor.newInstance` (`native-builtins/src/lang_class.rs:11017`) still has **no** member-modifier gate, so a private constructor is reachable without `setAccessible(true)`. Also: hidden classes are never nestmates (fails closed); and the probe this record asks for was never added, so its landed field narrowing is **unexercised**. |
| `L16-classnotfound-vs-noclassdeffound-shapes.md` | Our `ClassNotFoundException` names the array **descriptor**; HotSpot names the **element**. `native_class_for_name` (`lang_class.rs:2571`) still hands descriptors to `loadClass`. `RJdkFailure` does not assert the message, which is why 43/43 does not close it. |
| `W2-1-strict-refuses-the-synthetic-stream-stack.md` | Seven `NO_IMAGE_JDK_RECEIVERS` names minted outside `native-collections` were never probed (`native-api/src/no_image_receiver.rs:147-157`); `StreamChainCollector` is still an unguarded `try_alloc_synthetic`. |
| `W2-2-blocked-reader-async-close-wakeup.md` | A **fourth** surface of the same species, PLAUSIBLE not confirmed: `native-builtins/src/phases_early.rs:18236-18320` parks in a bare `read_retry_eintr` with no close-awareness and maps `Ok(0)` to `-1`. Probably synthetic-jdk-only. |
| `W3-4-forkjointask-status-flags-and-the-eager-default.md` | The eager-fork flip's blast radius on the Spring/H2 slice is unverified — and the two Rust guards for the now-non-default lazy path are **vacuous** (`apps/fjp_probe/` does not exist, so both tests early-return). |
| `W4-2-unnamed-accessor-bypasses-encapsulation.md` | Array classes report module `java.base` regardless of component type — `classloading/src/class_manager.rs:9568`, unconditional. **The only live item, and `RETIREMENT-20260811.md` does not mention it.** |
| `W5-2-two-silently-skipped-process-checks.md` | `getProcessPids0`'s per-row `OpenProcess` on Windows. A cost, not a defect, ruled **inherent** by W6-10 (`PROCESSENTRY32` carries no creation time). |
| `W6-6-nativelibraries-load-fabricated-success.md` | The boot-loader case cannot fire on either road, because `BootLoader.loadLibrary` is a no-op. W5-1 owns the arming. |
| `W6-8-method-invoke-exports-gate.md` | `unreflectSetter` on a trusted-final field is unchecked; the module half of `find*`/`unreflect*` is absent by design; `unreflectSpecial`'s `specialCaller` conjunct is unenforced; **and no vector asserts the positive**, so the headline fix is unexercised. |
| `W6-12-stampedlock-split-brain.md` | The `Collections` fidelity residual is structurally confined to `synthetic-jdk` and open only in the sense that **no `synthetic-jdk` binary has ever been built to measure it**. The `Phaser` fix is likewise unproven for the same reason. |
| `W7-1-treemap-views-and-iterator-remove-contract.md` | Families 3 and 4 untouched (owned by W7-3, W7-2); `sort`/`replaceAll` do not bump `modCount`; `native_map_key_itr_next` returns null past the end instead of `NoSuchElementException`; the view cache has no version stamp. |
| `W7-2-primitive-stream-terminal-surface.md` | §7.2's `DoubleStream`/`LongStream` holes — `anyMatch`, `reduce`, `findFirst`/`findAny`, `sorted`, `distinct`, `spliterator` — never written. |
| `W7-3-format-conversions-and-stringbuilder-bounds.md` | `append(CharSequence,int,int)` still clamps (and a test **pins** the clamp); `appendCodePoint` truncates; three `insert` overloads have no native; `%a` with the `0` flag and a width is wrong. |
| `W7-8-fabricated-success-io-sweep.md` | `FileChannel` natives lack the real-instance guard `close`/`isOpen` carry (**check this first when the branch is built**); `RandomAccessFile.writeUTF` writes plain UTF-8, not modified UTF-8; two `.max(0)` timeout laundering sites; `Files.isSameFile` ≈ `Path.equals`. |
| `W7-12-strict-annotation-proxy.md` | R2, the resolution-1 redesign. |
| `W7-13-strict-mh-insert-wrapper.md` | A stale §9 row in `docs/architecture/natives-over-real-jdk-classes.md`; the neighbouring `classloader.rs::lk_previous_lookup_class` slot-2 claim was never checked. |
| `W7-16-arraydeque-and-linkedlist-residuals.md` | The `jdk_interfaces` arm was **not** added, so the carrier implements no interfaces and an erased `(ListIterator) x` throws `ClassCastException` — **now reachable in strict mode too**, because the other two hunks landed. Worse than when recorded. |
| `W7-17-vm-internal-door-sweep.md` | §8's "what this record does not fix", plus the optional `fabricated_origin_for_name` arm for `CratonVM$…` names. |
| `W7-19-methodhandles-compatible-residuals.md` | `bindTo` does not raise `ClassCastException` for a wrong reference type; `isVarargsCollector()` answers `false`; `type()` is not narrowed after a getter/array-getter bind. |
| `W7-23-thread-container-registration.md` | The interlock is still `false` by default (`native-builtins/src/shared_secrets_bridge.rs:745`). Its blocker is **gone** — the de-registration half landed via W7-27 (`c3da9455d`) — so this is now flip-and-measure, not blocked. |
| `W7-24-httpserverloop-and-strict-fallbacks.md` | `cratonvm/net/HttpBodyReplaySubscription` (§4, left loud) and the two `SSLSocket*Stream` sites with their twins in `phases_late/ssl_security.rs`. |
| `W7-25-jul-getlogger-regression.md` | The `Supplier` convenience overloads evaluate a suppressed supplier; `LogManager.getLogger` demand-creates for an undemanded name; `log(LogRecord)` is not level-gated. |
| `W7-26-getannotation-swallowed-exception.md` | Twelve loader ladders still catch **any** exception from a user loader's `loadClass`; five sites re-raise as the wrong type; the two swallow-shape scans covered one file only. |
| `W7-31-enable-preview-wiring.md` | Only the `<Unknown>` vs `""` nameless-define distinction, which §3.1 argues is not worth doing. (Its README row is discharged by this rebuild.) |
| `W7-35-jul-supplier-and-payload-residuals.md` | One survivor: the `--jdk-only` half of #59 — see the `jul-logrecord-inferCaller` record below. |
| `W7-36-differential-view-families.md` | `stream.reuseThrows` deliberately not attempted (needs a `linkedOrConsumed` flag across ~99 call sites); the synthetic-mode `EmptyStackException` follow-up; `native_tm_get_or_default`; five TreeMap/TreeSet null-and-bound type checks that return where the JDK refuses. |
| `W7-38-crypto-trio-verified.md` | Of its seven refusing rows, four are closed in source by W7-39 and the ChaCha20 lane; **ChaCha20-Poly1305 remains gated** on a Poly1305 with its RFC 8439 §2.5.2 vector. |
| `W7-39-jca-missing-algorithms.md` | Advertised-vs-implemented is reconciled but not identical; the second SPI driver is recorded design debt; key-length checks moved to `init` rather than `doFinal`. |
| `W7-40-differential-at-14.md` | Three value divergences (`Enum.valueOfBadName` omits the qualified type; `NumberFormat` renders `($1,234.50)`; `Random.nextGaussian` digit count) plus **two instrument holes** — four `ArrayDeque` null observables and `COW.addAllAbsent` vanish with **no `SECTION-DIED`**, and seven `[SUREFIRE-NPE]` lines leak into stdout. |
| `jul-logrecord-infercaller-is-inert-under-jdk-only-20260812.md` | Under `--jdk-only`, `LogRecord.inferCaller()` yields `null/null` with every HotSpot precondition present. Two candidate causes, both unchecked. Compatible is byte-identical to HotSpot. |
| `W7-33-differential-dead-sections.md` | The synthetic-mode `EmptyStackException` follow-up in `classloading/src/class_manager.rs`. |
| `W7-21-keygen-and-the-synthetic-secretkeyspec-twin.md` | Beyond §2.1: the synthetic `KeyGenerator` serves a wider algorithm set than the provider seed advertises; `init(AlgorithmParameterSpec)` accepts and ignores; `java/security/Key.getAlgorithm` hardcodes `"AES"`. |

### 2.3 Owned by other running lanes — do not start these

`W4-4-slot-index-species-sweep.md`, `W6-5-vacuous-tests.md`, and any record
numbered `W7-41` through `W7-54`.

<!-- merge: both sides kept; the lane's finding and the reconciliation's commit attribution are complementary -->
`W3-4-forkjointask-status-flags-and-the-eager-default.md`,
`W6-5-vacuous-tests.md`, `W6-9-complete-erases-the-abnormal-record.md`,
`W7-1-treemap-views-and-iterator-remove-contract.md`, and the `W7-2` … `W7-6`
records.
### 2.4 Prescribed fixes that are WRONG or SUPERSEDED — do not apply

The observation was right in every case below; the prescription was not. Each is
marked in place in its own record. **This is the second-most expensive failure
mode in this directory after the stale "not applied" heading**, because a
superseded patch reads exactly like pending work.

| Record | The dead prescription | What to do instead |
|---|---|---|
| `W4-3` Patch E | "Remove the `CHACHA20`/`CHACHA20POLY1305` arms and drop `AES/KW`, `AES/KWP` from the seed list." | Nothing. All four were implemented for real on 2026-08-11 (`29429b755` and neighbours). Applying it would delete working crypto and break the ratchet at `jca/provider_chain.rs:4043`. |
| `W3-6` out-of-file patch | Make five `native-io` items `pub`, add `p60_handle_stream`, rewrite four bodies. | Superseded by delegation to the real `ProcessHandleImpl` (`0ab1067ec`), which passes its exceptions through untouched. Only the Windows `destroy()` line is still live. |
| `W6-12` out-of-file patch | Two variants for `alloc_common_factory`. | Both **rejected on the merits**; the fix reads the image's public static field (`46bb0ad2e`). The conservative variant's ordering is refused in a comment at `concurrent.rs:8566-8577`. |
| `W6-8`, the `Field.get` row | "Add the export helper as a widening disjunct on the public arm." | That trades an over-deny for an under-deny. `dcfe77cb8` **dissolved** the `is_public` split instead. |
| `W2-1`, residual 2 | `ArrayDeque.stream().count()` blamed on "the unwritten `tail`". | It was the missing spare ring-buffer slot (`fddf67650`). Do not chase `tail`. |
| `L8`, the older framing | The residual blamed on the discarded constructor seed. | That discard matches HotSpot. The defect is the two `setSeed` no-ops undoing SHA1PRNG reseeding. |
| `W7-18` patch A, preferred form | "Delete both registrations and the function." | Measured to **hang**. The gated fallback form is what landed (`4c9482908`). |
| `W7-22` §4's named cause | "Build the singleton through its real constructor." | Written, measured **inert in both modes**, reverted. W7-25 found the real mechanism. |
| `W4-1`, two struck claims | The layout-discriminator rationale, and "`allowedModes == 0` → allow". | Both false today. The code is correct only because of W6-3's class-side witness; a zero mode now refuses (`6dd552ce2`). |

### 2.5 `RETIREMENT-20260811.md` is stale on three of its kept rows

That audit's stated *reasons for keeping* a record are what put these records in
front of the next reader. Three are wrong, and all three were already wrong when
the audit ran:

* **W4-1** — "two optional hardening patches … still unapplied". Both landed
  2026-08-07 in `dcfe77cb8`, four days earlier.
* **W4-2** — "the `ServiceLoader` + encapsulated-provider interaction and
  `is_package_exported_to` failing closed are both still open". The first landed
  in `b3aca74c8`; the second is adjudicated unreachable. The record's actual live
  item (array-class module) is not mentioned.
* **W6-8** — "`Field.get`/`Field.set` ask the `opens` question unconditionally …
  and the whole `Lookup.unreflect*`/`find*` family has no module check of any
  kind". First half wrong (`dcfe77cb8`); second half half-wrong (the *mode* check
  landed in `3644142d5`; only the *module* half remains, deliberately).

### 2.6 What no source read can settle — hand these to a run

Listed so nobody guesses. Nothing below should ever be written into a status line
as "probably fixed".

| Question | The run |
|---|---|
| Does arming `BootLoader.loadLibrary` flip `RJdkJni`'s `net` probe? (W5-1) | `cratonvm --jdk-only` and `--real-jdk` on `RJdkJni`, with and without the arming, diffing the `CK RJdkJni loadedLibrary=` line. |
| Is `RJdkProcess` back to `checks=53`? (W3-6, W5-2) | `cratonvm --jdk-only -cp regression-suite/build RJdkProcess` vs HotSpot. The counter is the only signal — nothing throws. **A control binary pre-dating the 2026-08-12 merges fails this identically, so the failure is pre-existing.** |
| Does `RJdkLogging` pass? (W7-25, W7-35, jul-inferCaller) | `CRATONVM_ARGS=--jdk-only SUITE=all bash regression-suite/run.sh`. **Same control-binary caveat: its Compatible-mode failure is pre-existing, not a regression.** |
| Do the `synthetic-jdk`-only residuals reproduce at all? (W6-12, W7-10, L8) | `cargo build --features synthetic-jdk`. **No such binary has ever been built for these records.** |
| Do the Linux / non-Windows `process.rs` arms even type-check? (W5-2, W6-10) | A Linux host build, plus the advisory macOS CI job (`.github/workflows/cross-platform.yml`). They have never been compiled. |
| Does `StructuredTaskScope` behave? (W7-18) | `probes/StructuredTaskScopeProbe` on both arms, three consecutive byte-identical runs, watching for `join()` hanging. |
| Does the `duplicate_registration_gate` still hold? (W6-9) | `cargo test -p cratonvm-native-builtins --test duplicate_registration_gate`. **W6-9 explicitly forbids re-seeding that number without a real run.** |
| Are W7-20's two baselines correct now? | Re-take a linux/25 kind-map census and diff; the nine `LinkedListSnapshotListItr` rows must be the only difference. |
| Is the differential still at 14? (W7-40) | Re-run `ShadowDifferentialProbe` against a binary built from current `dev`; also settles whether the four missing `ArrayDeque` rows are a probe path or lost output. |

---

## 3. Standing constraints for anyone working this list

* **Before believing a record that says a patch was never applied, grep for the
  patch's token.** This is the single most expensive mistake made in this
  directory. `RETIREMENT-20260811.md` found fourteen such records; the 2026-08-12
  pass (W7-55-record-reconciliation.md) found **eighteen more**, including one —
  W6-9 §8 — that was handed out as pending work **twice** after landing in full.
  Lanes that could not edit a file wrote the patch down and handed it off, and
  nobody went back to the record when it landed. **If you land a hand-off patch,
  edit the originating record in the same commit.**
* **A green vector closes a headline, not a record.** Read §2.2 before deciding a
  record is empty. Two rows there are residuals whose own vector has never
  exercised them.
* `native-builtins/tests/stub_ratchet.rs` asserts `BASELINE_SYNTHETIC_STUBS`
  **exactly**, with `SLACK = 0` (the constant carries its own re-freeze history —
  the figure moves, so read it there rather than here), and separately asserts
  only `total >= 8_000` as a vacuity floor. The floor is not a claim about the
  exact total — do not cite one. The strict-mode siblings assert zero
  `SyntheticStub` registrations and `strict_total >= 7_500`; that second number is
  a collapse detector, not a measurement.
* `Compatible` mode must remain byte-for-byte unchanged (contract §5, §10). Most
  of the dangerous mistakes catalogued here are `Compatible`-mode behaviour
  changes made while intending to fix strict mode.
* No process globals for this feature's state (contract §2). Two of the items in
  this directory were existing violations; do not add a third.
* **Do not size anything here from an `rg` count.** The grep-derived sizes in
  these records are systematically wrong, and always in the same direction.
  Three measurements say so: 52 grep-visible `ensure_synthetic_class` call sites
  of which **3** fire on a strict boot; "about 8,000" registrations against a
  measured **11,909**; and a `native-collections` mis-tagging scoped at 1,195
  registrations that is really **10,084** spread over the whole tree. Take the
  census from the workload you care about instead — `requested_by` names the
  *Rust* call site of every fabrication, `kind_stated` separates chosen from
  inherited kinds, `image_declaring_method` adjudicates every registration
  against the class-path bytes whether or not the run touched the class, and
  `scripts/jdk-only-adjudicate.py` reads all three.
* **JMX and `java.util.function.Function$Identity` are already retagged
  `Bridge`** and are *not* among the residual 157. Any plan that starts from
  "retag JMX" is working from a stale report.
* **Run the probe under both modes with a HotSpot control before trusting any
  strict-mode claim in this directory.**
* **Anchor a `file:line` on the marker tag, not the number.** Wave-2 sites carry
  `// JDK-ONLY-WAVE2:`, deferred observations `// JDK-ONLY-NOTE:`, per-registrar
  category verdicts `// JDK-ONLY-CLASSIFY:`, field-slot verdicts
  `// JDK-ONLY-LAYOUT:`. Those tags are stable; the numbers are not. Line numbers
  in the older records have rotted by thousands of lines — L8's were the worst
  found in the 2026-08-12 pass.
* **Two registrars can own one class, and the last one wins.** W7-34 names the
  live instance (`java/util/Formatter`). A patch to the losing registrar is
  invisible; prove yours took effect with a `--dump-native-registry` diff.

---

## 4. The historical passes, compressed

Read this only to understand how the directory got here. Nothing below is work.

**2026-08-04 — the instruments, and three closures.** The observability surface
(`fixed-bugs/jdk-only-observability-surface-FIXED-20260804.md`), the `System.exit`
census (`fixed-bugs/jdk-only-system-exit-census-FIXED-20260804.md`) and the
real-protected-stub allow-lists
(`fixed-bugs/jdk-only-real-protected-stub-allowlists-FIXED-20260804.md`) all
closed. The forced-native `String` policy closed the same day — all four copies,
removed after being MEASURED inert (a binary without them produced a
byte-identical 392-case `String` transcript in both modes), with the policy moved
to registration: `NativeMethodRegistry::register` drops every `java/lang/String`
`Bridge` in real-JDK mode. Four new guards landed, each verified by injecting a
violation and watching it fail, then reverted.

**2026-08-05/06 — the strict boot, and the step-1 experiment.** Strict boot's
refusal of five classes closed
(`fixed-bugs/jdk-only-strict-boot-refused-five-classes-FIXED-20260806.md`); the
fifth, `cratonvm/internal/SystemLogger`, was reached unconditionally from
`ObjectInputFilter$Config.<clinit>`, so refusing it cost every
`ObjectInputStream` construction in the VM. The `bytecode_available`-at-step-1
proposal was implemented and **measured**: it took the corpus from 32/17 to
**3/46**, and was reverted
(`retired/jdk-only-step1-bytecode-available-RESOLVED-20260806.md`). §1.4's lever
is registration, not dispatch. What survived is the observation plus a dial,
`CRATONVM_ENFORCE_NATIVE_SHADOW=1` — which W7-22 has since shown is a **blind
instrument**: it yields at most once per triple, and the second dispatch path in
`vm/src/runtime/interpreter.rs` never consults it.

**2026-08-10 — the layouts, the bridges, the census.** Fabricated object layouts
RETIRED: the shadow-layout census is at **zero** NAME rows, zero `_vmN` rows and
zero `java/net/URI` access-site rows on both standing probes
(`fixed-bugs/jdk-only-fabricated-object-layouts-FIXED-20260810.md`, with
`fixed-bugs/jdk-only-newbufferedwriter-fd-in-writebuffer-FIXED-20260810.md`).
`ensure_synthetic_class` deleted outright
(`fixed-bugs/jdk-only-ensure-synthetic-class-deleted-FIXED-20260810.md`).
`Bridge` registrations on a receiver no supported image declares: 246 rows across
50 classes reclassified against six images
(`fixed-bugs/jdk-only-bridge-on-a-receiver-no-image-declares-FIXED-20260810.md`)
— and the 791-row deletion list handed off with them turned out to be a list of
registrations **nobody had exercised**, not dead ones. The census that asks one
class on one platform closed
(`fixed-bugs/jdk-only-census-one-class-one-platform-FIXED-20260810.md`): 1,939 of
2,542 "method not declared" rows are actually **inherited**, and 59 registrations
are a genuine bridge only on Windows — provable from a Linux host, because
CratonVM adjudicates an image it cannot run. `MemorySegment.set` had no
implementation for five of its nine carriers
(`fixed-bugs/ffm-memorysegment-set-carriers-FIXED-20260810.md`).

**2026-08-11 — the bridge wave, and the retirement audit.** The reclassification
question closed: the population is now five slack-free ratchets with committed
baselines rather than a number in a document — `bridge_without_acc_native` 8,911,
`bridge_shadows_bytecode` 6,066, `bridge_stated_shadows_bytecode` 24,
`superseded_kind_disagreements` 52, `superseded_stub_lost_to_admitted` 4, all
scored by `regression-suite/bridge-ratchet.sh` (the retired
`bridge-reclassification-wave` write-up). What remains open there is the 6,066-row
shadow population itself, for the reason already established — a class's state has
to become real before its shadow can be retired. Then this directory's own
retirement audit: 30 records moved, 22 kept, in `RETIREMENT-20260811.md` — whose
kept-list reasons are themselves stale for three records, see §2.5.

**2026-08-12 — the reconciliation.** Every record's status line was checked
against the tree rather than copied forward. Eighteen carried a stale
"not applied" or "NOT WIRED" claim; nine carried a prescription that had been
superseded or rejected. Write-up and method:
W7-55-record-reconciliation.md.

**The ranked work list this file used to carry is closed.** Items 1 through 11
are all either fixed or retired; the last of them (item 1, `NativeKind` ambient
and defaulting to `SyntheticStub`) was retired 2026-08-06 and the
reclassification it pointed at closed 2026-08-11. Item 11's thirteen
cross-cutting findings all closed and its record left the public tree on
2026-08-10.

**Retired one-off audits.** The registration census behind old items 1 and 2 and
the object-layout survey they rested on were one-off audits; their durable
findings are stated in [`docs/README.md`](../../README.md) and
[`docs/architecture/natives-over-real-jdk-classes.md`](../../architecture/natives-over-real-jdk-classes.md).

---

`docs/known-issues/` holds **unfixed** issues only. A record moves to the
internal record tree when it is fixed, not when it is planned. Internal records
are cited here **prefix-less and as plain text** — `fixed-bugs/foo-FIXED.md`,
`retired/bar-RETIRED.md` — and never as a markdown link: the internal tree is
being stripped from public git history before release, so a link into it would
dangle for every public reader, and `types/tests/doc_citation_paths.rs` fails on
one. It scans Rust comments too.
