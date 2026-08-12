# JDK-only mode — open defects

**Status:** OPEN, **21 records** (W3-6 and W5-2 retired 2026-08-12 by W7-46, W7-46 filed), reduced 2026-08-04, 2026-08-06, and
**2026-08-11** (thirty records retired — see `RETIREMENT-20260811.md`). Filed
2026-07-31 from wave-1 implementation findings.

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
  internal record tree **when it is fixed**, not when it is planned. The
  execution plan is elsewhere: the retired jdk-only wave-2 execution plan, twelve
  lanes with a file-ownership map, a conflict matrix and a verification protocol.
* Related non-known-issue docs:
  [`docs/jdk-only-runtime-services.md`](../../jdk-only-runtime-services.md),
  [`docs/jdk-only-native-review.md`](../../jdk-only-native-review.md),
  [`docs/jdk-only-migration.md`](../../jdk-only-migration.md).

### The corpus, and what a green corpus licenses

On 2026-08-07 a binary was built (dev `5d22671e3`, default features) and
`CRATONVM_ARGS="--jdk-only" bash regression-suite/run.sh` was run three times,
ABBA-interleaved, with the identical result each time: **53 passed, 1 failed**.
The one failure is `RMapGcStress`, which fails in every mode and every build and
is dev's own, not this campaign's. Compatible mode is unchanged at 30/1. A
`--features synthetic-jdk` binary run `--jdk-only` is still worse (48/6) — a
separate, previously unmeasured configuration. Campaign-level record: the
retired `STRICT-CORPUS-CAMPAIGN` record.

**What that does and does not license.** Every record whose corpus vector is in
`JDKONLY_CLASSES` (`regression-suite/run.sh:95`) has its vector passing, so its
"unverified by execution" caveat is discharged **at the vector level**. It is
not a per-assertion audit: a record claiming something narrower than its vector
asserts still owns that claim. Do not mass-flip statuses to FIXED on the
strength of this line — check what your record actually claims. The 2026-08-11
audit did exactly that, one record at a time, and the arithmetic came out
30 moved / 22 kept: most of what is left is *narrower than a vector*.

**If you re-measure, pass `--java-home`.** `run.sh` gives every CratonVM
invocation one; a hand-run that omits it measures the host's default JDK instead
of the JDK 25 image, which on this host inverted the per-mode verdict for
`RJdkModule`. Trace the real command rather than reconstructing it:
`ONLY="RJdkModule" bash -x regression-suite/run.sh 2>&1 | grep <binary>`.

---

## 2. What is still open

Every open item is an `L*`/`W*` record in this directory. There is no ranked
list any more — the old one is closed out and archived in §4. These are grouped
by what they need from you, not by danger, because none of them is now a silent
wrong answer on a corpus vector: the corpus is green.

### 2.1 Named residuals inside an otherwise-fixed record

The defect is fixed and the vector passes; a specific sibling case is not, and
the record says so and says why. Take these when you are already in the file.

