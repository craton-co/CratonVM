# JDK-only mode — open defects

> **Start at [`INDEX.md`](INDEX.md)** (added 2026-08-13, lane C18): one line per
> record — status read from the record's own prose rather than its filename,
> provenance (**MEASURED / PREDICTED / SOURCE-ONLY**), grouped by subsystem. It
> also carries the corrections this directory's records had drifted away from,
> the `W7-39` number-collision resolution, and the contradictions that are
> stated rather than guessed. `INDEX.md` was rebuilt again on **2026-08-17**
> (lane G40, FOURTH PASS): 36 records had no row, including the whole wave-G
> line and both of the two most current documents here. That pass also carries
> **§B, seven standing claims this directory got wrong**, and **§C**, the eight
> vectors measurement closed. Read §B.1 before trusting any "this body is dead"
> conclusion, and §B.2 before reasoning about native-versus-bytecode dispatch.
>
> **The headline count below is corrected as of 2026-08-22, and it will rot
> again** — the directory has gone 105 → 155 → 227 → 280 → **412** files.
> Recount before quoting.
>
> **2026-08-22 recount, `HEAD = 081dd2fe8`.** 412 files. Applying this README's
> own exclusion rule mechanically (28 matches) plus the five `WORKER-n-the-*`
> handoff briefs added 2026-08-21 leaves **379** as the upper bound. The rule
> needed no amendment this time — the five parallel lanes named their records
> `WORKER-n-NOTE-k-*`, which are records and are counted as such. The +132 in
> five days is five lanes writing at once, not 132 new defects: a large share
> are notes ON existing records, corrections, and acceptance evidence.
>
> **The 253 → 379 jump is not a measure of progress in either direction.**
> Records were also RETIRED in that window (WORKER-4 alone retired eleven
> `java/io` shadows and three `deprecated` natives, with their records moved),
> and the growth is dominated by the instrument and acceptance notes those
> retirements required. Do not read the count as a defect population; that is
> the mistake this section already records three times.

**Status:** OPEN. **280 `.md` files** in this directory on **2026-08-17**, at
`HEAD = 9ae371468`, taken with `ls docs/known-issues/jdk-only/*.md | wc -l`.
**That is the file count, not a defect-record count, and the distinction is the
whole reason the old headline was wrong three times running.** Applying this
README's own exclusion rule mechanically — this file, `INDEX.md`, `HANDOFF-*`,
`RETIREMENT-*`, `STUB-CENSUS-*`, `BASELINE-*` (10 files), plus the dated
deliverables that match no pattern at all (`APP-READINESS-`,
`JDK-ONLY-REPORT-CENSUS-`, `P1-*`, `P2-*`, `P4A-*`, `P4B-*`, `C8-*`, `C17-*`,
`WAVE-D-QUEUE`; 17 files) — leaves **253**. Read 253 as an **upper bound on
defect records, not a count of them**: a dozen more files in the remainder are
`META` by their own prose (`W7-55`, `W7-40`, `W7-100`, `W7-60`, `E23-1`,
`E39-1`, `W8-D2-1`, `G3-1`, `G20-1`, `G27-1`, `G33-1`, `G34-1`, …) and match no
naming pattern, which is exactly the drift the previous headline described and
then fell to.

**The old headline, kept because its lesson is the point.** It read *"OPEN, **94
records**"*, with a paragraph explaining that the arithmetic producing 94 —
`105 − 11` — had two terms that both moved and cancelled, so "a reader who
quoted 94 without recounting would have been right for the wrong reason, and the
next deliverable to land breaks that coincidence." It did. **The exclusion list
is the thing that drifts, not the count**; that sentence is still the most useful
line in this section. Note also that records land from parallel lanes and some
are **untracked when you count**, so `git ls-files` and `ls` disagree here by
design. Index rebuilt from the tree on **2026-08-12**
by W7-78-inherited-residual-closeout.md, on top of the reconciliation pass
W7-55-record-reconciliation.md. **Fourteen records left the directory that day,
in two passes.** Six in the first — four retired by RETIREMENT-20260812.md, and
two (`W3-6`, `W5-2`) whose `git mv` had simply never been done although both
records and this index already said RETIRED. Eight in the second, after the
campaign closed at 69/2 in both arms: `RETIREMENT-20260812B.md`, which
re-adjudicated ten nominations against the tree **and against running
binaries**, moved eight and held two — one of them (`W7-31`) on a defect that
pass measured for the first time. Earlier reductions 2026-08-04, 2026-08-06 and
2026-08-11 (thirty records, `RETIREMENT-20260811.md`). Filed 2026-07-31 from
wave-1 implementation findings.

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
> merely wasting time. All **eleven** are now marked DEAD at the patch block itself,
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

**"Feature ≠ mode" is true but incomplete, and the missing half was measured on
2026-08-12: the mode REQUIRES the feature.** A shipping binary given
`--synthetic-jdk` **refuses outright, exit 1**, during argument parsing — none
of the ~5,200 synthetic stubs are compiled in, and running without either them
or a real-JDK boot classpath is rejected rather than allowed to fail later as an
unexplained `NoClassDefFoundError`. So the two are not independent axes: the
feature is necessary and not sufficient (you still need the flag). The
consequence for this directory is concrete — **any residual living only in
`--synthetic-jdk` mode cannot be adjudicated by any run of a shipping binary**,
not by `run.sh` at any `SUITE=` value, not by a corpus run, not by a census. A
record marked "unreproducible" on such a residual measured the wrong binary.
Evidence: P4B-SYNTHETIC-JDK-MODE-20260812.md.

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
  [`docs/jdk-only-runtime-services.md`](runtime-services-blocker-inventory.md),
  [`docs/jdk-only-native-review.md`](../../jdk-only-native-review.md),
  [`docs/jdk-only-migration.md`](../../jdk-only-migration.md).
* **The 2026-08-12 measurement deliverables, which live in this directory but
  are not defect records** (and are excluded from the count above):
  `APP-READINESS-20260812.md` (what actually stops a real app),
  `STUB-CENSUS-20260812.md` (the native surface per row),
  `P2-COLLECTIONS-SHADOWS-20260812.md`,
  `P1-BASELINE-20260812.md` (the Phase 1 baseline, and **nine** blocking
  families where `docs/feature-designs/jdk-only-completion-roadmap.md` had
  named three), `JDK-ONLY-REPORT-CENSUS-20260812.md` (the census instrument and
  the request-is-not-a-failure rule, §3) and
  `P4B-SYNTHETIC-JDK-MODE-20260812.md`. **Six of those nine fabricated classes
  are NOT under `cratonvm/internal/`** — they sit in JDK namespaces
  (`…atomic/…$RustJvmImpl`, `javax/net/ssl/SSLSocket*Stream`,
  `java/util/function/Consumer$AndThen`, `java/lang/foreign/DowncallHandle`,
  `java/util/Enumeration$Impl`, `java/util/concurrent/CompletedFuture`), so any
  screen written as a `cratonvm/internal/` prefix match reports clean while
  they are all still fabricated. Screen on the refused-class set, whatever the
  package.

### The corpus, and what a green corpus licenses