| Record | The residual |
|---|---|
| `L8-securerandom-provider.md` | `crypto_impl.rs` registers `SecureRandom.<init>([B)V` to a no-op that stamps neither `algorithm` nor `provider`, and wins under synthetic-jdk. The clean fix is one deletion; the record names it. |
| `L15-nestmate-access-field-and-constructor.md` | `Constructor.newInstance` still has the caller-step gap `Method.invoke` and the field paths had. Route it through `caller_may_access_member`. |
| `L16-classnotfound-vs-noclassdeffound-shapes.md` | Our `ClassNotFoundException` names the array **descriptor**; HotSpot's `Class.forName` names the **element**. Closing it means teaching `native_class_for_name` to strip `[`s and resolve the element itself. |
| `W2-2-blocked-reader-async-close-wakeup.md` | `net_phase_e.rs::re1_socket_read_stream` still parks in `read()` with no close-awareness. Synthetic `java.net.Socket` surface, reachable only under `CRATONVM_SYNTHETIC_NET_SOCKETS`. |
| `W4-2-unnamed-accessor-bypasses-encapsulation.md` | `ServiceLoader` + an encapsulated module-path provider: the caller-sensitive `setAccessible` is refused, so `newInstance` is too. Also `is_package_exported_to` fails closed where `check_module_access` allows. |
| `W5-1-loadlibrary-allowlist-too-wide.md` · `W6-6-nativelibraries-load-fabricated-success.md` | The same residual from two roads: there is **no class-loader-scoped `loadedLibraryNames` bookkeeping in this VM**, so the JDK's dynamic "already loaded elsewhere" rule cannot fire. A static allowlist cannot model it. |
| `W4-3-security-getalgorithms-short-list.md` | `Security.getAlgorithms(type)` answers a plain `HashSet`, not `Collections.unmodifiableSet` — a caller asserting `UnsupportedOperationException` sees the divergence. Two real SHAKE digests are deliberately not advertised, because `message_digest::algorithm_supported` does not implement them. |
| ~~`W5-2-two-silently-skipped-process-checks.md`~~ | **RETIRED 2026-08-12 (W7-46).** Both residuals were closed 08-11, and the *detector* — a `checks=` count nothing asserted — was converted into an assertion plus a printed `skipped=` list. See `W7-46-process-cluster.md`. |
| `W6-2-module-serviceloader-provider-factory.md` | The constructor-form provider **subtype** check is absent — adding it could hard-fail every module-declared service in the JDK's own boot modules, and that was unmeasurable from the lane. |
| `W6-12-stampedlock-split-brain.md` | `java/util/Collections` is served by two registrars at different fidelities, so `unmodifiableSet(s).add(x)` throws while `unmodifiableList(l).add(x)` succeeds. Synthetic-jdk only. |
| `W7-46-process-cluster.md` | Two recorded-not-fixed, both stated rather than deferred: on **Linux**, one `isAlive0` still reads `/proc/<pid>` and then `/proc/<pid>/stat` — the same two-probe pid-recycle attribution hole the Windows arm just lost, on an arm this host cannot compile; and the four `java/lang/ProcessBuilder` triples registered by BOTH `register_phase57_process` (as `SyntheticStub`) and an untagged block in `register_enterprise_natives`, which collide only in **synthetic-jdk** mode. |

### 2.2 Whole surfaces that are still wrong

| Record | What is wrong |
|---|---|
| `W6-8-method-invoke-exports-gate.md` | Two live **OPEN** rows in its own inventory: `Field.get`/`Field.set` ask the `opens` question unconditionally and so **over-deny** public fields of exported-but-not-opened packages; and the entire `Lookup.unreflect*` / `find*` family has no module check of any kind. HotSpot throws there — measured. |
| `W2-3-module-descriptor-answers-empty-sets.md` | `ModuleDescriptor.modifiers()`, `Requires.compiledVersion()`, `version()`, `rawVersionString()` and `mainClass()` still have **no data source**: the bits are dropped at parse time or never surfaced through `NativeContext`. `RJdkModule` asserts none of them, which is exactly why the green corpus does not close this. |
| ~~`W3-6-processimpl-missing-natives.md`~~ | **RETIRED 2026-08-12 (W7-46).** Both stated residuals were already false: Windows `start_time`/`info0` were filled 08-11, and the `ProcessHandle` interface stubs were rerouted at a real measurement by W7-10. All ten `ProcessImpl` natives are registered, none is shadowed by a second registrar. The live successors are in `W7-46-process-cluster.md`. |
| `W2-1-strict-refuses-the-synthetic-stream-stack.md` | The stream stack is **SPLIT**: some sources divert into the `cratonvm/*` synthetic model, some run real `java.util.stream` bytecode. The record carries the inventory and a four-step staged path to a real `java.util.stream`; step 1 (the real path's own `ForkJoinTask.invoke()` defect) has since landed, so the path is now walkable. |

### 2.3 The slot-index species — swept, with a live tail

`W4-4-slot-index-species-sweep.md` and `W4-1-publiclookup-allowedmodes-never-checked.md`
are the two that remain. The species: **a native reads or writes a real JDK
object's field by the slot index the *synthetic* layout would have.** It fails
silently — the wrong field reads back empty, zero or null and the caller takes a
wrong branch with no exception. What is left:

* `reflect_invoke::build_string_set`, `ModuleLayer.modules()`, and three-slot
  `HashSet` allocations in `collections.rs` and `text_intl.rs`. **Latent, not
  live** — synthetic-jdk-only callers. Anything that promotes one of them to the
  real-JDK path must switch it to `build_real_layout_string_hashset` **first**.
* `lk_drop_lookup_mode` and `lk_in_method` read `allowedModes` by index while
  the by-name reader sits ten lines away. They survive by accident today.
* Two GC stale-local sites in `register_p59_module` (an unrooted `ObjectRef`
  held across a later allocation), left for whoever next touches that registrar.
* `W4-1` also lists the access rules `publicLookup()` deliberately does not
  enforce — all one-directional, admitting more than HotSpot, never less.

### 2.4 In flight on sibling branches — do not start these

Fixes are being written for these right now; treat them as owned.

`W3-4-forkjointask-status-flags-and-the-eager-default.md`,
`W6-5-vacuous-tests.md`, `W6-9-complete-erases-the-abnormal-record.md`,
`W7-1-treemap-views-and-iterator-remove-contract.md`, and the `W7-2` … `W7-6`
records.

---

## 3. Standing constraints for anyone working this list

* `native-builtins/tests/stub_ratchet.rs` asserts `BASELINE_SYNTHETIC_STUBS`
  **exactly**, with `SLACK = 0` (555 as of 2026-08-06; the constant carries its
  own re-freeze history — the figure moves, so read it there rather than here),
  and separately asserts only `total >= 8_000` as a vacuity floor. The floor is
  not a claim about the exact total — do not cite one. The strict-mode siblings
  in the same file assert zero `SyntheticStub` registrations and
  `strict_total >= 7_500`; that second number is a collapse detector, not a
  measurement, for the same reason.
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
  strict-mode claim in this directory.** Criterion 6 ("strict corpus green") is
  not a formality to tick after the list is done; it is where the defects are.
  The first breadth workload ever pointed at strict mode
  (`probes/JdkOnlyCensusLoadProbe.java`) found that `--jdk-only` could not start
  a thread, and had been unable to for as long as anyone can date.
* **Anchor a `file:line` on the marker tag, not the number.** Wave-2 sites carry
  `// JDK-ONLY-WAVE2:`, deferred observations `// JDK-ONLY-NOTE:`, per-registrar
  category verdicts `// JDK-ONLY-CLASSIFY:`, field-slot verdicts
  `// JDK-ONLY-LAYOUT:`. Those tags are stable; the numbers are not. These
  records were first written on 2026-07-31 against a working tree whose
  uncommitted wave-1 edits were destroyed by an external `git restore` and then
  re-landed by fresh agents — equivalent in design, different in detail. Every
  `file:line` was re-verified against the re-landed tree, and claims that could
  not be re-verified are marked **UNVERIFIED** inline rather than deleted.
* **Before believing a record that says a patch was never applied, grep for the
  patch's token.** Fourteen records claimed a hand-off was still pending for a
  change that is in the tree today; see `RETIREMENT-20260811.md`. Lanes that
  could not edit a file wrote the patch down and handed it off, and nobody went
  back to the record when it landed.

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
`CRATONVM_ENFORCE_NATIVE_SHADOW=1`. Two more records closed the same day: the
`ThreadPoolExecutor.execute` receiver-shape copies
(`retired/jdk-only-wave2-threadpoolexecutor-execute-receiver-shape-RETIRED-20260806.md`)
and VM-internal classes mislabelled `CompatibilityStub`
(`retired/jdk-only-wave2-vm-internal-classes-mislabelled-RETIRED-20260806.md`),
which also deleted `Class::is_synthetic_stub`.

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

**2026-08-11 — the bridge wave, and this audit.** The reclassification question
closed: the population is now five slack-free ratchets with committed baselines
rather than a number in a document — `bridge_without_acc_native` 8,911,
`bridge_shadows_bytecode` 6,066, `bridge_stated_shadows_bytecode` 24,
`superseded_kind_disagreements` 52, `superseded_stub_lost_to_admitted` 4, all
scored by `regression-suite/bridge-ratchet.sh` (the retired
`bridge-reclassification-wave` write-up). What remains open there is the 6,066-row
shadow population itself, for the reason already established — a class's state has
to become real before its shadow can be retired. Then this directory's own
retirement audit: 30 records moved, 22 kept, in `RETIREMENT-20260811.md`.

**The ranked work list this file used to carry is closed.** Items 1 through 11
are all either fixed or retired; the last of them (item 1, `NativeKind` ambient
and defaulting to `SyntheticStub`) was retired 2026-08-06 and the
reclassification it pointed at closed 2026-08-11. Item 11's thirteen
cross-cutting findings all closed and its record left the public tree on
2026-08-10. The three blockers that gated the list are gone: `StringJoiner`
heap-reference integrity retired 2026-08-04 (did not reproduce), real
`ThreadPoolExecutor` field initialisation retired 2026-08-06 at *registration*
(real-JDK mode registers no `Executors` pool factory at all, so a half-built
executor has no code path that can mint it), and RKC16N.6 was spent by moving
the `String` policy to registration.

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