On 2026-08-11 the strict suite was remeasured and closed at **68 passed, 0
failed**; Compatible was 41/0. On 2026-08-12 five inherited records' vectors were
re-run individually on the dev binary at `ba65f1a19` and all five pass in
**both** runtime modes: `RJdkSecurity` 61 checks (L8, W4-3), `RJdkFailure` 43
(L16), `RJdkJni` 35 (W5-1, W6-6; **41 today** — 40 after W7-79's five `Runtime` checks,
41 after 2026-08-12's second pass asserted *which* library loads),
`RJdkModule` 44 (W2-3, W4-2, W6-2). A
`--features synthetic-jdk` binary was also built and measured for the first
time — **63 passed / 7 failed** under `--jdk-only`
(W7-50-synthetic-jdk-strict-six.md).

**What that does and does not license.** A record whose corpus vector is in
`JDKONLY_CLASSES` (`regression-suite/run.sh`) has its "unverified by execution"
caveat discharged **at the vector level**. It is not a per-assertion audit. Four
worked examples of the gap, all from this directory:

* **W2-3**'s `isAutomatic()` check passes **vacuously** at every count the
  vector has ever had (44, then 104, then 155, now 163) — `isAutomatic()` is a hardcoded
  `false`, `cratonvm.jdkonly.svc` really is not automatic, and no fixture can
  tell the two apart without an **automatic** module to disagree with it.
* **W6-8**'s headline fix is exercised by **no vector at all**.
* **W6-2** was at 44/44 while a subtype check was missing on one of its two
  provider paths, because the fixture's provider is a *correct* subtype and only
  the positive case was ever walked (§2.2). **Fixed 2026-08-12
  (`W7-85-serviceloader-stream-validation.md`); the vector was 104/104 then and
  is 155 after 2026-08-12's constructor-form pass, and the example stands as
  written.** It remains the cleanest instance of the
  shape: a guard installed on one of two siblings, validated by a fixture that
  only ever satisfies it.
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
| `jul-logrecord-infercaller-is-inert-under-jdk-only-20260812.md` | SUPERSEDED — now retired/jdk-only-jul-logrecord-infercaller-SUPERSEDED-20260812.md, carrying a marker that points at W7-56-infercaller-strict.md, which measured it and fixed it. **Both of its candidate causes are refuted**: the setters stick, and our `StackWalker` hands `CallerFinder` exactly the frame list HotSpot's walks. Row added here 2026-08-12 and the stale §2.1 row for it deleted — it had survived the move. HANDOFF-20260812.md still lists this as an unreconciled README item and is now stale on it. |

Full evidence per record: `RETIREMENT-20260812.md`.

**Second pass the same day, after the campaign closed at 69 passed / 2 failed in
both arms.** Eight more, re-adjudicated against the tree **and against running
binaries** (`scratchpad/bin/cratonvm-f8.exe` A/B'd against pristine dev
`44044c7e2`, with HotSpot 25.0.3.9 as the oracle). Where a record's only
instrument was a probe, the probe was re-created and run — `run.sh` never runs
`probes/` at any `SUITE=` value, so no suite green can stand in for one.

| Record | Where it went, and why |
|---|---|
| `W7-59-layout-detector-coverage.md` | RETIRED — no defect left, only a RUN. Instrument is one implementation with 13 gates; every §8 row has a live owner. **Its census flag was run and emits**, so the gap is `regression-suite/`/`ci/` scheduling. Two corrections carried: `register_selector` is gone as a FUNCTION but survives in three tombstone comments, and `CRATONVM_DBG_LAYOUT_ALIAS` is now the deprecated spelling of `CRATONVM_DBG=layout-alias`. |
| `W7-13-strict-mh-insert-wrapper.md` | RETIRED-FIXED — its own falsifier run on the rebuilt binary: **12 of 12 combinators identical to HotSpot** under `--jdk-only` (was ten `NoClassDefFoundError`s) and **zero** `__mh_` rows in `--jdk-only-report`. Its two "observed on the way" rows (`asCollector` under `--real-jdk`, `bindTo` on a leading `int`) also measure closed. The one open row — the second, disagreeing `MethodHandle` slot map — was transplanted into `W7-19` §5.2.1. |
| `W7-40-differential-at-14.md` | SUPERSEDED by **W7-42** — and no row loses its owner on the way out: `stream.reuseThrows` is W7-65's, the other five are carried by W7-44 and W7-42. **Do not work from the number 14.** |
| `W7-48-fjp-unapplied-patches.md` | RETIRED — its retirement condition is satisfied and was confirmed: W6-9 §7.5 has its own row in §2.2 below. Its own deliverable is run-verified: `completionRecord()` at `RJdkForkJoin.java:370`, in `JDKONLY_CLASSES`, `PASS` in both arms. |
| `W7-82-forname-duplicate-define.md` | RETIRED-FIXED — vector `RLoaderChurnDefine` PASS in both arms. **The fix in the tree is no longer the one that record describes**: W7-87 deleted its additive block and replaced the predicate outright (`classloader.rs:2707`), and owns its residual. |
| `W7-38-crypto-trio-verified.md` | RETIRED-FIXED — what closed is its own live residual, the **instrument**: `RChaCha20Cipher.java` is tracked, is in `CORE_CLASSES`, and PASSES in both arms, so `prune_missing` no longer silently drops the vector three places claimed was green. RFC 8439 §2.5.2 vector at `chacha20.rs:459`. |
| `W7-67-host-default-locale.md` | RETIRED-FIXED — both halves in the tree, and the §3 residual retired **with a home**: it is written at `locale_bootstrap.rs:205-210` in `resolve_default_locale_for`'s own doc comment. |
| `W7-92-system-timezone-answers-utc.md` | RETIRED-FIXED — measured, not inferred from a `PASS`: `defaultZoneRawOffsetMs` **0 → -10800000** and `TimeZone.getDefault().getID()` `UTC → America/Buenos_Aires`, matching HotSpot in both modes. **Its §6 prediction of `179` is superseded** (both VMs read 177 at this hour — do not pin the number), and its §7 open question is settled: the named IANA id resolves, so the `ZoneInfoFile` `<clinit>` comment in `lib.rs` is stale. |

Full evidence per record, plus the two held back and why:
`RETIREMENT-20260812B.md`.

### 2.1 Out-of-file patches that are GENUINELY still unapplied

Re-grepped 2026-08-12. Each of these is a real, appliable change. **Check §2.4
before applying anything from a record that also appears there.**

| Record | The unapplied work |
|---|---|
| `W6-8-method-invoke-exports-gate.md` | **This row is stale in its first half — re-verified 2026-08-12.** `Field.get`/`Field.set` do **not** over-deny: `enforce_module_check_on_field` (`native-builtins/src/lang_class.rs:1312`) calls `check_reflection_export_access_with_target_id` on one arm for public and non-public fields alike, the `is_public` split having been *dissolved* by `dcfe77cb8` — see §2.4, where the record's own prescription for that row is listed DEAD. What is genuinely unapplied is only the **module/`exports`** half for `Lookup.find*`/`unreflect*`, and that is absent by design, not by omission. |
| `W2-3-module-descriptor-answers-empty-sets.md` | All four parts, nothing exists — re-verified 2026-08-12: `main_class` on `classloading::module::ModuleDescriptor` + `ModuleMainClass` parsing; six `NativeContext` accessors; their `ModuleRegistry` impls; consuming them in `build_module_descriptor`. `modifiers()`, `rawVersion()`, `mainClass()` have **no data source**; `version()` and `Requires.compiledVersion()` DO — both are parsed and carried today and are blocked on the bridge alone. **Apply the four parts together.** Part 1 adds a field to a `pub` struct with three literal sites outside `classloading/` (`access_control.rs`, `vm_init.rs`, `vm/tests/new19_module_access.rs`, one `main_class: None,` each), and on its own it changes nothing observable, so splitting there buys a compile dependency for no behaviour. No fixture assertion is possible first: every hardcoded answer is *correct* for `cratonvm.jdkonly.svc`, so a non-vacuous check needs an **automatic** module and a `--module-version`/`--main-class` jar, and both go red until the bridge lands. |
| `W2-1-strict-refuses-the-synthetic-stream-stack.md` | The stream stack is **SPLIT**. Step 1 is confirmed IN THE TREE 2026-08-12 (the `ForkJoinTask.invoke()` → `getRawResult()` bridge, `native-builtins/src/phases_early.rs:8514-8534`, live on both shipping modes). **Step 2 is adjudicated NOT TAKEABLE ALONE and the record's step ORDER is corrected:** `StreamSupport.stream(Spliterator,Z)` is ambient `SyntheticStub` (`service_loader.rs:3700`) so strict already drops it and the mechanism is available — but `ReferencePipeline.collect` is ambient **`Bridge`** in `native-collections/src/lib.rs:19965`, the one kind `--jdk-only` keeps, so a newly-real pipeline has its terminal yanked back into `stream_elements` and step 2 alone changes nothing observable while doubling what every stream defect must be measured against. Steps 2 and 3 move together, pull-backs first. Remaining blocker is a Spring/Tomcat arm run, not a mechanism. |
| `W2-2-blocked-reader-async-close-wakeup.md` | **FULLY APPLIED 2026-08-12.** `poll_stream_readable` **and** `poll_stream_writable` are `pub` in `native-io/src/net.rs` (both, because `net_phase_e.rs`'s duplicate had grown a direction parameter), and the collapse landed: the three `#[cfg]` arms of `net_phase_e.rs`'s local binding are deleted and both wrappers delegate to `cratonvm_native_io::net`. The `None` arm, the `rc > 0` readiness rule and EINTR-is-not-ready all survive inside the primitive and are re-stated where the binding was. `re1_socket_read_ready` kept as the timeout-0 wrapper `available()` calls. Idiom cleanup, no behaviour change intended; unbuilt. |
| `W4-3-security-getalgorithms-short-list.md` | **NOTHING — all five landed with `W7-63`, re-verified in the tree 2026-08-12** (`wrap_unmodifiable`, `real_md2`, the SHAKE arms, the fail-closed digest default, the `ML-DSA` de-advertisement). The one residual a second pass found — C's **alias** half, where `getInstance("SHAKE128")` still threw while the registry alias row ratcheted green — is closed too. Now covered by a scheduled vector: `RJdkSecurity` moves **61 → 80 checks**. Patch E is still DEAD, §2.4. Row kept for one merge cycle so a taker working from a remembered §2.1 does not redo it. |
| `W5-1-loadlibrary-allowlist-too-wide.md` | **Two of the three, re-read 2026-08-12; every line cite in the old row had rotted.** (a) Arm `BootLoader.loadLibrary` — `native-builtins/src/lib.rs:14068-14073`, `record_boot_loader_library` at `lang_system.rs:3244`, still **zero** callers. **Needs a measurement first, and it must be taken on LINUX — §2.6.** The record now carries the exact patch, plus a separable patch A (a comment pinning the ambient `Bridge` at the registration site) that is behaviour-neutral and landable alone. (b) The Compatible-mode `Runtime.load0`/`loadLibrary0` argument index is **CLOSED — do not re-hand it out**: both arms call `runtime_load_args` (`lang_system.rs:1522`, `:1540`, `:1596`, `:1615`), which reads `args.get(2)` at `:1722`. (c) Returning the resolved path from `load_native_library` is still open, and its prescription is **incomplete**: two of the three success arms report success with nothing opened, so no return value can carry their path — re-costed in the record (nine impls if the signature widens; two sites if an additive resolver is used instead). |
| `W6-10-process-enumeration-syscall-cost.md` | **NOTHING — the inventory row is CLOSED as NO TARGET, 2026-08-12 (`W7-46` §8.3).** It asked for a sixth row in a species inventory that is now retired and internal, for a defect that is fixed and recorded in two live records. W6-10's own description of that target was also wrong: the retired file's table does not stop at row 4, it has a **row 5** naming this very family. Row kept for one merge cycle so a taker working from a remembered §2.1 does not redo it. What W6-10 still needs is a **Linux build**, not a patch — §2.6. |
| `W7-5-registrars-that-never-shipped.md` | **BOTH TAKEN 2026-08-12, unrun.** The §6.3 ratchet is written as `native-builtins/tests/essential_wiring_ratchet.rs` — three tests, because the failure modes differ: registered-at-all, not-a-`SyntheticStub` (which `--jdk-only` would refuse at the door, and these triples have no bytecode to fall back to), and **still registered at the END of the whole boot**, which is the hazard the registrar's own comment asks a human to check by hand since `register_collections_natives` runs after essentials under last-write-wins. Six triples, not §6.3's predicted five: `Stream.forEachOrdered` is deliberately NOT asserted (still served by the interpreter's hardcoded `!has_code` special case), and all three primitive widths of `forEachOrdered` are. **`register_concurrent_skip_list_map_natives`: VERDICT neither wire nor delete — §6.4.1.** §6.4's binary was the error. It is disabled deliberately with the measurement that caused it (natural-ordering overlay ignored the `(Comparator)` ctor and silently dropped puts), a unit test `concurrent_skip_list_map_not_intercepted` *enforces* the disable, two of its triples exist nowhere else in the tree, and `__test_cslm_*` hooks keep its bodies exercised. No code change; the record is corrected. **Still needs:** a CI step for the new test target. |
| `W7-9-minted-interface-abstract-methods.md` | **§8.2 TAKEN 2026-08-12, unrun — and its blocker never existed.** "Blocked on a `NativeContext::invoke_static`" was a grep for a NAME read as the absence of a CAPABILITY: `NativeContext::invoke` (`native-api/src/registry.rs:1791`) resolves by name with no receiver and `native-collections` has been calling the static `Spliterators.iterator` through it on the strict path since 2026-08-05. `Selector.provider()` is registered in `native-io/src/nio_selector.rs`. The record also had the defect on the wrong receiver: the predicted `AbstractMethodError` only ever applied to two `java/nio/channels/Selector` mints that are BOTH `register_synthetic_overrides`-only, while `selector_open_native` allocates a real `sun/nio/ch/SelectorImpl` with no constructor, so `Selector.open().provider()` answered **null** in Compatible mode too. Gate: `RJdkNio` 81 → **84**. **§8.1: precondition 1 was FALSE and is now SATISFIABLE** — `Stream.forEachOrdered(Consumer)V` is registered on the shipping registrar (`native-collections/src/lib.rs::register_stream_natives`, ambient `Bridge`, `vm_init.rs:2434`); the deletion is deliberately NOT taken because the interpreter hack still wins (it precedes `resolve_native_for_dispatch` in the same arm), which makes the registration behaviour-neutral and the deletion gated on one run rather than on a registration. `native-builtins/tests/essential_wiring_ratchet.rs`'s "next row to add" comment is now stale. **§8.3 re-checked: blocker HOLDS** — unifying the two `DatagramChannel` layouts is a renumber, and `W7-77` records that on a fabricated class each slot map IS the layout, so it breaks the only receiver that exists; it also needs `native-io/src/lib.rs`, not `nio_selector.rs`. |
| `W7-10-processhandle-interface-stub-bodies.md` | **§7.3 APPLIED 2026-08-12, unbuilt — NOTHING left in this table.** `mk("commandLine", "()Ljava/util/Optional;")` is in `synthetic_stub_ctor_methods`' `java/lang/ProcessHandle$Info` arm (`classloading/src/class_manager.rs`), so §4's registration is no longer real-JDK-only; both halves were re-verified in the tree before editing (the registration is live in `phases_late.rs`, the carrier's list really was five names). It already HAD its scheduled witness — `RJdkStrict.processHandleInfo` asserts the sorted `$Info` surface as an **exact six-name list**, the only shape that can see an omission — and a mirror assertion over the fabrication path itself was added to `process_handle_native_fallback_has_verifier_visible_callable_shape`. No ratchet moves: this is a fabricated method declaration, not a registration. Row kept for one merge cycle so a taker working from a remembered §2.1 does not redo it. §7.1 (`onExit` on a minted handle) and §7.2 (the synthetic-JDK empty stream) are the record's remaining open work and are not §2.1 items. |
| `W7-14-fjp-common-factory-bound-by-name.md` | An explicit **human decision**, not a patch: under `--real-jdk`, `commonPool().getFactory().getClass().getName()` still answers a class JDK 25 does not declare. |
| `W7-15` · `W7-21` (crypto) | **BOTH APPLIED 2026-08-12 (third pass), in `native-builtins/src/jca/cipher.rs`.** (1) W7-21 Patch A took the **deletion** option: the two 2-arg `KeyGenerator.getInstance` overloads, `register_keygen_dispatch` and its call site are gone, leaving a tombstone. `keygen_default_bits` was NOT made `pub(crate)` — nothing needs it. The real path they bypassed is live: **thirteen** (the records say twelve) `SunJCE` `KeyGenerator` services under real JDK class names, plus the `sun/security/jca/GetInstance` named-provider and search bridges, both gated on `ec_real` whose `route_ec_to_real()` disjunct is default ON. This also makes `provider_chain.rs:1401`'s "`KeyGenerator` is NOT natively intercepted in `--real-jdk`" true. (2) W7-21 Patch C: `SecretKeySpec.<init>` now refuses a null or zero-length key with `IllegalArgumentException`, with the null test **before** `obj_arg` — `obj_arg` raises `NullPointerException`, which the caller's `catch (IllegalArgumentException)` does not catch, so the ordering is the fix, not a detail. Both are deliberate Compatible-mode behaviour changes; both are covered by three new `RCrypto` checks (`keygen2arg`) that fail on the pre-fix behaviour. W7-15's Patch 2 line cites (`:3346`/`:3358`) are retired with the code. The ChaCha20 half of both records was already closed — §2.0's `W7-38` row — that record retired in the second pass and its vector, `RChaCha20Cipher`, now runs and passes in both arms. **(3) Fourth pass, same day: Patch A's deletion made `RCrypto` RED, and the deletion was still right.** Walking the real path one frame further reached `Cipher.getMaxAllowedKeyLength` → `getstatic JceSecurityManager.INSTANCE` → a `<clinit>` that dies with `NullPointerException` in `Set.of(Option.DROP_METHOD_INFO, …)` because **every `java/lang/StackWalker$Option` constant is null in this VM** — measured identically on the pristine-dev `44044c7e2` control binary, so pre-existing, not this wave's. Fixed in-lane by natives for `Cipher.getMaxAllowedKeyLength`/`getMaxAllowedParameterSpec` answering `Integer.MAX_VALUE`/`null` (both measured on HotSpot 25 on this host under the default unlimited policy), keeping that clinit off the path; they reuse `tokenize_transformation` so they refuse exactly the malformed transformations the real method refuses and no more. The pre-existing `JceSecurityManager.getCryptoPermission` native could never have served this — it is reached *through* the getstatic that runs the failing clinit. **Out of lane, open:** `phases_late.rs:5222-5255` registers `StackWalker$Option` field natives for 3 of 4 constants (no `DROP_METHOD_INFO`, added JDK 22) and none is consulted for a `getstatic` on real class bytes. |
| `W7-18-structured-task-scope-jep505.md` | **B PARTIALLY APPLIED, C DECLINED WITH A REASON — 2026-08-12, unrun.** The "silently" is gone from B's hazard: the boot-path order is re-derived (`register_phase67_natives` → `register_phase_d_natives` → `register_jdk25_concurrency_natives`, all inside `register_synthetic_overrides`, which sets ambient `Intrinsic` — registrar #2 sets no category of its own), the two triple sets were compared, and the JEP 505 surface is **not** shadowed today; a new `#[test]` (`w7_18_jep505_surface_is_not_shadowed_here`) fails if that changes. The **third** registrar nobody had counted — `util_concurrent_ext.rs::register_pd_structured_concurrency`, with an INCOMPATIBLE `$Subtask` slot convention — is retired to a tombstone, provably inert (its triples were a subset of #3's; the only two it ever won are covariant-return `join()`s on classes JEP 505 deleted). It was the landmine: deleting a JDK-21 triple from the winner would have un-shadowed a body writing `$Subtask` state into **slot 3**, which is the `callable` REFERENCE in the winning layout — an `Int` in a slot the collector scans as an oop, i.e. the §5 species, not a wrong answer. `t3_impl.rs::register_t31_structured_concurrency` is already a tombstone for that defect. **NEW FINDING while doing it:** the two *surviving* registrars disagree on `Subtask.State`'s numeric encoding in the same slot — `jdk25_concurrency.rs` (which owns `get`/`state`/`exception`) has `SUCCESS=1, FAILED=2`, `phases_late/concurrent.rs` (which owns `fork(Runnable)`/`join()Object`/`isCancelled`) has `SUCCESS=2, FAILED=3`, and both files carry a comment claiming they "agree by value". A successful `fork(Runnable)` subtask therefore reads back as FAILED. **Still registered, deliberately:** the ~24 `ShutdownOn*` triples, `$Config`, `Joiner.policy()I` and the JDK-21-only `StructuredTaskScope` methods — ~20 `#[test]`s in a blocking gate pin them by triple, and the only mode any of this can be observed in (`--synthetic-jdk`) has never been run, so deleting means rewriting a gate against a guessed answer. Both B's remainder and C need **one** run: a `--features synthetic-jdk` binary in `--synthetic-jdk` **mode** (W7-50's binary was run under `--jdk-only`, where none of it registers). `SCOPE_OWNERS`/`SCOPE_JOINERS` (and `SCOPE_FORKS`) are still address-keyed. **Patch A's "preferred" form is DEAD — §2.4.** |
| `W7-20-refusal-laundered-into-wrong-answer.md` | **Half settled by W7-62.** The kind map is re-frozen. `jdk-only-bridge-ratchet.json` still fires, deliberately, with its derived movement in its `note`; the third stale ratchet `native-builtins/tests/stub_ratchet.rs` (+6) fires too. **Both still need the census, and 2026-08-12 sharpened WHICH census.** One command settles both JSON gates — `JAVA_HOME=<jdk25> bash regression-suite/bridge-ratchet.sh` — and it **must run on Linux**: both artefacts are keyed `<jdk-feature>/<os>`, the gate scripts derive the OS half from the running host, so on this Windows host they look up `25/windows`, find no baseline and exit 2 ("REFUSING"), which is neither a pass nor a fail. The red now has **three separable causes** and must not be re-frozen as one number: (a) this record's retag +10/+1/−2; (b) the four new scalar `StringBuilder.insert` overloads, up to **+12** Bridge rows and twelve kind-map rows but **zero** on `stub_ratchet` (ambient `Bridge`, outside that population); (c) anything else in the 182 commits, which is a finding to attribute. The unlanded 7-row `java/io/Print*` retirement is a fourth, *predicted*, contribution — do not pre-freeze for it. Also: `bridge-ratchet.sh` censuses `--real-jdk`, so a synthetic-mode-only change cannot move it by any amount and needs a `--dump-native-registry` diff from a `--features synthetic-jdk` binary in `--synthetic-jdk` MODE instead. |
| `W7-22-shadow-retirement-logging-and-time.md` | The 7-row `java/io/PrintWriter` retirement is **still not applied, and the reason changed 2026-08-12**: it is now a one-run work item, not an open question. The reinstatement check W7-25 prescribes has been run and comes back clean — all seven registrations sit under `register_printstream_fallback_natives`' ambient `Bridge` (`logging_shims.rs:118`…`:419`), and the only other registrar of those triples (`lib.rs:22864-22905`, ambient `Intrinsic`) is inside `register_synthetic_overrides` and does not register on either shipping mode — so the retag WOULD fire. What blocks it is that `java/io/Print*` is Compatible-visible, so seven rows move three artefacts frozen for `25/linux` (`jdk-only-bridge-ratchet.json`, `stub_ratchet.rs`'s `SLACK = 0` baseline, `jdk-only-kind-map-25-linux.tsv`); entries and re-freeze must land in one commit from a Linux build, and §2's Compatible `PwProbe` arm is still untaken. `retired_shadow.rs`'s module docs now carry this so it is not re-derived. **The ordered recipe is now written out as §2.1 of the record — eight steps with the exact commands — and a taker with a Linux build should read that and nothing else.** Two corrections it carries: the third artefact's arithmetic in §2 ("1,038 → 1,045") is **STALE and must not be pasted** — `stub_ratchet.rs` now holds TWO baselines split by the `management` feature (1263 / 1253, `SLACK` still 0), both moving +7 and both to be pasted from their own configuration's printed recount line, never hand-derived; and `sh regression-suite/bridge-ratchet.sh --update-baseline --note "…"` re-freezes the bridge ratchet AND the kind map from ONE census, so the "three artefacts" are two commands. On a non-Linux host both gate scripts exit **2** ("REFUSING"), which is why this is held rather than attempted. 29 `PrintStream` rows stay blocked on real `PrintStream` state. **§4's named cause and its repair are DEAD — §2.4.** |
| `W7-27-thread-exit-java-cleanup.md` | §10C: the main/primordial thread never gets `Thread.exit()`. **Fully diagnosed 2026-08-12 and written out as an exact two-file patch, deliberately not landed** — and the record's old pointer to "the file that owns the other `clear_tlab_addr` call site" was **wrong**: that is `vm/src/native/jni.rs`'s foreign-thread detach, and the main thread never reaches `clear_tlab_addr` at all. The main thread's death is expressed in `vm-cli/src/main.rs::run()` (a different crate), which contains no `mark_dead`, no monitor notify and no Java teardown; `run_thread_exit_shared` is module-**private** to `vm/src/vm/vm_exec.rs`, so the third call site needs a visibility change there plus a call in `vm-cli`. Insertion window and receiver are pinned in §10C (after the `catch_unwind` unwrap, before `begin_main_thread_blocking_region`, receiver read from the **registry** not from `main_thread.java_thread_obj`). **Recommended order: measure W7-23's container flip first** — two first-time-ever Java-teardown changes in one unmeasured wave cannot be bisected. **New residual filed at §12.2:** the uncaught-handler side table is not cleared on a clean death, so `getUncaughtExceptionHandler()` on a normally-terminated thread answers the stale handler in `--real-jdk` and HotSpot's answer in `--jdk-only` (the `Bridge` native yields there) — §9's point 4 is true of the dispatch and false of the getter. **Now covered by a scheduled vector:** `RJdkExecutors.threadExitCleanup()` (+13 checks). **That vector's first run FAILED, and §13 is the new defect it found — older than this record's patch and independent of it: `Thread.start()` has never enforced start-at-most-once.** Measured, both VMs: HotSpot throws `IllegalThreadStateException`, CratonVM does not and **re-runs the body** (`ran` 1→2, a second OS thread for a retired `Runnable`); the still-RUNNING half is missing too. Two independent causes, either sufficient: `Thread.start()V` is natively shadowed on **both** registration sets in `lib.rs` (the real-JDK one's own comment says the JDK implementation "must run … checks already-started" and then calls the native anyway), **and** the field that check reads is dead — with `--add-opens java.base/java.lang`, `holder.threadStatus` goes 0→2 on HotSpot and 0→**0** here, so a guard on it would be inert. **Authoritative state is the VM `ThreadRegistry`** (`thread_run_state`; it retains dead entries and `mark_dead` only flips `alive`), which is exactly why `Thread.getState()` is correct — it bypasses the field. Exact unapplied patch in §13.6 (one guard in `native_thread_start0`, covering all four registrations and the `ThreadContainer` route). **Narrowing is not new:** `ThreadPoolExecutor.addWorker` already throws `IllegalThreadStateException` off `getState() != NEW` in real bytecode, off the same registry read; fresh-mirror misresolution measured **0/2400** under forced address recycling. Synthetic mode reads an on-mirror marker instead (slot 2 `Long`), because its lookup falls back to the unguarded pointer walk — the DoHead flake — and the frozen binary cannot run `--synthetic-jdk` to check. Adjacent and NOT to be bundled: `Runtime.addShutdownHook` hooks are registered and never run. |
| `W7-29-jca-advertise-implement-gaps.md` | **NOTHING — same five as W4-3's, and they landed with `W7-63`; re-verified in the tree 2026-08-12.** Residual 2's alias half (`MessageDigest.getInstance("SHAKE128")`) was the only one still open and is now closed. Its assertions run in `RJdkSecurity` (**61 → 80 checks**) rather than only in `probes/`, which `run.sh` never runs. Row kept for one merge cycle, as W4-3's is. |
| `W7-30-stub-ratchet-boot-path-scope.md` | **Both taken 2026-08-12, unrun — and the 2026-08-11 fix turned out to have been INERT the whole time (§9).** The source witness §4 calls "worth more than the widened number it protects" located `vm_init`'s real-JDK arm with a `contains` match that hits a **comment quoting the attribute**, 44 lines above the attribute; it scanned 39 lines of the *sibling synthetic* arm, observed **8** registrars instead of 48 — all 8 modelled, so zero unmodelled — and passed while asserting nothing about the arm it names. The identical code sat in `duplicate_registration_gate.rs`: one locator defect, two files, neither able to notice by disagreeing with the other. Fixed (`starts_with` on the trimmed line: a comment can *contain* an attribute, it cannot *start with* one) plus the assertion that would have caught it — every name in `VM_INIT_SEQUENCE` must actually be OBSERVED (38 missing before, 0 after). **The model was never stale; only the instrument was**, so no baseline moves. (b) collapse: DONE — one model in `native-builtins/tests/common/vm_init_boot_path.rs`, included by three test targets. (a) move to `vm/tests/`: **half done and honestly labelled.** `vm/tests/stub_ratchet.rs` is deliberately *not* a second stub census (two count-ratchets over one VM is the shape that already cost weeks); it closes the half needing no census — the two `vm`-crate registrars contribute zero stubs and zero *unscoped* rows, which is not self-evident (their 18 registrations state no kind and `effective_category` defaults to `SyntheticStub`; only `vm_init`'s `set_category(Bridge)` wrapper saves them, and nothing asserted that wrapper existed). **Still needs:** a registration-only helper extracted from `SharedVm::new` in `vm/src/vm/vm_init.rs` before the inline registrations — and the floor-by-one — can be counted; and a CI step for `vm/tests/stub_ratchet.rs`. Two further population holes recorded, not fixed: the scan is name-shaped (`init_service_loader_bootstrap` is invisible to it) and the 8 inline registrations are ratcheted but uncounted. |
| `W7-34-formatter-family-residuals.md` | **APPLIED IN SOURCE 2026-08-12, unrun — and the record's own trap was half a trap.** Both registrars are patched: `format` reads the receiver's `Locale` from field 1 in each. The order question is settled by the call graph, not by a census: registrar 1 (`register_string_format_real_jdk_natives` ← `register_essential_natives_with_shims`, ambient `Intrinsic`) is the **only** one that registers on either shipping mode, because registrar 2 (`register_formatter_natives` ← `register_enterprise_final_natives` ← `register_synthetic_overrides`) is `#[cfg(feature = "synthetic-jdk")]` and reached only on the `use_synthetic_jdk` arm — so "the last one wins" is true in `--synthetic-jdk` only, and no `--dump-native-registry` diff was needed. One piece of the patch was **deliberately not taken**: registrar 1 does not get the `(Appendable,Locale)V` ctor, because the real `java.util.Formatter` ctor already writes slots 0/1 *and* `zero`, which no native writes and real bytecode reads. Covered by `RStrings` (`CORE_CLASSES`). New finding recorded there: `%t`/`%T` renders months, weekdays and AM/PM from **hard-coded English tables**, so every `SimpleFormatter` date is English in any locale. |
| `W7-37-differential-throwable-and-vm.md` | **All four adjudicated 2026-08-12 (Part 3), unbuilt and unrun.** Item 2 APPLIED and it was **two** sites, not one — `jit_aastore` *and* `jit_checkcast` both minted their throwable directly, which **corrects this record's own claim** that the funnel covers "the JIT cast helper": it did not, so after Part 2 landed the interpreter and the JIT printed *different* text for the same cast refusal. Item 3 DECLINED — item 2 removes its premise (all four raise sites now reach one builder) and it spans `opcodes.rs` for no behaviour. Item 4 needed nothing, re-confirmed. **Item 1 is the live row and its prescription is half wrong:** blast radius is **30 sites in 13 files**, 8 of them un-owned, and most of the thirty are free-text refusals (checked collections, `AtomicReferenceFieldUpdater`, `nio_file` views, `xnio_async`) with no two operands to carry — so a two-`ClassId` payload cannot *replace* the string variant, it has to be a second additive variant used by the four VM-minted cast sites only. Scope it that way. **The record's own "no in-tree caller asserts either message" is closed:** `RExceptions` (`CORE_CLASSES`) now asserts both exactly, each re-asserted past the JIT threshold — **and one of them is RED (Part 4, measured on the wave binary 2026-08-12)**. It is not a wording drift: the ASE tier-parity check reads `cold=[java.lang.Integer] hot=[no-throw]`, i.e. the JIT-compiled `aastore` performs the store and refuses nothing, so a `String[]` slot ends up holding an `Integer`. Cause: `jit/src/x64/bytecode_walk.rs:1778` lowers `aastore` **inline** and never calls `self.helpers.aastore`, so `jit_aastore` — the helper item 2 routed through the funnel — is dead code on x64; its arm's stated premise ("the helper does NOT enforce the ASE check … no regression") was falsified from another crate when the covariance check landed, and a premise in a comment is not a compile-time link. `checkcast` (0xc0) *does* call its helper, which is the whole reason the CCE half is green and the ASE half is not. **The codegen fix is written out as an exact patch in Part 4 (out of the jdk-only lanes' files):** keep the inline null/bounds checks, call `self.helpers.aastore` for the rest, map `jit_dispatch_threw`'s 0/1 onto the `i64::MIN` convention with `SHL RAX,63` so `emit_post_invoke_exception_check` routes it through a reason-9 precise frame into the method's own `catch`, and set `emitted_checkcast_throw` or the helper's "no thread" arm silently stores anyway. It reinstates a Rust-boundary call on the hottest reference-store path (R20 / HIGH-5's win) and must be A/B'd. |

### 2.2 Live residuals inside an otherwise-fixed record

The headline is closed and the vector passes; a specific sibling case is not.

| Record | The live residual |
|---|---|
| `L15-nestmate-access-field-and-constructor.md` | **Both residuals CLOSED IN SOURCE 2026-08-12, neither built nor run.** (1) `Constructor.newInstance` now has the member-modifier gate, mirroring HotSpot's `checkAccess(caller, clazz, clazz, modifiers)` through the same `caller_may_access_member` the field and method paths use. It is a **NARROWING** — read the record's *Blast radius* section, which walks every reflective construction in the suite and shows all of them are public, already-`setAccessible`, or already refused. (2) Hidden classes are nestmates again: the record's *"`NativeContext::is_hidden_class` does not exist"* was a grep for the wrong name — `is_class_hidden` exists with real VM and mock impls. Vectors: `RJdkReflect` 64 → **67** (three constructor checks placed ABOVE the first `setAccessible`), plus two paired unit tests for the hidden arm. |
| `L16-classnotfound-vs-noclassdeffound-shapes.md` | **CLOSED IN SOURCE 2026-08-12, unbuilt and unrun.** The residual was re-verified before being touched — `loadClass`' step-3 CNFE names the string it was given, and `native_class_for_name` gave it the descriptor and propagated it unchanged. `native_class_for_name` now corrects the NAME on the way out (`for_name_cnfe_name` / `for_name_rename_array_cnfe`, four call sites) rather than restructuring resolution; **the record's own prescription — resolve the element separately, HotSpot's structure — is DECLINED in place**, because it puts a second arbitrary-Java `loadClass` dispatch on a path that already works and buys nothing observable. Only `java/lang/ClassNotFoundException` is rewritten and only for a reference-array descriptor: a `NoClassDefFoundError` naming a missing *supertype of an existing element* is the answer `Class.forName` should give and is untouched. **The reason 43/43 never closed it is the lesson: `RJdkFailure` asserts the message only on the first, non-array probe** — a vector that asserts a throwable's type and not its text cannot see a message defect. Now covered in `RExceptions` (`CORE_CLASSES`, **default** invocation, not only `--jdk-only`): shape, message, the two-dimensional form, and two `[I` / `[Ljava.lang.String;` controls. Five `RJdkFailure` assertions (43 → 48) are recorded as exact Java in the record, not applied. **Adjacent and NOT fixed:** the explicit-null-loader arm refuses `Class.forName("[Ljava.lang.String;", false, null)` outright, because `is_bootstrap_class_name` tests a package prefix that no `[…` name has — a wrong *refusal*, not a wrong message. |
| `W2-1-strict-refuses-the-synthetic-stream-stack.md` | **Residual 4 CLOSED 2026-08-12** — `drain_spliterator_inline` now falls back to `drain_spliterator_inline_via_real_iterator` (`Spliterators.iterator`, per-element interleaving and short-circuit preserved). It is still unreachable under strict (`stream_has_chain` gates all five `stream_pull` entry points), so this closes a shape, not an observable defect. **Residual 3 half settled:** the seven names are split 5 live / 2 dead by REGISTRAR CHAIN, not by owning crate — `HashMap$Entry` and all five `ServiceLoader$Itr` mints are `register_synthetic_overrides`-only; `IteratorEnumeration` (`keystore.rs:2666`), `CompletedFuture` (`native-io/src/lib.rs:19305`) and the three `Atomic*FieldUpdater$RustJvmImpl` (`atomic_updater.rs::alloc_impl`) all ship. Open per live row: refuse-or-fabricate, which needs the `--dump-class-origins` run `no_image_receiver.rs` prescribes — and `$RustJvmImpl` is the suspect, because `ensure_class_initialized` fabricates rather than failing and `class_manager.rs:10611`/`:13799` give those three names a synthesised supertype and interface list. **Residual 2 is CLOSED and its diagnosis was wrong — §2.4.** |
| `W2-2-blocked-reader-async-close-wakeup.md` | The **fourth** surface is **SETTLED from source 2026-08-12 and needs no run**: `register_synthetic_socket_stubs` (`native-builtins/src/phases_early.rs:18029`; the reads are at `:18512-18682`, **not** the `18236-18320` the old row cited) is reached only via `register_synthetic_overrides`, and its caller early-returns on the default `io.real_net_sockets`, so it is unreachable in both shipping modes and absent from a default build. No last-write-wins contest either — the fixed surface uses the different class name `java/net/Socket$SocketInputStream`. What was left was a decision (fix to match `re1_read_close_aware`, or delete), not an investigation. **DECIDED AND APPLIED 2026-08-12: fixed, not deleted** — all three reads call `net_phase_e::re1_read_close_aware` (now `pub(crate)`; same `s2_registry().streams` map and same `sid`, so the mechanism transfers unadapted) and map its `Interrupted` carrier to `SocketException("Socket closed")` via `re1_socket_exception`. Deleting would have traded an unwakeable read for an `UnsatisfiedLinkError`: these are the tree's only `java/net/SocketInputStream` registrations and its methods have no bytecode. The deadline is read back off the socket's own `SO_RCVTIMEO` (`sis_read_deadline`), without which the poll-then-read shape would have removed one hang and added another on every `setSoTimeout` reader. Superseded as a *family* by `W7-53-blocking-close-family.md`. |
| `W3-4-forkjointask-status-flags-and-the-eager-default.md` | The eager-fork flip's blast radius on the Spring/H2 slice is unverified — and the two Rust guards for the now-non-default lazy path are **vacuous** (`apps/fjp_probe/` does not exist, so both tests early-return). |
| `W4-1-publiclookup-allowedmodes-never-checked.md` | **CLOSED 2026-08-12.** W7-62 moved the six `lk_find_*` tests but left two of the same species: `test_lk_lookup_registered` and `test_lk_public_lookup_registered` (`native-builtins/src/classloader.rs`) asserted registrations on `MethodHandles$Lookup.lookup()` / `.publicLookup()`, and **no real JDK declares either method on `Lookup`** — all three of `lookup`, `publicLookup`, `privateLookupIn` are statics on `MethodHandles`. Both now assert the LIVE `java/lang/invoke/MethodHandles` triple first and keep the `LK_CLASS` twin as a labelled synthetic-jdk-only row. W7-62's "it IS registered" was right about the fact and wrong about what it licensed: **registered is not reachable.** **Two of its own claims are struck — §2.4.** |
| `W4-2-unnamed-accessor-bypasses-encapsulation.md` | **FIXED IN SOURCE 2026-08-12, and VERIFIED BY A RUN 2026-08-12 (second pass) — the "unrun" is discharged.** A/B on `RJdkModule`: the pristine-dev control dies with `AssertionError: Exported[] must report its component's module, got module java.base` at `RJdkModule.java:807` (`arrayModules`), and the wave binary passes the whole vector, 163 checks, byte-identical to HotSpot. It was live exactly as recorded — `synthesize_array_class_for_loader` (the only array-synthesis site tree-wide) wrote `module_name: Some("java.base")` in its `Class` literal regardless of component type. It now derives the module from the **component class**, keeping `java.base` only for a primitive component; multi-dimensional arrays inherit through the inner array, as the defining loader already does. Marker `// JDK-ONLY-NOTE (W4-2)`. Vector: `RJdkModule::arrayModules`, 9 checks including three java.base **controls** that passed against the hardcode too, so a fix that answered "unnamed" for everything does not survive them. Still open and deliberately NOT bundled: `package_of("[I")` yields `""` where HotSpot's `int[].class.getPackageName()` is `"java.lang"` — different function, no measured consumer. |
| `W6-5-vacuous-tests.md` | **NEW ROW 2026-08-12.** The origin record for this campaign's most productive finding species: *a test that passes on the path where the fix does not matter.* Its own repairs are landed; what is live is (a) §3.2's **16** probe fixtures still missing under the gitignored `apps/` (2 of them not reconstructible at all — they need third-party jars), held by `vm/tests/probe_fixture_census.rs`'s two-directional ratchet, and (b) §3.4's `RJdkViews` (67 checks), mutation-proved capable of failing on HotSpot and **still never run under CratonVM**. Round 3 (2026-08-12) swept the fifteen fixtures under this lane and found the species alive in nine of them; see W7-51's row for the disposition and W7-60's for the ratchet. The lesson that keeps re-earning itself is in its §3.4 and W7-60 §5: **a constant assertion gets written while repairing constant assertions** — four occurrences now, in four lanes, every one by an author working on this defect class. |
| `W6-2-module-serviceloader-provider-factory.md` | **ONE row, not two — row (1) was fixed 2026-08-12 by `W7-85-serviceloader-stream-validation.md`,** which mirrored the factory return-type gate onto the stream path, matched HotSpot's message on both, and swept the rest of the file. `RJdkModule` is now 104/104 on HotSpot with a negative fixture; the RED it was measured against had `stream().map(Provider::get)` handing out a `String` for a service interface. **Its last row is now ARMED IN SOURCE 2026-08-12, unrun:** the constructor-form subtype check and the sibling W7-85 found beside it — the JDK's **public** no-arg constructor requirement — both now enforced on **both** provider paths in `native-builtins/src/service_loader.rs` (`constructor_form_is_subtype`, `constructor_is_public`, `no_public_no_arg_ctor_error`, which carries `getConstructor()`'s `NoSuchMethodException` cause because this is the three-argument `fail`). **The census the row waited on was taken by reading, not by running:** JLS §7.7.4 makes both rules compile-time errors on the `provides` directive, and every boot-module `provides` clause is javac output, so the population either gate can refuse is zero by construction — the same argument W7-85 used for the factory return type. The residual is only "does *our* `isAssignableFrom`/`getModifiers` disagree", bounded by the `module_declared` gate (the classpath path, i.e. all of Spring/Tomcat/ES/WildFly boot, never enters), by "never refuse on an unanswered question", and by strict mode dropping the file at registration. Negative fixture: two new services (`Unsub` ← `NotSubProvider`, `Ctored` ← `HiddenCtor`), both built with the `modules-overlay/` two-source device because javac refuses to express either shape. **Needs one out-of-file `run.sh` edit** — `compile_modules` `javap`-ground-truths only `WrongFactory`'s overlay, and an unapplied overlay makes the new checks fail on BOTH VMs. The classpath path's own copies of both rules stay unarmed and are a separate row. |
| `W6-6-nativelibraries-load-fabricated-success.md` | The boot-loader case cannot fire on either road, because `BootLoader.loadLibrary` is a no-op (`native-builtins/src/lib.rs:14068-14073`) and `record_boot_loader_library` (`lang_system.rs:3244`) has zero callers. W5-1 owns the arming, and it **needs a measurement** (§2.6). That no-op's `NativeKind` is **ambient** and must stay `Bridge` — a `SyntheticStub` there would be dropped under `--jdk-only`, restoring the Linux boot-class native-library lock the short-circuit exists to avoid. **Measured 2026-08-12, not inferred:** `scripts/baselines/jdk-only-kind-map-25-linux.tsv:9400` is `bridge`, `kind_stated=0`, and is the only row for that triple, which is also the single-registrar proof. Do **not** pin it with `register_with_kind`: behaviour-identical, but it flips `kind_stated` 0→1 in that baseline, re-freezable only by `bridge-ratchet.sh` on Linux — the guard is a comment, drafted as patch A in W5-1. **New residual, found 2026-08-12:** the two roads' admission lists have drifted — `System.loadLibrary` gained `jdk_image_ships_library` (the file-presence test added for `awt`) and `NativeLibraries.load` never got it, so the java.desktop family loads on one road and throws on the other. Two-line patch in the record, unmeasured because this road is inert on Windows. |
| `W6-9-complete-erases-the-abnormal-record.md` | **NEW ROW 2026-08-12 — its §7.5 named a divergence that had never been indexed.** `ForkJoinPool.invoke(task)` reads `fjp_state_get` while `join()` reads `fjp_state_get_checked`, so on a **cancelled** task `pool.invoke` hands back the cached result where the real `ForkJoinPool.invoke` — `externalSubmit(task); return task.join();` — reaches `reportException` and throws `CancellationException`. Two accessors, one task, opposite verdicts: the same shape as §7.1 one level up, and a fabricated success where the spec mandates a failure. **Not the §7.5 patch, which landed in `e643b5893` and is separately recorded as INERT** (`fjt_has_own_raw_result_slot`'s side-table arm names three abstract classes and `method_exists` walks the superclass chain, so no Java receiver reaches it — W7-48 §3). Fixing this one changes what every cancelled-task `pool.invoke` call site sees from a value to a raised exception, so it needs the blast-radius survey §4 did for `complete`, and it has no vector: `regression-suite/src/RJdkForkJoin.java` never calls `pool.invoke` on a cancelled task. **Do not re-seed the `duplicate_registration_gate` number while adjacent** — §8.2 and §2.6 both forbid it without a real run. |
| `W6-8-method-invoke-exports-gate.md` | `unreflectSetter` on a trusted-final field is unchecked (re-read 2026-08-12 and deliberately still not written — the JDK rule is `final && (static \|\| hidden \|\| record)`, and guessing it refuses the `setAccessible`-then-`unreflectSetter` idiom every deserialization framework uses); the module half of `find*`/`unreflect*` is absent by design; `unreflectSpecial`'s `specialCaller` conjunct is unenforced. **The positive vector is HALF closed 2026-08-12:** `RJdkHandles.accessChecks()` now asserts all three polarities of the `unreflect*` MODE gate (refuse / allow-with-PRIVATE / allow-after-`setAccessible`, in that order so the flag cannot do the work), 51 → **54** checks. The headline **`Method.invoke` exports gate now HAS its witness** (landed 2026-08-12 by the lane that owns `RJdkModule.java`, unrun): `encapsulation()` invokes `EnGreeter.greet()` — public method, public final class, declaring class in the **non-exported** package — and requires `IllegalAccessException`, with `Exported.visible()` as the must-succeed control. The receiver comes from `ServiceLoader`, **not** from `internal.getDeclaredConstructor().newInstance()`: the line above already asserts that call is refused, so the obvious spelling would have caught the CONSTRUCTOR gate's exception and read green whether or not `Method.invoke` had a module gate at all. |
| `W6-12-stampedlock-split-brain.md` | The `Collections` fidelity residual, **structurally confined to `--synthetic-jdk` by construction** — `phases_early`'s identity bindings reach the registry only through `lib::register_synthetic_overrides`, which is a `#[cfg(not(feature = "synthetic-jdk"))]` no-op shim (`vm/src/native/builtins.rs:29`), so they are *compiled out* of both shipping binaries rather than out-voted. Order would not have saved it — their ambient kind is `Intrinsic`, which `JdkOnly` does **not** drop. A feature binary now exists (W7-50, 63/7); what has never been run is one in the `--synthetic-jdk` **mode**. The `Phaser` fix is unproven for the same reason. |
| `W7-1-treemap-views-and-iterator-remove-contract.md` | Families 3 and 4 untouched. **`native_map_key_itr_next` CLOSED IN SOURCE 2026-08-12** — it raises `NoSuchElementException` past the end, gated by `RJdkViews.keyIteratorExhaustion`; note that `null` was never merely a wrong answer there, it is a legitimate key-set element. Two left **on purpose**, each with its reason at the patch site: `sort`/`replaceAll` do not bump `modCount` — the comodification machinery it would extend has still never been executed, and the record's own open question is whether the FIRST CME change breaks Spring Boot or Tomcat, so stacking a second set of bumps costs the bisect; and the view cache has no version stamp, where a missed mutation site is a silently stale view. |
| `W7-2-primitive-stream-terminal-surface.md` | **§7.2 WRITTEN IN SOURCE 2026-08-12 except `spliterator()`** — `anyMatch`, both `reduce` overloads, `findFirst`/`findAny`, `sorted` and `distinct` are registered for the three primitive streams (§9), and the `iterator()Ljava/util/Iterator;` bridge now BOXES: §3's "answers EMPTY" was half stale, the live half was a bare `Value::Int` handed back where a reference is declared. **§9.2's boxing fix was applied to the SHADOWED copy and changed nothing; the reachable half is fixed 2026-08-12 in §9.5, the first section of this record with a VM run behind it.** That descriptor is registered twice — `native_stream_empty_iterator` (`native-builtins/src/streams.rs`, all five stream interfaces) and `native_stream_iterator` (`native-collections/src/lib.rs`, `BaseStream` + `Stream` only) — and native-collections registers LAST, so a call site whose static type is `BaseStream` (which is where `iterator()` is declared, hence how every `IntStream` reaches it) lands on the native-collections one. Measured under `--jdk-only`: the iterator handed back is `java.util.Arrays$ArrayItr`, never the `ServiceLoader$Itr` §9.2 fixed; `o != null` is TRUE and `o.getClass()` NPEs on the same word; `Int`/`Long` streams yield 0 typed elements out of 3/2, `Double` and reference streams yield all. `native_stream_iterator` now runs `box_primitive_stream_elements` (itself made fallible — its old `.ok().flatten().unwrap_or(Value::Object(None))` laundered a refusal into a silent `null` element — and `box_primitive_result` gained the missing `Float` arm). The new raise is NOT reachable on this path: `boxed()` already proves `Integer.valueOf` succeeds on this exact receiver in this exact mode, and it propagates rather than being swallowed (W7-65's 25 swallowers sit on `int_stream_elements`, which this path does not use). Source only — this lane could run the frozen binary but not rebuild it. **`spliterator()` is REFUSED, not deferred, and §3's "one registration closes four more members" is struck:** `tryAdvance(IntConsumer)` is abstract on `Spliterator$OfInt/OfLong/OfDouble`, so the default-method-preference escape hatch the `java/util/Spliterator` natives rely on does not exist, every real JDK primitive spliterator becomes a candidate receiver, and the bail branch has no correct answer. `takeWhile`/`dropWhile`/`mapMulti`/`concat` therefore stay dead one frame deeper. `DoubleStream.min`/`max` still use `f64::min`/`max` and are still NaN-wrong. Gate: `RJdkViews.primitiveStreamSurface`. |
| `W7-3-format-conversions-and-stringbuilder-bounds.md` | **ALL FOUR CLOSED IN SOURCE 2026-08-12, unrun** (`native-builtins/src/lang_string.rs`). `append(CharSequence,int,int)` range-checks (`checkRange` → `IndexOutOfBoundsException`, null substituted to `"null"` FIRST so the check runs against 4) and the pinning test is **corrected in the same pass** — `…_clamps_out_of_range` becomes `…_rejects_an_out_of_range_window`, both polarities, plus a null-order test. `appendCodePoint` no longer truncates, and its "trade-off" was a misreading: `0xD800..=0xDFFF` **are** BMP code points, so the JDK appends a lone surrogate verbatim and `Character.toChars` never sees it — and the correct expansion was **already in the file**, registered for the same triple and then shadowed by the truncating body (`Integer.toString(II)`'s shape, inside ONE registrar function), so the winner is now the delegation. Four scalar `insert` overloads (`IZ`/`IJ`/`IF`/`ID`, not three) gain natives on a shared splice. `%a`/`%A` join the zero-pad set with a sign-then-`0x` `lead` split. Winning registrar for all of it: `register_string_builder_natives` from `register_essential_natives_with_shims` (`lib.rs:18229-18231`), ambient `Bridge`; the `register_synthetic_overrides` copy is synthetic-only. Covered in `RStrings` (`CORE_CLASSES`) and four unit tests. **Ratchet note (arithmetic, do not paste):** the four `insert` overloads are the only new REGISTRATIONS, ×3 receiver class names = up to **+12** `bridge_shadows_bytecode` rows and 12 kind-map rows, both keyed `25/linux` and needing one real run; `BASELINE_SYNTHETIC_STUBS` does not move (ambient `Bridge`). |
| `W7-8-fabricated-success-io-sweep.md` | **Re-adjudicated 2026-08-12 (§7); four of the five old residuals are settled and a bigger one replaced the headline.** The real-instance guard EXISTS (it moved inside `native-api/src/synthetic_file_channel.rs`'s accessors) — but §3.1's whole fix set landed in `register_phase57_file_channel`, which only `register_synthetic_overrides` reaches, while `register_phase57_nio_file` carries unhardened copies of seven of the same triples and is what `vm_init` calls in **both shipping arms**. Rows 1/2/19 were LIVE. **§6 items 2–6 ALL APPLIED 2026-08-12 (§8), unbuilt.** The re-aim was done by SHARING, not moving or delegating: the seven triples both registrars serve are now one `p57_fc_*` function each, registered by BOTH, so last-write-wins is no longer load-bearing for them in any mode and the synthetic mode's behaviour is unchanged. A `--dump-native-registry` diff shows **no row change in any of the three configurations** — a duplicate updates a slot in place — which is the correct result and the thing a "the fix did nothing" reading trips over. The shipping-path hazard §3.6 named is not cashed: `fd_value` answers a non-`Int` for a foreign receiver, so the shared bodies have a third arm that keeps each site's pre-W7-8 answer verbatim for a real `FileChannelImpl`. Also applied: both ordering comments, the modified-UTF-8 codec plus `writeChars`' `encode_utf16`, `Files.isSameFile` onto `paths_name_the_same_file` with a `vfs_decode` screen at the call site, and the three-site option scan. **The `RNioNoFollow` Java that the option scan unlocks is written out in §8.5 and is NOT applied** (another lane's file); landing the refusals without it is safe, the reverse reddens the suite. No scheduled fixture covers the FileChannel re-aim — §8.6 says why. FIXED in `native-io`: `DatagramChannel.socket().setSoTimeout(-1)` (was `.max(0)` → *no* timeout), `watch.rs`'s negative-timeout-blocks-forever, and RAF `readUTF` (was bound to `readLine`). |
| `W7-12-strict-annotation-proxy.md` | R2, the resolution-1 redesign. **SIZED AND DESIGNED 2026-08-12, deliberately NOT started: it is not one lane.** `AnnotationProxy` has **134 references across 14 files**, and eleven of the fourteen — `vm_exec.rs` (43), `typecheck.rs`, `dispatch_virtual.rs`, `interpreter.rs`, `invoke.rs`, `jit/helpers.rs`, `class_manager.rs`, `native-api/src/registry.rs` — are outside any natives lane. The premise did NOT rot (three of its four parts re-verified in the tree). The record's new §R2 carries the four decision points, names the cheaper subset a first lane should scope to (make the HANDLER real, keep the carrier as the member-data record — that moves none of the 43 `vm_exec.rs` arms), and explains why **no fixture assertion is possible in either direction**: resolution 1 changes nothing observable from Java, so its instrument is a Spring Boot soak, not a vector. **R1 is FIXED, not "partly addressed"** — every `create_annotation_proxy_with_type` call site propagates with `?` and the swallow shape survives only in a comment. |
| `W7-16-arraydeque-and-linkedlist-residuals.md` | **CLOSED IN SOURCE by W7-62, not rebuilt.** The `jdk_interfaces` arm is applied and CLOSES the `ClassCastException` rather than moving it. Instrument: `probes/ListItrInterfaceProbe.java` plus a HotSpot 25 control transcript. **Re-verified from the tree 2026-08-12** — the arm is at `classloading/src/class_manager.rs:10979`, between the `ArrayListSubList` entry and `java/util/Dictionary` exactly as the patch named, and `jdk_interfaces` is read at **two** sites (`fabricate_class` and `create_synthetic_stub`), so it covers both doors; the retag (`no_image_receiver.rs:267`, tombstone `:198`) and the `ensure_vm_internal_class` mint are both present, though the record's mint line numbers have drifted ~950 lines — anchor on `native_ll_list_iterator`/`_idx`. **No suite run can close it**: `run.sh` names no path under `probes/` at any `SUITE=`, and the only two fixtures touching `listIterator` (`RJdkCollections:66`, `RJdkViews:209`) assign through the declared return type, so javac emits no `checkcast` and they cannot see this defect in principle. A scheduled vector needs an erased-type `instanceof`/cast. |
| `W7-17-vm-internal-door-sweep.md` | **The `CratonVM$…` arm is APPLIED 2026-08-12 (source only, unbuilt), and "optional" was wrong.** `fabricated_origin_for_name` gained `is_vm_reserved_namespace_name` (`classloading/src/class_manager.rs`), routing the prefix to `ClassOrigin::VmInternal`. The reason it is not belt-and-braces is a **second name the sweep's corpus never reached**: `CratonVM$StsForkRunner` (`native-builtins/src/jdk25_concurrency.rs:860`, the JEP 505 `StructuredTaskScope.fork()` worker body) is minted with a bare `try_alloc_concurrent_synthetic` and has no `ensure_vm_internal_class` of its own — §6A's defect in a file §6A never opened, and §4's own "the other choke point produced zero rows" scope limit predicted exactly this kind of miss. Gate 2 re-checked per name, not by analogy: neither `CratonVM$` name is in any table in `native-api/src/no_image_receiver.rs`, so `receiver_declared_by_no_supported_image` is `false` and nothing re-tags either `run()V` — §3's "reviewed VM service" row, where the door fix is necessary and sufficient. The prefix argument is written onto the new predicate with the one statement that differs from `AnnotationProxy`'s spelled out (`java/lang/annotation/` is closed by the JVM's package rules, `CratonVM$` only by convention), plus the standing instruction for anything added beside it. §8's R1 is closed by W7-26, whose own R1 is partially discharged the same day; §8's other three residuals are untouched. **No scheduled assertion** — the observable is `--dump-class-origins` / `--jdk-only-report`, and `StructuredTaskScope` is preview API the suite cannot compile. |
| `W7-19-methodhandles-compatible-residuals.md` | **Two of the three CLOSED IN SOURCE 2026-08-12, neither built nor run.** `isVarargsCollector()` now answers the marking — `MH_VARARGS = MH_BASE + 5`, both `lang_invoke.rs` allocators widened to `MH_VARARGS + 1`, and **the read is width-guarded on `object_num_fields`**, which the record's own prescription omitted: `MethodHandles.empty`/`zero` allocate 17-slot handles and `panama.rs` a compact layout, so a bare read of slot 21 is the out-of-bounds species `MH_KIND_ARRAY_GET`'s doc already records. `type()` is now narrowed after a getter / setter / array-getter / array-setter bind, which **tightens** the `bindTo` guard onto the only two shapes in the record's 13-shape census where CratonVM under-refused — and replaces a silent `MH_BOUND` overwrite with HotSpot's `IllegalArgumentException`. **Still open, and now blocked on a capability rather than on time:** `bindTo` raises no `ClassCastException` for a wrong reference type. The only assignability predicate on `NativeContext` is `is_subclass`, which answers FALSE for a fabricated stand-in against a real JDK interface, so a same-lane fix would raise FALSE `ClassCastException`s on the Groovy-indy / SpEL / log4j bind paths — a refusal of working code, worse than the wrong answer. It wants an `aastore_element_assignable`-shaped predicate (contract: never a false refusal) or a lane that can run those slices. Vector: `RJdkHandles` **37 steps / 116 checks → 40 / 128**; five of the twelve new checks fail on the old behaviour, the rest are controls. Carries ONE out-of-file line (the `check_override` mirror in `vm_exec.rs`), completeness not correctness. **Note the count: the record's own "316 checks" and §2.6's "54 checks" are both wrong for this file — the arithmetic is in the record's §5 preamble.** |
| `W7-23-thread-container-registration.md` | **FLIPPED 2026-08-12, UNRUN — `VM_REMOVES_THREADS_FROM_CONTAINERS` is now `true`** (`native-builtins/src/shared_secrets_bridge.rs`, anchor on the constant name, not a line). The blocker was gone (W7-27 landed the decrement), so this is now measure-only. **Read §10 of the record before the suite run: the wrong-flip signature is a HANG with no `FAIL` and no `PASS` line** — a vector that reaches `ThreadFlock` stops mid-transcript with the owner parked in `awaitAll()`. First diagnostic is one re-run with `CRATONVM_THREAD_CONTAINERS=0`, which un-flips it in the same binary; second is `-ea`, where `ThreadFlock.onExit`'s `assert removed` firing is the *opposite* regression. A green Jetty/Tomcat arm is **not** evidence either way (`SharedThreadContainer` never waits on a count), and the disappearance of the once-per-process `JavaLangAccess.start: thread started WITHOUT registering it` WARN is the cheapest positive signal the flip took effect. Second, silent signature: `ThreadContainers.root()` counts move the *wrong* way, because `platformThreads()` filters on `threadContainer() == null`. **Do not blame this flip for W7-27 §13.** `RJdkExecutors`'s `IllegalThreadStateException` failure arrived in the same wave but is not downstream of the flip: it reproduces with `CRATONVM_THREAD_CONTAINERS=0`, and its cause (`Thread.start()V` natively shadowed + `holder.threadStatus` never advanced past 0) predates both this record and W7-27's patch. **Coverage: none, and it cannot be written without a `run.sh` change** — a container vector must be reflection-only (naming `StructuredTaskScope` mints a 69.65535 class file) and needs `--add-opens java.base/jdk.internal.misc` + `java.base/jdk.internal.vm`, which no `class_args` entry supplies. |
| `W7-24-httpserverloop-and-strict-fallbacks.md` | `cratonvm/net/HttpBodyReplaySubscription` (§4, left loud) and the two `SSLSocket*Stream` sites with their twins in `phases_late/ssl_security.rs`. **Re-verified from source 2026-08-12 (§7.1): both residuals STILL OPEN, both landed fixes STILL PRESENT** — the door fix (`ensure_vm_internal_class(HS_LOOP_CLASS, 1)`) and the `ByteArrayOutputStream` fallback are in `net_phase_e.rs`, the `RE5_REPLAY_SUBSCRIPTION` mint is still a bare `try_alloc_concurrent_synthetic(..)?`, and `register_re1_socket`'s two mints are still behind the `io.real_net_sockets` early return. **Neither residual can take a fixture assertion**: §4's current correct reading is a refusal (a scheduled RED) and §5 is unreachable without `CRATONVM_SYNTHETIC_NET_SOCKETS`, which no `class_args` entry sets. §4's candidate table is a warning, not a menu — every `Flow.Subscription` stand-in either counts demand without delivering (a HANG replacing a loud refusal) or moves delivery off the caller's thread. |
| `W7-25-jul-getlogger-regression.md` | **Two of the three residuals are gone; one is a standing design decision.** (1) The `Supplier` convenience overloads were **already closed by §4's own fix** — all fourteen registrations are `jul_convenience_log`, which hands the supplier OBJECT to `log(Level, Supplier)` through `invoke_virtual` and never resolves it, so they gate wherever that overload gates. The record read them as open because it cited a line band (`lib.rs:17076-17106`) instead of following the call; that band is `jdk/internal/misc/VM` today and the registrations are at `lib.rs:17412-17460`. Already covered both polarities by `RJdkLogging.supplierOverloads()`. (2) `log(LogRecord)` **is now level-gated** (`logmanager.rs`, `native_jul_logger_log_record`, sharing `native_jul_logger_is_loggable` on the record's own level) — `RJdkLogging.recordPayloads()` gains both polarities at `Level.WARNING`, which the section could not see before because it ran at `Level.ALL`. (3) `LogManager.getLogger` demand-creation: **will not fix as a patch**, verdict re-affirmed — the JULI/Tomcat shims are built on it, and the triple is a retired shadow so `--jdk-only` already gets HotSpot's `null`; the divergence is Compatible-only. |
| `W7-26-getannotation-swallowed-exception.md` | **R3 DISCHARGED 2026-08-12 — the census was widened from one file to the workspace, and both of this row's old numbers were one-file counts.** The loader-ladder population is **21 `loadClass` delegations in 12 files**, not twelve in one: 4 propagate, 2 discriminate correctly, 1 is a documented best-effort preload, **14 swallow any failure**. Four sites are narrowed in source (unbuilt) behind one new policy helper, `absorb_class_absent` (`native-builtins/src/classloader_real.rs`, `absorb_thrown`'s shape with the two roots R1 names — `ClassNotFoundException` and `NoClassDefFoundError` — tested by `ClassId` hierarchy): the real-mode parent-delegation rung, `ucl_real_find_class`, `resolve_lookup_supertypes` ×2, and `upgrade_synthetic_class` ×2. The last is the sharpest and is **not** a lost-diagnostic defect — `.ok()` on the superclass fed `compute_field_layout` a `None`, so an upgrade whose supertype would not resolve installed `first_field_index = 0` and a silently shortened interface list; the sibling `define_class_with_options` propagates both, so this was the outlier. R2 also gained two members the one-file count missed, both worse than the original five because `ClassNotFoundException` is what a loader's caller expects (`ucl_real_find_class`, FIXED; `lang_class.rs:10022`, patch written). **Still open: thirteen ladders, every one in another lane's file** — `classloader.rs:3260` (the synthetic-mode twin of the fixed site) and `lang_class.rs:10022` carry exact replacement text; the other eleven carry dispositions. Gate: `RLoaderChurnDefine.aParentsFailureIsNotAMiss()` (CORE_CLASSES, so it runs on a default `run.sh`), a bare `java.net.URLClassLoader` over a parent overriding `loadClass(String,boolean)` — the only form both VMs reach — asserting the exception's TYPE and naming `ClassNotFoundException` as the old-behaviour failure, plus two over-correction controls. Known residual, stated at the helper: `MethodCallFailed::InternalError` is still absorbed, because `resolve_class_loader_aware` reports a plain classpath miss that way. |
| `W7-31-enable-preview-wiring.md` | **NOT RETIRED — its own falsifier, run on the wave binary 2026-08-12 (second pass), found a LIVE row; the "NOTHING left in this table" below is superseded.** Two of three falsifiers pass and discharge the headline: `--enable-preview` parses, the gate refuses without it and admits with it in HotSpot's exact words, and `jdk.internal.misc.PreviewFeatures.isEnabled` agrees with HotSpot on **both** arms. The third renders `<Unknown>` correctly and then throws the **wrong type** — `ClassLoader.defineClass(null, …)` on a 69.65535 class file gives `java.lang.ClassFormatError` wrapping a Rust `Debug` string where HotSpot gives `UnsupportedClassVersionError`, measured identically on `--jdk-only`, `--real-jdk` and the pristine-dev control, so pre-existing rather than this wave's. Site: `native_classloader_define_class1` (`native-builtins/src/lang_system.rs:5011`) ends every failure with `define_class_format_error(&name, "defineClass1", msg)`, which flattens the typed `LinkageError` §4(C) added and leaks a `Debug` rendering into a Java message; `defineClass2` takes the same tail. That is part C unfinished on the road part C was written for, and a caller catching `UnsupportedClassVersionError` — every container probing whether it can load a bundle — does not catch it. Full measurement in the record's new §7; RETIREMENT-20260812B.md §3.1. **§6's WILL NOT FIX stands and was not re-opened:** the `<Unknown>` vs `""` nameless-define distinction is **WILL NOT FIX**, decided rather than deferred, with the reasoning as the record's new §6. The reachable half (JNI NULL, `defineClass(null, …)`) already renders `<Unknown>` exactly as HotSpot does; the only input the patch changes is `defineClass("")`, which is not a valid binary name, which `javac` cannot emit and which nothing in the corpus produces. The observable is one word inside an exception message that nothing parses, and the cost is an `Option<&str>` threaded through `define_class_with_options` — the funnel every define path converges on — plus two files in another crate. §6 also names what would reopen it. Row kept for one merge cycle so a taker working from a remembered §2.1 does not redo it. |
| `W7-33-differential-dead-sections.md` | The synthetic-mode `EmptyStackException` follow-up in `classloading/src/class_manager.rs` — **still out of file, but now written out as exact replacement text against the tree as it stands** (two `match` arms, `jdk_superclass` → `java/lang/RuntimeException` and `synthetic_stub_fields` → the two-slot throwable set; anchor on the quoted blocks, both cited line bands rot). Re-verified 2026-08-12 that the headline half IS in the tree (`RuntimeError::EmptyStackException` at `types/src/error.rs:1098`, mapped `:1633`, exhaustiveness `:1720`). Neither addition can move either shipping mode or any Compatible-mode ratchet, and **nothing in-tree can observe it** — no suite runs `--synthetic-jdk`. **The headline now has the witness it lacked:** `RExceptions` (`CORE_CLASSES`) asserts `Stack.pop()` *and* `peek()` on empty, each with a `catch (NoSuchElementException)` arm beside the `catch (EmptyStackException)` one so the wrong type is *named* rather than escaping uncaught. |
| `W7-35-jul-supplier-and-payload-residuals.md` | **The survivor is CLOSED IN SOURCE, and §5's own patch is NOT the fix — do not apply it.** The `--jdk-only` half of #59 is W7-56-infercaller-strict.md, whose cause is neither of the two this record named: retiring the four source accessors was necessary and **not sufficient**, because a shadow **constructor** dropped `needToInferCaller`, so the now-real lazy getter read `false` and never called `inferCaller()`. All three parts are in the tree, re-verified against the source 2026-08-12 (W7-56's new "Landed state" section carries the registrar census: the four accessors have exactly ONE registrar, and `LogRecord.<init>` has THREE of which two ship and both are refused under strict). §5's `with_category(Bridge)` patch is superseded by W7-43 for #54 and by the retirement table for #59 — a category makes a row *eligible* to be retired, it does not retire it. **One NEW defect found in this record's own files while closing it (§2.1):** §2's Compatible-mode inference had a SECOND inference point running unconditionally after it, taking a second `capture_stack_trace` per record and overwriting the first answer with the weaker of the two predicates — adjudicated against JDK 25's `CallerFinder`, whose two stages (`isLoggerImplFrame` latch, then `SurrogateLogger.isFilteredFrame`) show §2 used the LATCH names as a SKIP set. Now a guarded fallback rather than an override. The predecessor record is no longer in this directory — retired/jdk-only-jul-logrecord-infercaller-SUPERSEDED-20260812.md. |
| `W7-36-differential-view-families.md` | **Two of its three rows discharged in source 2026-08-12 (Part 5), unrun.** The five TreeMap/TreeSet null-and-bound refusals are placed — `containsKey`, `remove`, `TreeSet.contains`, `getOrDefault`, the single-bound `headMap`/`tailMap` type check and `TreeSet.subSet(hi, lo)` — each at the point the JDK runs `compare(key, key)`, with three placements that are not interchangeable (before the view branch; before the empty-set early return; `container_is_empty = true` for the bound check, because `m.compare(hi, hi)` never consults `root`). **`native_tm_get_or_default`'s suspected defect DOES NOT EXIST** — both storage paths read the mapping, so present-with-null already answers `null`; the real defect at that native was the missing refusal, and the correction is locked by an assertion rather than left to the next reader. Still open: the synthetic-mode `EmptyStackException` follow-up (LANE 3's file), and `TreeSet.headSet`/`tailSet`'s single-bound type check, which is the same species and does not go through `tm_new_range_view`. Gate: `RJdkViews.sortedContainerRefusals`. (`stream.reuseThrows` is **closed** by W7-65.) |
| `W7-39-jca-missing-algorithms.md` | Advertised-vs-implemented is reconciled but not identical; the second SPI driver is recorded design debt; key-length checks moved to `init` rather than `doFinal`. |
| `W7-21-keygen-and-the-synthetic-secretkeyspec-twin.md` | Beyond §2.1, re-checked 2026-08-12. **The "wider algorithm set" row is CLOSED and closed from the other side:** W7-39 widened the real-mode seed from three names to twelve (`native-builtins/src/jca/provider_chain.rs:1418-1440`), which is the direction this record argued for — the seed was the narrower and more suspicious of the two. Still open: `init(AlgorithmParameterSpec)` accepts and ignores (defensible — the interface is an empty marker); and `java/security/Key.getAlgorithm` still answers `"AES"` for a carrier whose slot 1 is not a String (`native-builtins/src/phases_early.rs::carrier_algorithm`), synthetic-jdk only, blocked on a slot count on `NativeContext` as the record's Result 2 states. |
| `W7-46-process-cluster.md` | Both recorded-not-fixed rows were taken on 2026-08-12 (**§8**, still source-only — no build, no run). (1) The **Linux** `isAlive0` double-read is **FIXED IN SOURCE** and is the sharp residual: `linux_liveness_and_start_time` is one `/proc/<pid>/stat` read sharing its parser with `linux_proc_stat_times`, written **from a Windows host onto an arm nothing here compiles** — it needs the Linux build in §2.6 before anyone calls it done. No `RJdkProcess` check was added and that is deliberate: the merge removes a pid-recycle window and a two-read race, so **every assertion available would have passed on the old behaviour too**; the scheduled witness is the Linux-only `one_stat_read_reports_the_same_start_time_as_the_separate_probe` in `native-io/src/process.rs`. (2) The four `java/lang/ProcessBuilder` triples are **ADJUDICATED, fix out of lane** — §8.2 carries the exact deletion for `native-builtins/src/lib.rs:37971-37986`, and corrects this row's old framing twice: the untagged block inherits **`Intrinsic`** (a *chosen* kind, so it rewrites the slot's kind, not just its callback — and `Intrinsic` is the one kind `JdkOnly` does not drop), and it wins **three** of the four, `start()` being re-won afterwards by `native-io`'s `register_process_natives`. Also in §8: W6-10's five widened signatures audited on every arm (all consistent), and W6-10 finding 1 shown **stale** in strict mode — the §3 delegation moved `Process.descendants()` from 1 snapshot / 0 `OpenProcess` to 2 snapshots / `100 + N`. |
| `W7-51-vacuous-sweep-round-2.md` | **NEW ROW 2026-08-12, and it corrects the record's own §3.** Its "recorded and not fixed" list is **stale on three entries**: `RNioNoFollow`, `RCrypto` and `RJdkX509Intercept` were all repaired by `2969f44be` / `173f38606` and are specified with fresh mutation evidence in `W7-60-harness-extract-blindness.md` §3/§4 — verified present in the tree (`symlinkArms`/`plainFileArms`, the GCM known-answer and nine refusals, the two X509 negatives). **Do not re-fix them.** Its §2.5 reading of `RJdkServices:197` is also wrong in a way worth keeping: the loose `kind.equals("none")` disjunction is a real weakness *locally*, but `kind` is printed on the diffed line `CK RJdkServices badProviderCause=`, so the cross-VM diff discriminates it and tightening it against an unmeasured JDK behaviour would risk a wrong red. Same for `RJdkLambdas`'s `bridgeCount >= 1` — the count is on a `CK` line, and pinning it would pin a javac-version artefact. Genuinely still open from it: the 16 fixtures, F27's `gpu-offload` placement, `jdk-only-strict-probes.sh`'s absent-arm agreement, the two `churn=(n > 0)` booleans in `RForNameGcStress` / `ROverlaySystemGcStress`, and the two arithmetic tolerances at the end of its §2.6. |
| `W7-53-blocking-close-family.md` | Seven named rows where a thread blocked in a native read/write/accept still cannot observe another thread's `close()`. **The four TLS sites now have a written design and are still NOT applied — a second lane owning all four files read it on 2026-08-12 and declined, adding its reasons to the design rather than landing it**: trap 1 fails *silently* (a wakeup routed through `read_eof_tolerant` produces the pre-fix behaviour on a path that now looks close-aware) and nothing in the tree would catch it, since the instrument has no TLS row; trap 3's cheap fix duplicates a descriptor per blocking op on a request-rate path and the cost is a measurement; trap 4 means the row is currently *masked by a 30 s timeout*, not hanging, which bounds the cost of leaving it open. One obstacle it did remove: `t27_tls.rs` no longer has to add a binding, since `cratonvm_native_io::net::poll_stream_{readable,writable}` are `pub` and `net_phase_e.rs` has now demonstrated the cross-crate call. Original standing: — the key fact is that every registry entry already carries a `raw: Option<TcpStream>` duplicate reachable without the stream mutex, and `servlet.rs` already owns `s2_wait_ready_close_aware`, so no new primitive is needed; the four traps (`read_eof_tolerant` swallows `Interrupted`; `raw` is owned, not an `Arc`, and `close` removes the entry; `t27_tls.rs` has no poll binding; an existing 30 s socket timeout already masks the row) are all invisible to a compiler. The Windows pipe sink write is **narrowed, not fixed**: its blind window is now one 4 KiB slice instead of the whole payload. **Its instrument `probes/AsyncCloseProbe.java` has never run in any suite and cannot** — `run.sh` globs `src/*.java` and reads a word list; scheduling it means moving it to `regression-suite/src/RAsyncClose.java` and adding it to `CORE_CLASSES` (core, not jdk-only: this family is a Compatible-mode defect strict inherits). **THIRD PASS 2026-08-12 — declined a third time, and this time because the written design is UNSOUND, not merely unverified.** Trap 5, which none of the four implies: **socket readiness is not stream readiness.** TLS decrypts a whole record at a time, so a caller that asked for less than the assembler holds gets the rest with no socket I/O at all — `rustls-0.23.42`'s `Stream::prepare_read` touches the transport only `while conn.wants_read()`, and `wants_read()` is `false` while `received_plaintext` is non-empty — so the design's "poll `raw`, then take the mutex and read" **parks a read that had its answer in hand**, on the most ordinary HTTP-over-TLS shape, and `servlet.rs`'s 32 KiB readahead does not screen it. Three things the pass DID settle, so the next lane does not re-derive them: the screen exists on both stacks (`conn.wants_read()`; `native_tls::TlsStream::buffered_read_size()` in `native-tls-0.2.18`) and must be asked **under the stream mutex before it is released for the poll**; **trap 3 is retired** — the nineteen fixed sites `try_clone` nothing, they hold an `Arc`, so the three `raw: Option<TcpStream>` fields should become `Option<Arc<TcpStream>>` and the per-read descriptor duplication disappears; and a residual survives even the correct fix, because a 16 KiB record spans ~11 segments and `complete_io` parks between them, so the wakeup covers a reader at rest and not one mid-record. **Pilot named**: `rustls_stream_read`'s CLIENT arm only — no `cfg` arms, one exact screen — and `rustls_stream_write`/`s2_tls_write` must get NO loop, since W7-61 measured HotSpot not waking a parked TLS write. **Two of the thirteen shapes are now SCHEDULED**, which is new for this family: `regression-suite/src/RJdkNet.java::asyncCloseWriteAndAccept()`, 8 checks (72 → **80**), covering the write twin landed in this wave (`net_write_close_aware`, fails on the pre-fix behaviour) and the accept twin as a ratchet, on the default real-JDK path (`sun/nio/ch/SocketDispatcher.write0` and `sun/nio/ch/Net.accept`, both `Bridge` in `net::register_sun_nio_ch_net` ← `nio_native::register_t16_channel_overrides` ← `register_io_natives`, which `vm_init` calls on all three boot arms — so the SHIPPING registrar, not a synthetic-only one). Both prove the park before the close, bound every wait by the file's `T` so an unfixed native FAILs instead of timing out the suite, and the write row closes the peer in a `finally` **before** asserting so a red row cannot leave a thread in the kernel. No TLS row was added and must not be — the Windows half is open, so it would be a scheduled RED. |
| `W7-60-harness-extract-blindness.md` | **The instrument, not a defect in the VM.** The blind population is now measured at **zero** across all 70 scheduled vectors, held by four mutation-checked guards. **Its recorded-not-fixed row is CLOSED 2026-08-12:** `RPriorityQueueGc` and `RTreeRangeGc` publish check counts and their rows are **deleted** from `regression-suite/harness-uncounted.txt` (the ratchet's reverse direction requires the deletion, not an annotation). The deferred decision, recorded so it is not re-litigated: the unit is one `check()` call, and no run under `--nojit --Xmx 64m` was needed because the number is a property of the source, not of the heap — those flags decide whether the assertions FAIL, not how many run. `RPriorityQueueGc`'s concurrent drain loop is deliberately routed through an **uncounted** `checkDyn()` twin, because how many elements are left to drain is scheduling-dependent and a diffed count over it would red a CORRECT VM. `harness-uncounted.txt` now carries exactly one row, `RClassUnloadSweep`. **`RMapResizeGc` / `RMapGcStress` were reported as also-uncounted and are not** — both have published `PASS <Class> (N checks)` since they were written and adding them would fire the reverse ratchet. **Read its §6 before the next suite run — the pass count is expected to move, and downwards is the good direction.** |
| `W7-61-sslengine-layout-and-tls-blocking.md` | (1) `javax/net/ssl/SSLEngine` carries a 7-slot map and a 14-slot map on ONE object under `synthetic-jdk`; the 8 triples p68 does not re-register index slots 7–13 past the end of a 7-wide allocation. The Compatible-mode half of W7-49's "LIVE, 7 vs 2" row is a **false positive** and the record shows why. (2) The **Windows** half of the TLS read wakeup: Winsock has no `shutdown` that aborts a pending blocking call. **Both halves of item 2 re-verified present in the tree 2026-08-12** (`s2_tls_close`/`rustls_stream_close` shut down `entry.raw`; both classifiers run on every return), and the Windows half is now **specified** rather than merely named — the wakeup is the nineteen-site park-in-`poll` shape, which makes the missing `shutdown` irrelevant, but only behind an assembler-side screen: see W7-53's "Third pass" for trap 5, the `Arc` change that retires trap 3, and the `rustls_stream_read` client-arm pilot. Item 1 is also verified landed (the ordering table, the corrected stale sentence, `pub(crate) fn register_re6_ssl_context`, and `mod registry_ordering_tests` in `tls.rs`). |
| `W7-65-stream-reuse-throws.md` | **TWO OF SIX CLOSED 2026-08-12, unrun.** `close()` (§5.3) now `stream_mark_linked`s **before** running the handlers, matching `AbstractPipeline.close()`'s own order — the blocker was a census, and the grep comes back clean: the only two VM-internal stream closes are the `flatMap` inner-stream closes and both run *after* that stream is read, exactly as the JDK's own try-with-resources does. `onClose()` (§5.4) now raises `IllegalStateException`, placed **after** the `is_synthetic_stream` gate because slot 4 on a real pipeline is one of its own fields. Gate: `RJdkCollections.streamReuse()`, 61 → **69** checks, two reds and six controls, in `JDKONLY_CLASSES` — not in `probes/`, which `run.sh` never runs. Still open, both re-costed: **primitive streams** (§5.1.1 — the swallow currently eats NOTHING, all three `stream_elements` error paths are unreachable from a primitive receiver, and the conversion's real cost is checking live pins at each of the 25 sites, not appending `?`; a bare `?` in `native_int_stream_peek` leaks a pin, which compiles); the deferred intermediate op in Compatible mode only; the `GathererOp` constructor (nothing to attach a residual to); and **short-layout streams** (§5.6.1 — it is two `.max()` calls in `native-builtins/src/service_loader.rs`, out of this lane's files, and it is the `getResultStream`/`ServiceLoader`/`Files.walk` path, i.e. exactly the "most pervasive path" the record declined twice for; the cost is the arm run, not the two lines). |
| `W7-77-guarded-slot-maps.md` | The four guarded rows are dispositioned and gated, and **none is renumbered** — on the fabricated class each map IS the layout, so a renumber would break the only receiver that exists. **Its "`verify_declared_slot_maps` still has no caller" residual is CLOSED and the tree was re-verified 2026-08-12** — W7-90 wired both triggers and links 7-9 of `native-api/tests/read_alias_coverage.rs` hold them; HANDOFF-20260812.md still repeats the old claim and is stale on it. The published-map count is **eight, not seven** (W7-88's `SSC_P58_SLOT_MAP`), so this row's four maps contribute 15 of **33** predicted census rows, not 29; the real-JDK-mode figure is unchanged at 24 because every map added since has been synthetic-only. Still open: the dead `util_time.rs` `java/time/Month` twin is documented, not deleted — **re-examined 2026-08-12 and deliberately kept**, because it is dead by call ORDER with no gate on that order and its `#[cfg(test)]` block is the crate's only cover for the month-length arithmetic; the `StringJoiner` row cannot leave the census while the fabricated stub shares the real class's binary name (structural — `SlotMap.class` is a `&'static str`; **not** a defect and must not be "fixed" by editing the declaration). Corrects W7-69 on three counts — `Month`'s Int-in-a-reference is **not** collector-visible on either layout, `Thread` is four disagreeing slots not one, and `java/time/Month` has two slot maps of which one is dead. |
| `W7-91-format-date-symbols-hardcoded-english.md` | **HEADLINE DISCHARGED BY MEASUREMENT 2026-08-12 (second pass); what is live is §5, and only §5 — this record was NOT retired for it.** Re-run with the pristine-dev control beside it: `streamBytes` 175 → **177**, `handlerLevelGate` 87 → **88**, both equal to HotSpot in the same session, so the month name is closed. **Its §8 falsifier is superseded — do not work from it:** it predicts a residual one-character gap against 179, and both VMs read 177, because W7-92 landed the hour half *and* the unpadded 12-hour field is one digit for everyone at that hour. **The live row is §5, "the numeric half, deliberately not moved":** `String.format("%,.2f", x)` with no `Locale` localizes against ROOT where a real `Formatter` uses `Locale.getDefault(FORMAT)` — a deferral for want of a measurement, which RETIREMENT-20260812.md §3 rules a live row, and unlike W7-67's residual it is not written at its source site. Successor if anyone closes it rather than carries it: `W7-34`. The rest of this row is the filing record. **Filed 2026-08-12 off a MEASURED red — `RJdkLogging` fails the cross-VM diff on pristine dev — and only half of it was fixed then.** The deficit is a uniform 2 chars per LOG RECORD (`streamBytes` 175 vs **179**, `handlerLevelGate` 87 vs 89), and solving those four numbers against `SimpleFormatter`'s default pattern gives two independent one-char defects, not one. `%n` is **refuted** by the same arithmetic, W7-71 §4.2 having already checked the source. **Fixed here:** `%tB` `%tb`/`%th` `%tA` `%ta` `%tp` `%tr` `%tc` rendered from four hard-coded English arrays in every locale and now go through `java.text.DateFormatSymbols`, with the `DecimalFormatSymbols` re-entrancy latch W7-34 built copied for it; covered by `RStrings` 39 → **46** checks. **NOT fixed, and it is the other half:** `ZoneId.systemDefault()` answers `UTC` on every Windows host, so `SimpleFormatter` dates run three hours behind HotSpot's here. Live producer is `native_timezone_get_system_id` in `native-builtins/src/lib.rs` (`let tz_id = "UTC";`); `util_time.rs`'s Windows arm is only a fallback and repairing it alone is **inert** (marker comment left in place). Expect `RJdkLogging` to move 175 → **177** and stay red — that residual gap is the falsifier, not a failure. **Supersedes W7-80 §3's "the gap is exactly two bytes" and its §7 green prediction.** |
| `W7-93-stackwalker-option-constants-null.md` | **Filed 2026-08-12 off a MEASURED red on pristine `dev` (44044c7e2), FIXED IN SOURCE, not built and not run — and it is a live residual inside a doc marked FIXED.** The headline handed to this lane, *"every `StackWalker$Option` constant reads back null"*, is true of exactly **one** of four: `DROP_METHOD_INFO` (JDK 22+) is null, and `RETAIN_CLASS_REFERENCE`/`SHOW_HIDDEN_FRAMES`/`SHOW_REFLECT_FRAMES` are **non-null but nameless** — `name()` null and `ordinal()` 0 on all three — which is the harder half, because every null-check passes while `Enum.valueOf` finds nothing, `compareTo` calls every pair equal and an `EnumSet` over them collapses to one bit. **Answered first and it is the cheap, decisive question: this is StackWalker-ONLY.** `DayOfWeek`, `StandardOpenOption`, `RetentionPolicy`, `TimeUnit` and `Thread$State` all initialise correctly through their own real `<clinit>` on the same binary in the same run, so `Enum.<init>`, `putstatic`, `$values()`, `getEnumConstantsShared` and `EnumSet` are all sound. Mechanism, established not assumed: the **real** class bytes ARE loaded (five declared fields including `DROP_METHOD_INFO` and `$VALUES`, plus the real `values`/`valueOf`/`$values` bodies — the fabricated shape has three fields and no `$VALUES`), and `javap -p -c` shows an entirely ordinary `<clinit>`; it never runs because `stack_walker.rs::register_stack_walker_boot` registers a native for the `<clinit>` triple and **registration is the gate** (natives-over-real-jdk-classes.md §1), unconditionally in BOTH modes — the real-JDK arm calls `register_essential_natives_with_shims` directly and the synthetic arm reaches it via `register_builtins`. The native allocated bare instances from a hard-coded THREE-name list and never called `Enum.<init>(String,int)`; both symptoms fall out of those two lines. The reporting lane's read of `phases_late.rs`'s three static-field natives is **confirmed dead, by identity measurement not inference** — a `getstatic` never consults the method registry, and `values()[0] == RETAIN_CLASS_REFERENCE` proves the objects came from the `<clinit>` native; they would break `==` against `$VALUES` if ever made live, and now carry a tombstone. Fix: derive the constants from the class's **own declared static fields** (declaration order = ordinal order; `$VALUES` filters itself out by descriptor) and write `name`/`ordinal` — so JDK 21 gives 3, JDK 25 gives 4 and the next JDK costs nothing; **a fourth hard-coded name would have re-armed the same trap**. Deleting the registration so the real `<clinit>` runs is the right end state and is left OPEN, because the registrar is shared with `--synthetic-jdk` where the class is fabricated with no `<clinit>` bytecode and `class_manager.rs` injects an `ACC_NATIVE` one — removal alone converts that mode to `UnsatisfiedLinkError`, so it needs the registration made conditional on the RUNTIME mode, built and run. §3.2 is the reason this sat: the internal ES fixed-bug doc closed the area in 2026-07 recording *"`values()` returns length 3"* as verified-good, and the unit test asserted `array_length == 3` — identity holds fine between three wrong constants and a three-long array of the same three. Coverage: `RJdkStrict` gains `realEnumsAreSelfConsistent()` over `Option` **and the five sibling enums**, asserting only self-consistency against the loaded class (no count, no constant named, so it cannot rot); verified **PASS on HotSpot at both `--release 25` and `17` (347 checks)** and **FAILS on the control** at *"values() has 3 entries but the class declares 4"*. Wild victim, reproduced: `Cipher.getMaxAllowedKeyLength("AES")` → `ExceptionInInitializerError` via `JceSecurityManager.<clinit>`'s `Set.of(DROP_METHOD_INFO, …)`. **§8, filed 2026-08-12: this is now the enum-identity record and holds a SECOND, DIFFERENT cause — `Thread$State`, fixed in source, not built.** Read it as a correction to the row above: *"this is StackWalker-ONLY"* was measured with a probe that asked only non-null-and-correctly-named, and `Thread$State` passes that. It fails identity. Its constants and its `<clinit>` are **fine** and `getEnumConstants()[0] == State.NEW` — the opposite half is broken: `concurrent.rs::register_p71_thread_extras` registers minting natives on `values()` and `valueOf()`, so `values()[0] != Thread.State.NEW` and `values()` hands back different objects on two consecutive calls. It reaches `--jdk-only` because `register_essential_natives` (`lib.rs:9159`) calls that registrar for its **`ThreadGroup`** half during JBoss-Modules bootstrap; the `Thread$State` half is collateral. Two counter-intuitive measurements the fix rests on, both taken not assumed: `Thread.getState()` is **NOT** the culprit — `native_thread_get_state` already returns the canonical static field, refuting the leading hypothesis — and an enum **`switch` still selects the right arm** over a minted constant, because `$SwitchMap` is ordinal-keyed, so only `==` shapes detect this (`EnumSet`/`EnumMap` survived too). Scope closed by identity-measuring **twenty** real JDK enums on the red binary: `Thread$State` is the only BAD one, `Option` included among the OK, and every other `p57_alloc_enum` minting site sits in a phase registrar `--jdk-only` never runs. Fix: `lang_system.rs` gains `canonical_enum_constant` / `canonical_enum_values`, which derive the constant names from the class's **own declared static fields** and return `None` — falling back to the existing minting body — unless every one reads back non-null, so a fabricated synthetic-JDK stand-in is unaffected and no null-holed array is ever built; the two `concurrent.rs` natives try the canonical route first. Deleting the registrations is the right end state and is left OPEN for the same reason as §7.1. Coverage: `RJdkStrict` keeps the unweakened failing assertion and gains the stable-elements/`==`-identity shapes, **PASS on HotSpot at 359 checks** (was 347), with the switch check annotated in-source as a non-detector. |

### 2.3 Filed 2026-08-12, source landed, NOT yet verified against a binary

These carry no known-stale rows; what they need is a build and the verification
command each one names in its own final section. **Do not re-derive them.**

`W7-42-differential-instrument-holes.md` (the live differential figure: **9**.
**Both its holes re-verified PRESENT in the tree 2026-08-12** — the probe carries
all nine ledger markers and `native-builtins/src/lib.rs:38238` gates the
`[SUREFIRE-NPE]` forensic — and the **runner** was then audited and had six more
ways to lose or misread a row, all closed in `probes/shadow-differential.ps1`: a
digest mismatch printed the diff anyway (now withheld entirely — a warning above
a plausible diff gets scrolled past, which is how hole 1 happened); `PROBE-LEDGER`
was matched against a frozen five-field string that would red a **healthy** run
the day the ledger grows again (now parsed, every field must be 0); the stderr
sidecar was kept and never read, though this record's one new finding came from
there and is invisible to every stdout diff (now scanned for nine linkage/lookup
errors, printed above the diff, not gating the exit code); no cross-side check on
`PROBE-OBSERVABLES-EMITTED`, which both sides can disagree on while each ledger
reads `missing:0`; no provenance, though "the scratchpad copy outlived the run"
is hole 1's actual mechanism (now a `provenance.txt` with probe SHA-256, `javac
-version` and each exe's hash and mtime); and `Get-Content` decoded both
transcripts in the host ANSI codepage while the sides are pinned to UTF-8. Its
re-measure block is written with the **numbers blank** and says not to copy the
9, the 864 or `22732607802c59c2` into it. Note the brief's "858-line oracle" is a
conflation: 858 is this probe's declared-observable count; RETIREMENT-20260812.md
records W7-4's retired oracle as **540** lines) ·
`W7-49-slot-index-recensus.md` (**two of its rows corrected in place
2026-08-12, both overstating open work.** (1) Its §7 `javax/net/ssl/SSLEngine`
7-vs-2 "LIVE" row is a **false positive on both shipping boot paths** —
`ssleng_alloc`'s two callers are overwritten 23 lines later by
`net_phase_e::register_re6_ssl_context`, and `tls.rs:4948-5021` is a source gate
holding that order; the map is live only under `--synthetic-jdk`. Per W7-61 — do
not re-derive it. Its two other `SSLEngine` mentions should read `synthetic-jdk
only`; the "20 LIVE `over` call sites" figure drops by one site but is
deliberately **not** restated here — that list is enumerated by class while the
20 counts sites, and re-deriving one from the other by hand is how W4-4's
`ConcurrentHashMap` 16-vs-10 row got written wrong.
(2) Its §2(c)/§9.3 "511 direct `alloc_object` sites the instrument cannot see" is
**CLOSED**: the detector moved to `native-api/src/layout_alias.rs` with a second
observation point on `NativeContextImpl::alloc_object`, the funnel's call kept
because it clamps before allocating and is the only place `under` is still
visible. Note what that did *not* close — the clamp runs two lines after the
observation, so `direction=under` describes a mis-request and never a short
object; short objects arrive with `real_fields == 0` and report as
`undeclared`) ·
`W7-50-synthetic-jdk-strict-six.md` (the 63/7 baseline; supersedes the tracked
48/6) · `W7-51-vacuous-sweep-round-2.md` ·
`W7-54-strictmath-fdlibm-family.md` · `W7-56-infercaller-strict.md` ·
`W7-57-close-flush-swallow-sweep.md` (residual audit 2026-08-12 by the
`classloading` lane: **zero** close/flush residuals in `classloading/**` or the
three `native-builtins` classloader files — every `"close"`/`"flush"` hit there
is a `mk(…)` entry in a fabricated method table, not a delegation. Its rows
**48–51 are confirmed STILL OPEN** on this tree, which is the condition the
record itself set: W7-52 has not merged, all four `java.util.Formatter` sites
are still bare `let _ =` at `native-builtins/src/lib.rs:21440`/`:21455`/
`:41498`/`:41506` with no `Closeable`/`Flushable` guard, and registrar 1 still
discriminates on `ctx.read_string(target).is_none()` — a value-shape test.
Its five named false positives are re-verified `?`-terminated, so its
51 − 4 + 5 = 52 reconciliation still holds) · `W7-58-bytebuffer-direct-arm.md` (its own
78 sites are synthetic-mode-only and remain unrunnable by the suite; §12 splits
its probe's rows into the three that matter and schedules the reachable half in
`RDirectBufferElem` groups 6-7) ·
`W7-62-ratchets-and-dead-code.md` ·
`W7-63-jca-advertise-vs-serve.md` (second pass 2026-08-12: six of its seven
dispositions verified present; the seventh — #2's **alias** half — was green on
a *proxy*, because the ratchet asked `get_service_entry` while
`MessageDigest.getInstance` gates on the digest engine's own name table and
reads no registry, so `getInstance("SHAKE128")` still threw in both shipping
modes. Fixed, and the record's assertions now live in the scheduled
`RJdkSecurity`, which moves **61 → 80 checks** in all three arms) ·
`W7-64-printstream-trouble-and-errormanager.md` (**OVERSTATED open work;
corrected 2026-08-12.** Two of its five "what is left" items are CLOSED IN
SOURCE and re-verified from the tree, not from the closing records:
`native_printstream_close` is no longer a no-op — W7-70 gave it the receiver
test its comment was a reason for (`logging_shims.rs:1245`, a `closing` latch
plus the sink resolved by the JDK's own `out` name) — and
`route_write_through_out` no longer reports an absorbed failure as "not
routed": W7-81's `DelegatedWrite` (`print_error_state.rs:221`) splits
Delivered/Absorbed/Refused and **both** branches now end in
`classify_write_failure(..).routed()`. Genuinely left: the `Error` that is
still absorbed rather than propagated — now printed to the console rather than
lost, which is a changed shape; `ErrorManager`'s six static ints under
`--synthetic-jdk`; the `PrintWriter` closed-marker, whose `PrintStream` twin
W7-70 built as a named `closing` field and deliberately did not give
`PrintWriter`; the unasserted `InterruptedIOException`; and `addSuppressed`.
**And the record never said which build contains its rows.** Three of five —
`checkError`/`setError`/`clearError` (`lib.rs:22979`), the whole
`StreamHandler`/`ErrorManager` surface (`register_p61_handler_error_manager`
← `register_phase61_natives` ← `lib.rs:24027`), and the `PrintWriter`
flush/close recording — are inside `register_synthetic_overrides`, which is
`#[cfg(feature = "synthetic-jdk")]` (`lib.rs:21525`). That feature is not
default, so **the default binary never compiled them**; only the `trouble`
write funnel and the `Handler.<init>` fix ship in it. Its final instruction is
also wrong twice: `--synthetic-jdk` needs a `--features synthetic-jdk` build,
and `probes/` is never run by `run.sh`, so no suite run discharges this
record) ·
`W7-66-live-over-allocations.md` (**two rows corrected 2026-08-12, later pass.**
(1) Its §6 slot-5 `keys` clobber — a `java.net.ServerSocket` living in
`AbstractSelectableChannel.keys` — is **REPAIRED** by W7-72; §11 item 4 struck.
(2) Its §4.3 narrowing moved a site into the *other* census and no record noticed
on the day: `alloc_obj`'s four callers went 12 → `SC_OBJECT_SLOTS` = 6, and
`SocketChannel`/`ServerSocketChannel` declare **10**, so
`native-io/src/socket_channel.rs:637` — filed by W7-73 §3.2 as "12 against 10 —
over" — is now **short by 4**. Not a defect (an `Err(_)` arm, latent by
construction) but a reclassification, and the general lesson is the row's value:
**a repair that narrows a request toward the declared width can push its own
fallback arm below it. The `over` and `under` censuses are one measurement read
from two sides.** §1's "`over` is not on its own a defect predicate" and §2's
superclass-fields-take-the-low-slots rule both re-read and hold) ·
`W7-68-live-under-allocations.md` (**exactly one row is DEAD — §3.2,
`FileChannel` — and it is dead twice over.** The repair landed (W7-72, one step
across both crates, new owner `native-api/src/synthetic_file_channel.rs`), *and*
the premise on which §3.2 refused it is **inverted**: the `isOpen()` copy that
reads slot 0 as the fd is `register_phase57_file_channel`, whose entire
transitive caller chain to `vm_init.rs` is `#[cfg(feature = "synthetic-jdk")]`
and which is overwritten by the constant-returning copy even inside that build —
inert in all four configurations. **A "which registration wins" claim is not a
statement about the file the registrations are in**; the ordering is decided in
`vm_init.rs`'s two `#[cfg]` arms and one `if`, and must be traced to the top in
each of the three configurations. §4's tally row and §6 item 3 struck to match.
Its §6 item 2 — the `declared == 0` short-object census it predicted and could
not take — is now **closed as an instrument gap** and measured: 28 sites, **12**
short. Every other verdict re-read and holds, including §1's structural finding
that three later records depend on) · `W7-69-read-side-alias-instrument.md`
(**read W7-77 BEFORE it, never after.** W7-77 §3 re-derived all four guarded
rows from `javap` and corrects this record on three counts: `java/time/Month`'s
Int-in-a-reference is **NOT** collector-visible on either layout — the compact
arm stores zero for a non-reference `Value` and the legacy arm never visits an
`Int` cell — so it is a wrong ANSWER (a nulled `Enum.name`, and January for
every month) and not heap corruption; `java/lang/Thread` is **four**
disagreeing slots, not the one its §4.3 lists, because the adjacent
`THREAD_FIELD_*` run is a separate `const` run the comment-scraper never
reached; and `java/time/Month` has **two** slot maps of which `util_time.rs`'s
is dead, which also falsifies its §4.3 "Nothing is dead in this population".
Treat every `native-builtins/src/lib.rs` line number in it as stale — W7-77 §3.2
measured the drift as a per-file constant. **And its census has never run**:
both entry points gate on `layout_alias::enabled()`, i.e. on
`CRATONVM_DBG_LAYOUT_ALIAS`, and that name appears **nowhere** under
`regression-suite/` or `ci/`, so §4's tables are a source-level classification
and must not be quoted as a transcript) ·
`W7-70-printstream-close-noop.md` ·
`W7-71-jca-exception-types-and-line-separator.md` ·
`W7-73-short-object-blind-spot.md` · `W7-74-short-object-repairs.md`
(**the short-object count is 12 of 28, not 14 — re-derived 2026-08-12, later
pass, and the ratchet's doc, its failure message and both records are corrected.**
The 14 was an arithmetic slip with a lesson: **the population number and the
short number are different numbers and must be decremented together.** W7-74 §1
took W7-73's 16 to 14 *out of a population of 30*; the two `java/lang/Thread`
mirrors it then repaired were members of that 14, so 30 → 28 had to carry 14 →
12. Two later movements land the same day and cancel, which is how a stale figure
gets "confirmed": `native-io/src/lib.rs`'s `native_fc_open` LEAVES the short
column — W7-72 put its width on `synthetic_file_channel::alloc_slots`
(= `base_for_class(…) + 2`), making the row structurally unable to be short by
W7-74 §1.3's own argument, the same shape as `MappedByteBuffer` — and
`native-io/src/socket_channel.rs:637` JOINS it via W7-66 §4.3's narrowing.
**W7-73 §3.4 is the current table**; every line number in W7-73 §3.1/§3.2 and
W7-74 §1.4/§2 is stale — the `native-io/src/lib.rs` rows by +12..+538 against
W7-73 and by a further +4..+407 against W7-74's own correction of them. Both
appended-slot widths are now held by a gate rather than by prose —
`layout_alias_coverage.rs::the_appended_slot_allocators_do_not_regress_to_a_literal_width`
— because a literal restored there is invisible to the ratchet (the site count
does not move) and to the width census (the reported width is correct for the
object actually allocated). Unbuilt, unrun) ·
`W7-88-net-channels-dead-registration.md` (the losing `ServerSocketChannel
.socket()` never registered at all — `--dump-native-registry` shows
`overwrote=null` in all four configurations; its seven writes land on
`AbstractInterruptibleChannel.interruptedTarget` and five real
`java.net.ServerSocket` fields, `closed := -1` among them. **Landed state
re-verified from source 2026-08-12, §9**: the deletion, `SSC_P58_SLOT_MAP`, the
unconditional `native-io` winner and the `#[cfg(feature = "synthetic-jdk")]`
gating on the loser's single call chain all hold — but **every line number in
§2.1/§4/§6 has drifted** under concurrent edits (`socket_channel.rs:4776` →
`4778`, `lib.rs:23922` → `24018`), so anchor on the identifiers. What still needs
the run is unchanged and is a **census diff, not a vector**: the four
`--dump-native-registry` configurations must be byte-identical, compared by
field rather than by text) ·
`W7-89-memorysession-checkvalidstate.md` (**the whole FFM lifetime model was
inert in Compatible mode** — use-after-close returned stale data, off-thread
access to a *confined* arena succeeded, double-close succeeded; the arity bug
was real but fixing it alone would have moved nothing, because
`p67_session_modelled` tested slot 0 for an `Int` on a real `MemorySessionImpl`
whose slot 0 is the declared-reference `resourceList`. **§11 re-read the repair's
own witness end to end and it is on the live path**, and §9's asked-for fixture
now exists — `RForeignLayoutJdkInterfaces.foreignArenaLifetime()`, 22 checks, no
`run.sh` change. The use-after-close READ and thread confinement stay
probe-only: `MemorySegment.get`/`set` are gated on `--enable-native-access`,
which this suite does not pass) ·
`W7-90-slot-map-sweep-caller.md` (two triggers, not one: `System.exit` never
reaches the launcher line, and that is how every Spring Boot and Surefire
fixture ends. **Both triggers re-verified present 2026-08-12** — `vm-cli`
`main.rs:4281`, `vm_init.rs:7965`, `lang_system.rs:137`/`1354`/`2628`/`2664`,
links 7-9 green. **The third door — the one §2.2 named as the whole reason the
trigger exists — is now WIRED (2026-08-12, later pass).** A Surefire fork leaves
through four `ForkedBooter` triples registered from
`register_essential_natives_with_shims` — LIVE in both modes — onto **three**
bodies (`exit()V` and `exit1()V` share one) that each `std::process::exit`
without ever reaching `System.exit`, so neither trigger fired and the sweep
printed nothing on exactly the corpus it was built for. Applied:
`lang_system.rs:137` is `pub(crate)`, and `test_frameworks.rs:3684`/`:3710`/
`:3735` each call the shared helper — not a fourth open-coded gate, because the
no-`else` property link 9 asserts lives in that helper — with its own trigger
label, below the `soft_exit` early return. Link 10
(`the_surefire_exit_paths_sweep_before_they_terminate`) is in
`native-api/tests/read_alias_coverage.rs`, was simulated **RED on all three
bodies** beforehand, and the module header's item 9 no longer reads as though
`System.exit` were the whole self-terminating population. Unbuilt, unrun.
Also: the published-map population is **8**, so its §4 totals are 42 slots /
33 predicted rows, corrected in §4.6.1 and §4.7) ·
`W7-75-continuation-forkjoinpool-alias.md` (its §4 per-collector table is
SUPERSEDED by W7-84's convergence — do not quote it) ·
`W7-80-locale-data-stage-two.md` (the JDK image's own CLDR data was reachable
all along, behind eleven comments in four files saying it was not; closed the
last strict red) ·
`W7-83-segment-as-backing-array.md` (a `MemorySegment` returned where `[B` is
declared, via `s2_bb_arr` — but **§8 corrects its own §4 on reachability**: the
receiver needs `MemorySegment.asByteBuffer()`, which has NO registration in
either crate and is on no forced list, so §4's `before` column is what *would*
have happened and its `seg.*` rows cannot be scheduled at all. §7.1's
`arrayOffset()` gap is the one part of the record a fixture can reach; §8.1
prescribes the `servlet.rs` body and schedules 26 assertions in
`RDirectBufferElem` group 7) ·
`W7-84-primitive-in-reference-store.md` (four heaps, not three; `cargo test -p
cratonvm-gc --test primitive_in_reference_slot` **RUN 2026-08-12, 10/10**) ·
`W7-86-static-native-arity.md` (`Runtime.exit` exited 0 for every status;
`ClassLoader.findBootstrapClass` answered null for every name) ·
`W7-87-urlclassloader-namespace-asymmetry.md` (`new URLClassLoader(urls, null)`
— *the* isolating idiom — had no isolation; 17 asymmetric consumers remain) ·
`W7-72-ssc-socket-and-filechannel.md` (inverts W7-68's registrar reading for
`FileChannel.isOpen`; the copy that reads a private slot is compiled only into
the synthetic build and overwritten even there. **Re-verified from the tree
2026-08-12 and every source claim holds** — the side table, its two GC hooks at
`native_roots.rs:417`/`:423`, `native-api/src/synthetic_file_channel.rs`, and
**both** `isOpen` registrations now naming the single body `p57_fc_is_open`
(`nio_file.rs:15037`), so the disagreement §2.4 called load-bearing is gone
from the source. **What is NOT true is that any suite run could show it.** No
fixture calls `ServerSocketChannel.socket()` — the six `.socket()` hits are all
`DatagramChannel.socket()` in `RJdkNet` — and `probes/` is never run by
`run.sh`, so item 1 has no vector at all. Item 2 has a vector on the CLASS but
not on the DEFECT PATH: `RChannelInterrupt:125`/`:147` do assert
`FileChannel.isOpen()` in both polarities, which is exactly §4.2's asymmetry,
but `FileChannel.open` yields a real `sun.nio.ch.FileChannelImpl` while the
private map lives only on a literal `java/nio/channels/FileChannel` minted by
the `newFileChannel` legacy fallback. §7.1 states what a real vector for each
item must do) ·
`W7-76-bytebuffer-alias-residuals.md` (`HeapByteBuffer`'s `6` is a LAYOUT
WITNESS, not a width — widening it darkens every indexed fallback in the one
mode with nothing to fall back to. **§10 is a fifth defect its own §4.3 walked
past**: §4.3 measured that HotSpot RESETS the byte order on `slice`/`slice(II)`/
`duplicate`/`asReadOnlyBuffer` and never asked what `servlet.rs` does — all four
s2 sites PROPAGATE it, live in Compatible mode, 36 assertions now in
`RDirectBufferElem` group 6. §4's own `bigEndian` seed is NOT schedulable: in
Compatible mode `alloc_byte_buffer` is reachable only through
`Channels.newReader`) ·
`W7-79-loadlibrary-compatible-arm.md` (the name is `args[2]`, measured on a
running VM, not `args[1]` as recorded) ·
`W7-81-write-route-three-way.md` (its 23 call sites were a red herring; the
defect was one helper mapping three inputs onto two outputs differently per
branch) ·
`W7-85-serviceloader-stream-validation.md` (closes `W6-2`'s first row. Its RED
and its HotSpot oracle were BOTH measured — on the dev binary and on HotSpot
25.0.3.9 — so what is unverified is only the fix; the vector, `RJdkModule`, is
in `JDKONLY_CLASSES` and runs 104/104 on the oracle. Read its population table
before touching `service_loader.rs`: it names which of the file's validations
are enforced on both provider paths, which on one, and which on neither, and it
corrects one divergence that a source read predicts and a measurement refutes)

Also filed 2026-08-12 and in the same state:
`W7-41-format-exception-subclasses.md` (re-adjudicated 2026-08-12 and it holds
on every load-bearing claim — `IllegalFormatException`'s `sealed … permits` list
really is those twelve, `UnknownFormatFlagsException` really is the one that
does not quote its flags, `IllegalFormatArgumentIndexException` really is
package-private, all read from `jdk25src`; the source side is present. What is
unverified is only the run. Its `%t` residuals gained a measured sibling —
W7-91) ·
`W7-43-formatmessage-substitution.md` ·
`W7-44-numberformat-enum-and-double-tostring.md` (**§3's wiring list is
CORRECTED, do not work from it**: it names `native-collections`'
`register_random_natives`, which `securerandom.rs` overwrites at a `vm_init`
boundary labelled *do not reorder*, so the fdlibm `log` landed in a body the VM
does not run and a third unregistered copy went unmentioned. Closed by W7-54 §7;
re-verified in the tree. §1's *"deliberately not done"* is likewise stale —
W7-80 did it. The ULP determination, the 7.3% census, the fdlibm port and the
`Enum.valueOf` half all stand) — the three that split out of
the differential's value divergences.

`W4-4-slot-index-species-sweep.md` is the census this whole family descends
from, and **its value is the census, not any fix in it**. A second banner was
added to it 2026-08-12: two of the three instrument holes that bounded every
table in it are now closed (the detector sits on the base allocator, not on one
funnel; `declared == 0` reports as `undeclared` instead of being silent), so its
figures are a snapshot of a narrower instrument. Its "intersect list 1 with
lists 2 and 3" procedure is **now sound as written** — the correction saying
list 1 cannot contain `AsynchronousSocketChannel` is itself stale. The one
source-level number in the family that is current, because a ratchet holds it
rather than prose: the `ClassId::new(0)` fallback population is **28** sites, 12
of them short, downward-only (the 14 was an arithmetic slip — see the W7-73 /
W7-74 row). Everything else needs the run.

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

`L8-securerandom-provider.md` joined this list on 2026-08-12: its out-of-file
patch is now **applied**. The three shadowing `SecureRandom` registrations in
`native-builtins/src/crypto_impl.rs` (`setSeed(J)V`, `setSeed([B)V`,
`<init>([B)V`) are deleted with their three no-op bodies, and the stale
registration-order comment is replaced. The record's own "synthetic-jdk only"
scope was re-derived against the registrars before the deletion and holds:
`register_crypto_impl_natives` has exactly one call site tree-wide
(`native-builtins/src/lib.rs:24167`), inside `register_synthetic_overrides`,
which is `#[cfg(feature = "synthetic-jdk")]`. All three triples are served by
`native-builtins/src/securerandom.rs` in every mode, so the deletion cannot move
`--jdk-only` or Compatible by any amount and cannot move a ratchet taken in
Compatible mode.

`W7-15-cipher-silently-wrong-algorithm.md` was in no index row at all. It is
the record for the ChaCha20-served-as-AES-256-ECB defect (tampered ciphertext
decrypted cleanly). Its headline is FIXED and verified at full parity by the
retired `W7-38-crypto-trio-verified` write-up (§2.0, second pass — cite it by
name, it is no longer in this directory); it is kept for the SHAPE — a dispatch line
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
| `L8`, the "Out of scope" framing | The residual blamed on the discarded constructor seed. | That discard matches HotSpot — `new SecureRandom(seed)` selects DRBG, whose `engineSetSeed` reseeds. The defect was the two `setSeed` no-ops undoing SHA1PRNG reseeding, and **the correct deletion was applied 2026-08-12** — see §2.3. Nothing is left to apply from either framing. |
| `W7-18` patch A, preferred form | "Delete both registrations and the function." | Measured to **hang**. The gated fallback form is what landed (`4c9482908`); both registrations must stay. |
| `W7-22` §4's named cause | "Build the singleton through its real constructor." | Written, measured **inert in both modes**, reverted. W7-25 found the real mechanism: a retired shadow reinstated by another registrar holding the same triple. |
| `W4-1`, two struck claims | The layout-discriminator rationale, and "`allowedModes == 0` → allow". | Both false today. A zero-mode `Lookup` now refuses (`6dd552ce2`); only the *unreadable* case still allows. |
| `L16`, "resolve the element separately" | "Teach `native_class_for_name` to strip `[`s and resolve the element itself — the HotSpot structure — rather than handing array descriptors to `loadClass` at all." | **Right about HotSpot's structure, wrong about the cost here, declined 2026-08-12.** It puts a second arbitrary-Java `loadClass` round trip on the success path of every array `Class.forName` — a path that already works — and then re-derives an array `Class` that `synthesize_array_class_for_loader` already builds correctly one layer down. Correcting the *name* achieves the whole observable contract (measured on HotSpot 25: `forName` names the element and `cause=null`; `loadClass` names the descriptor) with no new dispatch. |
| `W7-37` item 3 | "Stop the four raise sites rebuilding the `ClassCastException` text downstream by routing them through a shared builder." | **Premise removed by its own item 2, declined 2026-08-12.** Item 2 routed both mint sites — the interpreter's *and* `jit_checkcast`, which the record did not know was a second one — through `throw_runtime_error`, so the text is built once already. Item 3 now spans `opcodes.rs` for zero behaviour change. Item 1 (two `ClassId`s on the variant) is still live but must be an ADDITIVE variant: 21 of the 30 construction sites are free-text refusals with no two operands to carry. |

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
| Does the new nestmate field block pass on CratonVM? (L15) | `cratonvm --java-home "<jdk-25>" --real-jdk -cp regression-suite/build RJdkReflect`, and again with `--jdk-only`. Expect `PASS RJdkReflect (67 checks)` on both; HotSpot 25 already gives it. **A red here means the landed `check_field_access` narrowing is inert** — which is exactly what nothing had ever asked. The count moved 64 → 67 on 2026-08-12 with the three CONSTRUCTOR checks; those are the vector for L15's narrowing, and the two positives among them pass under the OLD (ungated) behaviour, so only the third row distinguishes the two. |
| Does the `unreflect*` mode gate hold, and does `Constructor.newInstance` still construct? (W6-8, L15) | `CRATONVM_MH_STRICT_INVOKEEXACT=1 cratonvm --java-home "<jdk-25>" --jdk-only -cp regression-suite/build RJdkHandles`, expecting `PASS RJdkHandles (54 checks)`, then the full `CRATONVM_ARGS=--jdk-only SUITE=all bash regression-suite/run.sh`. **The whole-suite run is the real instrument for L15's narrowing**: a new `IllegalAccessException` from a reflective construction anywhere in the corpus is the thing to look for, and the record's *Blast radius* section lists every site it has already been checked against. |
| Does arming `BootLoader.loadLibrary` flip `RJdkJni`'s `net` probe? (W5-1, W6-6) | `cratonvm --java-home "<jdk-25>"` on `RJdkJni` in both modes, **with and without** the one-line arming, diffing the `CK RJdkJni loadedLibrary=` line, against `java -cp regression-suite/build RJdkJni`. The arming can only turn a success into an `UnsatisfiedLinkError`, and the first library it claims is `net` — which `is_vm_provided_jdk_library` still carries *because* the dynamic rule cannot fire. **Take it on LINUX (2026-08-12).** The road at risk is the Linux boot-class `<clinit>` one (`lib.rs:14062-14067`, `java.net.NetworkInterface`); the plausible Windows caller is `Inflater.<clinit>` claiming `zip`, which changes nothing observable because `zip` already throws. A green Windows A/B measures the wrong road and is not a licence to arm. **The fixture now fails hard rather than diverging quietly:** `RJdkJni` asserts `loaded` is `"net"` (40 → **41 checks**), so a boot claim on `net` is an `AssertionError`, not a changed `CK` line — which also matters because `run.sh` SKIPS the cross-VM diff entirely when no HotSpot is on the host. And read the outcome carefully: the arming's input is CratonVM's own boot sequence, so a flip means CratonVM boot-touches `java.net` where HotSpot does not — a different defect, not one this bookkeeping fixes. |
| ~~Does W6-2's stream path really hand out an illegal provider?~~ **ANSWERED — the fixture asked for here already EXISTS.** W7-85 wrote it (`Rejected` ← `WrongFactory`, `Nulled` ← `NullProvider`) and 2026-08-12's second pass added the constructor-form pair (`Unsub` ← `NotSubProvider`, `Ctored` ← `HiddenCtor`). Four illegal providers, not one, each asserted from **both** `iterator()` and `stream()`. What is left is the RUN, not the writing. | `ONLY=RJdkModule CV=<cratonvm.exe> JDK="<jdk-25>" bash regression-suite/run.sh` — `run.sh`'s `class_args` supplies `--module-path regression-suite/build-modules --add-modules cratonvm.jdkonly.svc`, and **the module flags are required**: omitting them produces a harness error already misread once as a VM defect. Expect `PASS RJdkModule (155 checks)` on HotSpot and on both CratonVM arms. A red on **both** VMs means `modules-overlay/` did not land, not a VM defect — `javap -p -classpath regression-suite/build-modules/cratonvm.jdkonly.svc com.cratonvm.jdkonly.svc.internal.HiddenCtor` must show a **private** constructor. |
| Do the `--synthetic-jdk`-**mode** residuals reproduce? (W6-12, W7-10, L8, W7-63 §8) | A `--features synthetic-jdk` binary now **exists** (W7-50, 63/7 under `--jdk-only`). What has never been run is that binary in `--synthetic-jdk` **mode**, which is the only configuration these residuals live in. Feature ≠ mode — **and the mode requires the feature**, §1: a shipping binary refuses `--synthetic-jdk` with exit 1 at argument parsing, so this row can never be discharged by any binary already on disk here. It needs its own build, from a clean `git archive HEAD` export rather than a campaign-edited working tree. **Do not restate this as "the mode has never been executed, ever"** — that is falsified by `apps/h2database-suite-runner/RESULTS-20260721.md:91-95` (a synthetic-mode boot that died in `TestBase.<clinit>` on a missing `DateTimeFormatter.ofPattern`), and four in-tree runners pass the flag today (`h2database-`, `spring-`, `hib-`, `tomcat-suite-runner`). The true statement is narrower and unchanged in force: **the feature build has never been run in that mode, no `RJdk*` vector ever has, and nothing gating launches it** — not `regression-suite/`, not CI, not `scripts/`. P4B-SYNTHETIC-JDK-MODE-20260812.md. |
| Do the Linux / non-Windows `process.rs` arms even type-check? (W6-10, **W7-46 §8.1**) | A Linux host build, plus the advisory macOS CI job (`.github/workflows/cross-platform.yml`). Then `cargo test -p cratonvm-native-io process`. W6-10's finding 4 widened five signatures across arms that **have never been compiled by any lane that edited them** — those five are now audited by source read and consistent on every arm (`W7-46` §8.4), which rules out a one-arm `Result` or a drifted arity and rules out nothing inside a body. **The debt GREW on 2026-08-12**: `W7-46` §8.1 added `linux_liveness_and_start_time`, `linux_stat_line_times`, a third `foreign_start_time_or_dead` arm and a Linux-only `#[test]`, all written on Windows. This row is the gate on calling `W7-46`'s first residual closed. |
| W7-28's falsifier, carried forward after its retirement | `javac --release 25 --enable-preview` a 69.65535 class `P` and a 69.0 class `Q`; `cratonvm -cp . P` must fail with HotSpot's wording, `--enable-preview -cp . P` must run, `-cp . Q` must be unchanged, and the over-deny canary `B55p` must still run. |
| Is `RJdkProcess` back to `checks=53`? (W7-46) | `cratonvm --jdk-only -cp regression-suite/build RJdkProcess` vs HotSpot. **A control binary pre-dating the 2026-08-12 merges fails this identically, so the failure is pre-existing.** |
| Does `RJdkLogging` pass? (W7-25, W7-35, W7-56) | `CRATONVM_ARGS=--jdk-only SUITE=all bash regression-suite/run.sh`. **Same control-binary caveat.** |
| Does `StructuredTaskScope` behave? (W7-18) | `probes/StructuredTaskScopeProbe` on both arms, three consecutive byte-identical runs, watching for `join()` hanging. **And a THIRD arm, which is the one nothing has ever taken:** the same probe on a `--features synthetic-jdk` binary in `--synthetic-jdk` **mode**. W7-50's feature binary was run under `--jdk-only`, where every `StructuredTaskScope` registrar is unreachable, so it measured none of W7-18's B or C. That third arm is the single run both B's remainder and C are blocked on — **and it is a build, not a run**: the shipping binary refuses `--synthetic-jdk` outright (exit 1, §1 and the row above), so no probe invocation of any binary on this host can take it. |
| Does the `duplicate_registration_gate` still hold? (W6-9) | `cargo test -p cratonvm-native-builtins --test duplicate_registration_gate`. **W6-9 explicitly forbids re-seeding that number without a real run.** |
| Are W7-20's baselines correct now? (W7-62) | `JAVA_HOME=<jdk25> bash regression-suite/bridge-ratchet.sh` — one census, both gates. **ON LINUX**: both artefacts are keyed `<jdk-feature>/<os>` and the gates read the OS from the running host, so on Windows they look up `25/windows`, find nothing and exit 2 ("REFUSING") — not a pass. Expect three separable contributions (W7-20's retag; the `StringBuilder.insert` overloads, +12 here and 0 on `stub_ratchet`; the rest of the 182 commits). Anything else is a finding, not a re-freeze. |
| Is `stub_ratchet` where W7-62 says? | `cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture`. Expected to FIRE. **Paste what it prints; do not cite a remembered number.** The `StringBuilder.insert` overloads move it by **zero** (ambient `Bridge`); the unlanded `java/io/Print*` retirement would move it up by up to seven. |
| Do the boot-path witnesses actually see `vm_init`'s real-JDK arm? (W7-30 §9) | `cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture` and `--test duplicate_registration_gate`. Look for the `boot-path(scope):` line: it must say **48 registrars, 46 modelled, 2 unmodelled**. `8 … 8 … 0` is the pre-2026-08-12 blind locator, and it PASSES. |
| Does the new wiring ratchet hold? (W7-5 §6.3.1) | `cargo test -p cratonvm-native-builtins --test essential_wiring_ratchet -- --nocapture`. Three tests, no baseline, expected green. Not in CI yet. |
| Do the two `vm`-crate registrars cost the stub baseline anything? (W7-30 §6.1) | `cargo test -p cratonvm-vm --test stub_ratchet -- --nocapture`. No baseline; expected green. A failure on the source witness means `vm_init` lost the `set_category(Bridge)` around the instrument registrars, which silently drops 18 bridges under `--jdk-only`. |
| Does `probes/ListItrInterfaceProbe` go green in both modes? (W7-62, W7-16) | `javac -d <out> probes/ListItrInterfaceProbe.java`, then both arms with `--java-home`. Expect 16/16 in each; `al.*` red means broken instrument, not finding. |
| Is the differential still at 9? (W7-42) | Re-run `ShadowDifferentialProbe` against a binary built from current `dev`, **one compile, both sides**. Diff against W7-42's transcript and `PROBE-MANIFEST-DIGEST`, never against W7-4's retired oracle (**540** lines per RETIREMENT-20260812.md — 858 is this probe's declared-observable count, a different number). **Both rules are now mechanised** rather than left to discipline: `probes/shadow-differential.ps1` compiles once and hands both sides the same class directory, stores no baseline transcript, and on a digest mismatch prints **no diff at all**. One command: `.\probes\shadow-differential.ps1 -Cratonvm .\target\release\cratonvm.exe -Java "C:\jdk-25.0.3.9-hotspot\bin\java.exe" -VmArgs "--real-jdk"`; exit 0/1/2 (clean / divergence / instrument error). Read its `STDERR SIDECAR` block too — a run with sidecar lines and a clean diff is not a clean run. Nobody has taken this probe through the `--jdk-only` arm at all. |

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
* **A request is not a failure.** `--jdk-only-report`'s
  `compatibility-class-requested` rows record that a native *asked* the VM to
  fabricate a class and was refused. In the same run,
  `java/util/HashMap$KeyItr`, `java/util/Comparator$Native` and
  `java/util/Enumeration$Impl` are all requested-and-refused while `HashMap`
  iteration, the `Comparator` combinators and `Collections.enumeration`
  **pass** — the caller recovers onto real JDK bytecode, which is strict mode
  working as designed. So **the census over-reports** (reading its 19 rows
  alone would put 19 classes on the Phase 1 worklist when a handful break
  anything) and **a probe under-reports** (it sees only the routes it thought to
  take, and can never prove absence). **The live set is the intersection —
  refused *and* not recovered from.** Measured both ways on 2026-08-12: three of
  the nine Phase 1 blocking families came from a source audit no probe had a
  route to, and the audit's own scoping was wrong exactly where the probe was
  right — `Properties.propertyNames()` was predicted the widest failure and
  passes. Neither instrument's verdict survives without the other's.
  (JDK-ONLY-REPORT-CENSUS-20260812.md, P1-BASELINE-20260812.md.)
* **`--jdk-only-report` is a complete, machine-readable census and nothing was
  consuming it.**
  `cratonvm --jdk-only --explain-jdk-only --jdk-only-report r.json -cp <cp> <Main>`
  emitted 1569 violations on one ordinary program — 19
  `compatibility-class-requested`, 226 `native-shadows-bytecode`, 1324
  `synthetic-native-registered` — each with `class`, `requester` (`file:line`),
  `initiating_loader` and `reason`, under `schema_version: 1`. Three traps: the
  flag is silently ignored if placed **after** the main class, it needs a
  **Windows-shaped path** on this host, and it is **not written when the program
  calls `System.exit`**.
* **`--explain-jdk-only` reports boot refusals and only boot refusals.** All 13
  it names come from `vm/src/vm/vm_init.rs`'s boot block, before `main`; every
  runtime refusal is silent. A strict run that boots cleanly has proved nothing
  about what its natives will do at call time, and a record citing the banner as
  a completeness check is citing a boot-time instrument for a runtime question.
* **Do not read your own pipeline's behaviour as the VM's.** The shutdown
  `[jdk-only:shutdown]` report was filed as "names one class, apparently
  truncated"; it was a `head -40` on a 1570-line run and the report is complete.
  That is the **third** instance of the species in one campaign — a successful
  25-minute build read as failed because the command ended in `grep -c`, and a
  census claim whose `exit=0` was grep's. Check the last stage of your own
  command before attributing a truncation, a silence or an exit code to the VM.
* **"I cannot derive your claim from the source" is evidence, not a lane
  failing.** The lane handed the truncation claim above said its reading of the
  source disagreed, said so plainly, and attached the check that settled it —
  against the claim. Treat that response as a signal about the claim.
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
