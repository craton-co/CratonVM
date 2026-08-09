# JDK-only wave 2 — parallel execution plan

**Purpose:** finish `--jdk-only` (contract:
[`../jdk-only-mode.md`](../jdk-only-mode.md)) with as many people working at
once as the file layout allows. This is the *guide*; each lane has its own doc.

**This is not the defect list.** The evidence — what is broken, how it was
measured, blast radius — lives in
[`docs/known-issues/jdk-only/`](../../known-issues/jdk-only/) and stays there.
A record moves to the internal record tree when it is fixed. This directory says **who
can work on what, simultaneously, without colliding.**

---

## The end state is two modes, and it is a rename

**This governs every lane. Read it before deleting anything.**

There are three modes today — `--features synthetic-jdk`, `--real-jdk`
(`Compatible`) and `--jdk-only` (strict). There will be two, and they arrive by
**renaming**, not by purging:

| today | becomes | means |
|---|---|---|
| `--jdk-only` | **`--real-jdk`** | real JDK bytecode is authoritative; no synthetic stub is admitted |
| `--real-jdk` (`Compatible`) | **`--synthetic-jdk`** | real JDK image, plus the Rust natives that make apps work today |
| `--features synthetic-jdk` | folds into `--synthetic-jdk` | |

**The rule that follows, and it is not negotiable: no synthetic method that is
used by either surviving mode may be removed.** A wave that ends with
`--jdk-only` renamed to `--real-jdk` must not have taken anything out of the
mode that gets renamed to `--synthetic-jdk`. Strict mode declines to **admit** a
native — at registration, by `NativeKind` — and that is the entire mechanism.
Deleting the Rust function is not an implementation of "strict refuses it"; it
is a different, larger change that also breaks the other mode.

So every lane's deletions have to be sorted into two buckets, and the sorting is
the work:

* **Policy artefacts — delete.** Hard-coded name lists, per-dispatch-path copies
  of one decision, `matches!` chains, `compat_native_wins = true`. They answer
  "who wins dispatch", and `resolve_dispatch` should answer it from
  `NativeKind` + `Method::code()` instead. Item 3's `String` lists and item 7's
  eight-plus-one sites are this bucket.
* **Implementations — keep.** The natives themselves. If one is wrong in
  `Compatible` mode, fix it. If it must not run under strict policy, tag it
  `SyntheticStub`. "Unreachable in strict mode" is never a reason to delete it,
  because it is reachable in the other one.

Two records already say the wrong thing under this rule and are corrected in
place: the evidence record's *"`native_es_execute` can go away entirely"* and
L11's step 5. What goes away is its **admission**, not the function.

### The axis this rule is NOT about

`set_drop_real_layout_synthetic` is a **correctness** gate, not a mode gate, and
the two get conflated. It answers "the real class is loaded, and this synthetic
surface writes a layout that corrupts it" — `StringJoiner`, `Cleaner`,
`ReferenceQueue`, `Permissions`, `LinkedBlockingDeque`, the legacy regex
natives, `ScheduledThreadPoolExecutor`, and as of 2026-08-06 the `Executors`
pool factories. Those are dropped wherever real bytecode exists, in **every**
mode that loads a real image, and they were dropped there before `--jdk-only`
existed. They are not "synthetic methods used in `--synthetic-jdk`" being
removed; they are synthetic methods that were **breaking** the mode, and the
build that genuinely has no real bytecode (`--features synthetic-jdk`) never
sets the flag and keeps every one of them.

If a lane wants to drop a native in `Compatible` mode, it must say which axis it
is on, and show the corruption on the correctness axis. Saying "strict does not
need it" is the policy axis and is not sufficient.

## Start here

1. Read the contract's §1.3, §1.4, §5, §7 and §11. Every lane is judged against
   those, not against this plan.
2. Pick a lane from the table below whose **owned files** nobody else holds.
3. Read that lane's doc. It states its own current state, steps, verification
   and "done when".
4. Follow *Verification protocol* below. It is not optional and it is the part
   most likely to be skipped.

## The lanes

| Lane | What | Owned files (the parallelism contract) | Gated on | Effort |
|---|---|---|---|---|
| [L1](L1-classloader-side-table.md) **DONE 2026-08-05** | Move the four VM-internal loader fields out of the object | `native-builtins/src/classloader.rs`, `classloader_real.rs` | — | M |
| [L2](L2-native-map-init-by-name.md) **DONE 2026-08-04** | `native_map_init`'s raw `MAP_FIELD_*` branch → by-name | `native-collections/src/lib.rs` | — | M |
| [L3](L3-scanner-membername-residual.md) **DONE 2026-08-05** | Trace + fix the last unclassified layout rows | `native-builtins/src/phases_early.rs`, `lang_invoke.rs`, **`native-io/src/lib.rs`** | — | S |
| [L4](L4-overlay-detector-blind-spots.md) **DONE 2026-08-05** | Detector misses reads, same-kind writes, null writes | `vm/src/vm/vm_exec.rs` (hunter only), `classloading/src/shadow_layout.rs` | — | M |
| L5 (`jdk-only-wave2-L5-nativekind-native-io-DONE-20260805.md`) **DONE 2026-08-05** | `register_with_kind` migration, `native-io` first — 87 registrations stated, 117 left inherited on purpose ([residuals](../../known-issues/jdk-only/l5-native-io-bridge-residuals.md)) | `native-io/src/*.rs` | — | M |
| L5b/L5c (`jdk-only-wave2-L5bc-nativekind-awt-builtins-DONE-20260805.md`) **DONE 2026-08-05** | The same migration for the other crates — `native-awt` 21 and `native-builtins` 582 registrations stated; `native-collections` measured and has **zero** to state ([residuals](../../known-issues/jdk-only/l5bc-awt-builtins-bridge-residuals.md)) | `native-awt/src/*.rs`, `native-builtins/src/*.rs` | L5 | M |
| L5-residuals **DONE 2026-08-05** | Closes what L5/L5b/L5c left: the `unknown`-marked registrars, all 31 mixed sites, `vm/src/runtime/instrument.rs`, and the two instrument gaps that made three verdicts wrong ([record](../../known-issues/jdk-only/census-asks-one-class-on-one-platform.md)) | `native-io/`, `native-builtins/`, `native-awt/`, `vm/src/runtime/instrument.rs`, `scripts/`, `probes/` | L5, L5b/L5c | M |
| L6 (`L6-unadjudicated-bridge-ratchet-DONE-20260805.md`) **DONE 2026-08-05** | Ratchet the unadjudicated `Bridge` rows — frozen at **10,069** (25/linux); L5 did not move it, and could not: the 87 rows L5 stated are exactly the ones that DO have an `ACC_NATIVE` target | `regression-suite/`, `scripts/` | — | S |
| L7 (`L7-ensure-synthetic-class-migration-RETIRED-20260805.md`) **DONE 2026-08-05** | Make fabrication refusable, migrate the callers that fire — 10 fire, not 52; a strict boot fabricates **zero** compatibility classes now | `classloading/src/class_manager.rs` + callers | — | M |
| L8 (`jdk-only-wave2-L8-strict-corpus-green-RETIRED-20260805.md`) **RETIRED 2026-08-05** | Criterion 6: strict corpus green | `probes/`, `regression-suite/`, `scripts/` | — | L |
| [L9](L9-blocker-rkc16n6-string.md) | ~~**Blocker.** Real `String` bytecode during JDK `<clinit>`~~ **CLOSED 2026-08-04** — did not reproduce; the four policy copies were measured inert and deleted | `vm/src/runtime/interpreter/` | — | L |
| L10 (`L10-blocker-threadpool-init-DONE-20260806.md`) **DONE 2026-08-06** | ~~**Blocker.** Real `ThreadPoolExecutor` field init~~ — real-JDK mode registers **no** `Executors` pool factory, so the real bytecode constructs every executor. Owned `native-collections/src/lib.rs` and **did not touch it**: the defect was one arm of `NativeMethodRegistry::register` | ~~`native-collections/src/lib.rs`~~ → `native-api/src/registry.rs` | — | L |
| [L11](L11-delete-the-hardcoded-lists.md) **DONE 2026-08-06** | Items 3 + 7: delete the lists — item 3 2026-08-04, item 7 2026-08-06 (all eight receiver-shape sites, the ninth receiver-blind arm, and the probe helper) | `native_override.rs`, `vm_exec.rs`, `invoke.rs`, `dispatch_virtual.rs` | ~~L9~~, ~~L10~~ | M |
| [L12](L12-item11-residuals.md) | Item 11 §2/§4/§6/§8/§9/§10/§11 | mixed — see doc | partly L5 | L |

**Lane L6 grew a second gate, 2026-08-06.** `regression-suite/bridge-ratchet.sh`
now scores `scripts/jdk-only-kind-map.py` over the same census: a per-registration
freeze of `NativeKind`, because L6's two counts cannot see the dangerous
direction — a `Bridge`→`SyntheticStub` mass re-tag takes rows *out* of the
`Bridge` population, so both numbers fall and the gate says "IMPROVED". Landed
with the ambient-category fix that retired
`known-issues/jdk-only/native-kind-is-ambient-and-defaults-to-syntheticstub.md`:
`current_category` is an `Option` and scoped, nine unscoped registrars state
their kind, and 58 triples `--jdk-only` was admitting by registration order are
refused — including the `Function$Identity` / `UnaryOperator.identity` copies in
`phases_late/streams.rs` that L7 item 4 did not reach.

**Every lane in the table above is done but L12.** (L1–L8 landed; L9 closed
2026-08-04; L10 landed 2026-08-06 and with it the last blocker, and L11 spent it
the same day — item 7's nine dispatch sites are deleted. L12 — item 11's
residuals — is what is left.)

**A SECOND pool is in flight. Its lanes are `SC-<n>`, and its FILENAMES collide
with this table's lane numbers.** The strict-corpus campaign of 2026-08-06/07
staffed one lane per corpus failure and filed each under
`docs/known-issues/jdk-only/L<n>-*.md` — filenames that are kept as they are,
because lanes are still writing them. Those numbers have **nothing** to do with
the rows above: `L3-scanner-membername-residual.md` in *this* directory is the
Scanner/MemberName layout lane, while
`known-issues/jdk-only/L3-definehiddenclass-returns-null.md` is **SC-3**,
`Lookup.defineHiddenClass`. **Cite the full path AND the `SC-<n>` label; never a
bare lane number.** The reconciliation table — `SC-<n>` → record → subsystem →
the wave-2 lane it collides with — and the campaign record are
[`STRICT-CORPUS-CAMPAIGN-20260807.md`](STRICT-CORPUS-CAMPAIGN-20260807.md).

## Conflict matrix — read before claiming a second lane

Two lanes are safe together iff they own disjoint files. The collisions that
exist:

| Pair | Collides on | Resolution |
|---|---|---|
| L2 ↔ L10 | `native-collections/src/lib.rs` | **Moot.** L2 landed 2026-08-04; L10 landed 2026-08-06 without editing that file at all — the defect turned out to be in `native-api/src/registry.rs`. The conflict this row was written to manage never arose, which is the second time this wave a lane's owned-file claim did not survive contact with the code (L3's was the first). |
| L4 ↔ L11 | `vm/src/vm/vm_exec.rs` | L4 owns the overlay hunter (~line 3070–3200); L11 owns dispatch (~14700, ~22700). Disjoint regions in one file — coordinate, do not both `git add -A`. |
| L3 ↔ L12 | `lang_invoke.rs` | **Resolved:** L3 landed 2026-08-05; L12 rebases onto it. |
| L3 ↔ L5 | `native-io/src/lib.rs` | **Resolved the same way.** L3 had to take this file — the Scanner writer was there, not in `phases_early.rs` — but it touched only the `Scanner` natives and the delimiter regex cache, no `register*` call site. |
| L5/L6/L12 ↔ each other | `register_with_kind` semantics | **Resolved:** L6 landed 2026-08-05 and changed no call site — it reads the census and freezes two numbers. L5's migration now has to move them; re-freeze with `sh regression-suite/bridge-ratchet.sh --update-baseline --note "…"` in the same change. L12 §4 is JIT-side. |

`native-collections/src/lib.rs` is 55k lines and `vm_exec.rs` is 26k — two
people in either will conflict even in "different" areas. Treat whole-file
ownership as the unit.

## Verification protocol

Non-negotiable, because this codebase has repeatedly produced fixes that
measured as working and were not.

1. **A/B against the pre-fix binary, same workload, same run shape.** An
   aggregate over several probes and a single run are *not* a before/after —
   that error made an inert guard read as a working one on 2026-08-04.
2. **Both modes plus a HotSpot control.** `--jdk-only`, `--real-jdk`, and real
   HotSpot on the same probe. A divergence present in *both* CratonVM modes is
   not a strict-mode defect. **Give the control the same arguments the suite
   gives it** — class path, `--module-path`, `--add-modules`, the lot.
   `regression-suite/run.sh` builds those from one variable per arm precisely so
   they cannot drift; an ad-hoc control that drops or mistypes one measures
   nothing, and on 2026-08-06 that produced a wrong classification (see *Failure
   modes*). If the control fails, run it three times before believing it: an
   intermittent failure is a racy assertion in the vector, not a contract.
3. **Check exit status, not just stdout.** A timed-out run prints a truncated
   transcript that reads exactly like a short clean one.
4. **Gate the crate you actually edited.** An earlier gate set omitted
   `native-builtins` while fixing a `native-builtins` file.
5. **A guard must be shown to fail.** Inject the violation, watch it fail,
   revert. See *Failure modes* below.

Standing commands:

```sh
# layout defects
CRATONVM_DBG=overlay,overlay-all cratonvm --real-jdk --java-home $JDK -cp probes JdkOnlyCensusLoadProbe
# who wrote it (Rust frame + file:line; the Java frames mislead)
CRATONVM_DBG=overlay,overlay-all,overlay-bt=ClassName cratonvm ...
# the ThreadPoolExecutor receiver-shape predicate, one line per call.
# Universally-true is TWO claims: no `real=false` AND at least one `real=true`.
# (CRATONVM_DBG_TPE_SHAPE was removed 2026-08-06 with the predicate it measured)
cratonvm --jdk-only --java-home $JDK -cp probes L10ThreadPoolInitProbe
# native adjudication
cratonvm --real-jdk --java-home $JDK --explain-jdk-only --dump-native-registry c.json -cp probes JdkOnlyCensusLoadProbe
python3 scripts/jdk-only-adjudicate.py c.json     # section 7 is the ratchet's block
# ...and the gate over it (takes its own census; needs no probe)
JAVA_HOME=$JDK sh regression-suite/bridge-ratchet.sh
sh regression-suite/bridge-ratchet.sh --selftest  # hermetic, no VM, no JDK
# both at once
JAVA_HOME=$JDK CV=... PROBES=... scripts/jdk-only-measure-refusals-and-overlays.sh
```

## Failure modes this feature keeps producing

Every one of these has happened, most more than once. They are why the protocol
above looks paranoid.

* **A guard that cannot fail.** Three in one evening: a test that froze a
  divergence; `object_num_fields(x) >= N` (returns the requested count either
  way, so it cannot distinguish layouts); `slot < object_num_fields(this)`
  (stops an out-of-range write, not a wrong-field one). **Identify a layout by a
  field NAME the real class declares** — `vform`, `scheme`, `protocol`, `type` —
  never by count.
* **A number in a record is a claim, not a measurement.** Six were wrong:
  "about 8,000" registrations (11,909), "1,195 mis-tagged" (10,084), "52 call
  sites" (3 fire), "costs every inline-cached call" (zero), "sweep four crates"
  (24 named slots), and L10's own verification bullet, "`cargo test --release -p
  cratonvm-native-collections --lib` (94 tests)" — it is 105, and the lane never
  edited that crate. Take the census before sizing anything.
* **A verdict measured against a broken oracle is worse than no verdict.** The
  strict-corpus campaign's ad-hoc HotSpot arm ran `RJdkModule` with the module
  name `cratonvm.regression.jdkonly`; the module in this tree is
  `cratonvm.jdkonly.svc` (`regression-suite/modules/cratonvm.jdkonly.svc/module-info.java`,
  and `run.sh:58`). HotSpot 25 duly failed, the vector was written off as a bad
  one, and a **real CratonVM defect** — `Module.getLayer()` not answering the
  boot layer, in both modes, `RJdkModule.java:57` — was nearly suppressed for the
  whole campaign. With the right invocation HotSpot prints `PASS RJdkModule (44
  checks)`. **A "HotSpot fails it too" verdict is only as good as the
  invocation**, and step 2 of the protocol above is where it gets earned.
  `RJdkProcess` is the same question with the opposite answer — HotSpot really
  does fail it, intermittently, and the *intermittency* was the tell — so the
  rule is not "distrust the oracle", it is "run the oracle the way the suite
  runs it, and run it more than once".
* **An absent instrument reads exactly like a satisfied one.** L10's first A-arm
  reading was `true=0 false=0`, which says "these dispatch sites are dead" — a
  tidy finding, and wrong: the binary was linked before the flag existed. Print
  the successes as well as the failures, and confirm the instrument is IN the
  binary before believing a zero from it.
* **A brief can over-report a lane as easily as under-report it.** L10 was given
  a 55k-line file to own and a list of null fields to go fix. The file was never
  touched and the fields had been initialised for four weeks; the defect was one
  `matches!` arm in a different crate. Run the probes against the pre-fix binary
  *before* planning the lane — the brief is a hypothesis about where the code
  is, and it is the cheapest thing to test.
* **The Java stack does not name the native writer.** The `ClassLoaders` rows
  show `BufferedWriter.initialBufferSize()`. Use `overlay-bt`.
* **A comment can describe control flow the code does not have.** §7 step 3
  claimed it fell through "past the JNI chain to the bytecode path"; that arm
  has three outcomes and none is the bytecode. Follow the `else` chain.
* **The shared build host lies.** Builds get OOM-killed (`SIGKILL`, never a
  compile error) and ssh drops mid-command. Check `MemAvailable` before
  building; treat load > 80 as invalidating; verify a background job exists
  rather than assuming your launch survived.
* **The census cannot see a same-kind wrong-field write, so a lane brief
  written from the census under-reports its own defect.** L1's step 4 said the
  three `ClassLoader` REFERENCE slots were safe because they "are already
  written by name too" — but the by-name write and the index write land on
  DIFFERENT fields (`name` is slot 1, and the index write put the parent
  ClassLoader there). The value-tag predicate only flags cross-type-class
  coercions, so zero of it appeared in any census.
  `classloader_parent` was returning the platform loader's own name String as
  its parent, and nothing measured it until a behavioural probe was diffed
  against the host JDK. **Read the writer against `javap` of the real class;
  the census is a floor, and for reference-into-reference it was a floor of
  zero.** *(Corrected 2026-08-05: L4's shadow-layout diff compares our model
  against the image by NAME and found 23 such slots on its first run. The floor
  is zero only where the model slot is anonymous — `_fN`, which asserts
  nothing.)*

## Definition of done (contract §11)

1. Strict registry contains zero `SyntheticStub` entries — **passes today**, but
   because `register()` refuses them at the door, not because they are gone.
2. Zero synthetic-stub invocations through any path.
3. Zero `CompatibilityClassRequested` violations on the corpus.
4. `Compatible` mode byte-for-byte unchanged.
5. No process globals for this feature's state.
6. **Strict corpus green.** The unmeasured half until 2026-08-04, and where the
   defects turned out to be. **Not green**, and now measured on every CI run by
   `scripts/jdk-only-strict-probes.sh` — see
   L8 retired (`jdk-only-wave2-L8-strict-corpus-green-RETIRED-20260805.md`).
   The open records between here and green are all **compatibility** defects
   that `--jdk-only` did not introduce. *(The count "four open records" was
   written on 2026-08-05 and is not re-verified here — treat it as
   **UNVERIFIED**; the 2026-08-06/07 corpus run below is the current number.)*

   **Two instruments answer criterion 6 and they disagree by an order of
   magnitude. Never quote one alone.**

   | instrument | what it measures | reading, 2026-08-07 |
   |---|---|---|
   | `scripts/jdk-only-strict-probes.sh` | five breadth probes, scored section-by-section against a HotSpot control | **2 baselined divergences** (`scripts/baselines/jdk-only-strict-corpus-25-linux.txt`, both `JdkOnlyPlatformProbe/*/vthreads` — one per arm, so neither is strict-only) |
   | `regression-suite/run.sh` `JDKONLY_CLASSES` | 21 hostile vectors, asserting JDK contracts check by check — **strict arm only**, never run in Compatible mode (`run.sh:83-86`) | **14 failures** |

   Both are honest; they ask different questions. Reading criterion 6 off the
   probe baseline **alone** badly overstates where the mode is — a green ratchet
   over a narrow instrument is evidence about the instrument's reach, not about
   the mode. Quote both, always. See
   [`STRICT-CORPUS-CAMPAIGN-20260807.md`](STRICT-CORPUS-CAMPAIGN-20260807.md).

## State as of 2026-08-05

Closed: items 8, 9, 10; item 11 §1 (answered — its cost is zero), §5
(retracted), §12, §13. Fixed outside the list: `--jdk-only` could not start a
thread; `MethodType.toString()`; eight missing system properties.

Item 2: 24 measured slots → **15 open** (→ **12** after L3, 2026-08-05: the two
`Scanner` rows and the `MemberName` row are gone, leaving `URI` ×2 and
`Proxy`). Item 1: instrument built, migration not started. Items 3/7: blocked
on L9/L10.

**Update, later on 2026-08-04 — L2 landed.** The `Properties` family is gone
from the census (slots 2 and 3 → 0) along with the `HashMap` slot-2 `Int` over
`[`; `map_state`, `map_resize`, `resync_view_set`,
`hashmap_serialized_capacity` and both `HashMap` constructors resolve `table`
on the receiver instead of on a fixed `java/util/HashMap`. Re-measured with
the three standing probes plus the new `probes/MapLayoutMatrixProbe`: 6
classes / 15 slots before, 5 / 12 after — not comparable to the 24 above, see
the evidence record for why. `Compatible` corpus byte-identical.

~~Two things that block a clean read of item 2's remaining rows, both L4's:
a null written over a primitive is still invisible to the hunter, and a
same-kind wrong-slot write always was.~~ **Both closed 2026-08-05 — see the L4
update below.** Every count in item 2 is still a floor, now because three probes
are not Spring Boot rather than because the instrument is half-blind.

**Update, 2026-08-05 — L1 landed.** The eight `ClassLoaders` census rows are
gone (slots 0/3/4/6 on each built-in loader → 0), A/B'd against the pre-fix
binary on JDK 25 with every other row byte-identical, and loader identity is
pinned against HotSpot 25 by `probes/L1LoaderIdentityProbe`. It also took the
three same-kind REFERENCE slots its own brief had ruled out — which is the
second half of the floor warning above, now with a concrete instance: **it was
returning the platform loader's `name` String as its parent.** Two divergences
its probe found are filed as residuals in the lane doc, both pre-existing and
both outside its owned files (`isAssignableFrom` true across unrelated loaders;
a duplicate `defineClass` not raising `LinkageError`).

**Update, 2026-08-05 — L9 is closed and item 3 with it.** The blocker did not
reproduce: its April symptom was a class-load defect fixed the same month by
RKC16N.9, and all four copies of the forced-native `java/lang/String` policy it
was about turned out to decide nothing — `resolve_step1_native` dispatches a
registered native on the triple alone, before any of them runs, which a binary
with the lists deleted confirmed by producing a byte-identical 392-case
transcript and identical invocation counts. Two lanes were planned around a
comment. Item 7 is still blocked on L10.

**Update, 2026-08-06 - L10 and L11 are closed, and item 7 with them.** L10 was
not a blocker: `Executors.new*ThreadPool()` already returns objects built by the
real `ThreadPoolExecutor.<init>`, and `apps/executor_probe/ExecProbe.java` (path corrected 2026-08-07; there is no `probes/ExecProbe.java`) shows all six
factory shapes running asynchronously on a worker thread in both modes, matching
HotSpot. That made the per-INSTANCE question the eight receiver-shape probes
existed to answer vacuous, so all eight went, together with the ninth
receiver-blind `force_native` arm and the probe helper. This is the SECOND lane
pair planned around a stale premise - L9's was a comment, L10's was a `docs/`
row nobody re-measured after the fix it asked for had landed. **Re-measure the
blocker before staffing the lane.** The replacement is the same shape as item
3's: the decision moved to registration (`NativeKind::SyntheticStub` plus one
allow-list entry), where no dispatch path can disagree with another.

That also starts item 1's migration: the four reviewed `java/lang/String`
fast-regex natives plus `hashCode` are `register_with_kind`'s **first callers**,
so `kind_stated` is no longer `false` on all 11,909 rows.

**Update, 2026-08-05 — L3 landed, and item 2's table is down to `URI` and
`Proxy`.** `java/util/Scanner` slots 3/4 and `java/lang/invoke/MemberName` slot
4 are gone from the census (1 → 0, 1 → 0 and 7 → 0), A/B'd against the pre-fix
binary over both standing probes × both modes with the benign `HashMap` row
byte-identical and no other row present in either arm. Two new probes,
`L3ScannerLayoutProbe` and `L3MemberNameProbe`, are byte-identical to HotSpot
25 in both modes; the Scanner one FAILS on the pre-fix binary, which is what
makes the census delta mean something.

Three things worth carrying into the remaining lanes:

* **A lane brief scoped from the census under-reports its own defect — again.**
  L1 found this with the `ClassLoader` reference slots; L3 found the `Scanner`
  model was writing FIVE wrong fields, of which the census could see two. The
  other three are reference-into-reference. Read the writer against `javap`.
* **The file named in a brief may not be where the code is.** The brief said
  `phases_early.rs`, three fields; `overlay-bt` said `native-io/src/lib.rs`,
  five. There were two Scanner implementations over two different layouts, and
  the one in the brief had been dead in every configuration since
  `register_io_natives` started running two lines after `register_builtins`.
  The dead copy is deleted rather than kept in sync.
* **Kind 3 does not always need a side table.** `MemberName`'s vmindex sentinel
  was an `Int` written to a reference slot, which `set_field` coerces to null —
  the very condition the census reports. It had never reached the object in any
  layout, so both readers already answered 0 and removing the write is
  behaviour-preserving. A comment called it "critical". Check whether a value
  survives its own write before building storage for it.

L3 also fixed three host-JDK divergences the new probe surfaced next to the
layout rows (the default delimiter's pattern string, `next()` leaving the
position past the delimiter — the shape the JDK's own `NextIntNextLineTest`
exists to catch — and a constructor `MethodHandle`'s `type()` returning
`void`), and made `Scanner.close()` mean something. Those change `Compatible`
mode, from silently wrong to matching HotSpot, exactly as L2's
`try_set_jdk_map_field` fix did; criterion 4 is about not perturbing
`Compatible`, not about preserving its bugs.

**Update, 2026-08-05 — L4 landed, and item 2's work list roughly quadrupled.**
The census goes from **4 distinct sites to 135** on the same three probes in
both modes, with every pre-fix row preserved at its count. Reads are
instrumented (70 read sites where there were none), `Object(None)` over a
primitive is flagged (two new write rows the pre-fix binary is silent on), and
the same-kind wrong-slot case is covered by a **shadow-layout diff**:
`synthetic_stub_fields` is the model the natives were written against, so when
the class also has real bytes the two layouts are diffed once at define time and
every disagreeing index reported.

That diff found **23 slots across 12 classes where our model names a different
field than the image declares** (152 disagreeing slots in all, across 73 of the
156 modelled classes the probes reach) — the kind-5 family this README's own lesson
below calls "a floor of zero". `java/lang/ThreadGroup` has both `name`/`parent`
and `daemon`/`maxPriority` **transposed**; `java/lang/Thread` slot 5 writes a
`ClassLoader` over `holder`; `ProtectionDomain`, `CodeSource`, the buffered
IO wrappers and `java/lang/reflect/{Field,Method,Constructor}` are all in the
list. None is fixed — each is its own change with its own A/B, and the list is
the lane's output. L4 also settled L3 step 3 in passing: `Scanner`'s model is
`instance_fields(5)`, and slots 3/4 are the real `delimPattern` /
`hasNextPattern`.

**The lesson below needs one correction, not a rewrite.** "For
reference-into-reference the census is a floor of zero" was true of a detector
that only compared value tags. Comparing our *model* against the image by NAME
is a different signal and it finds them. What stays unfindable is the case where
the model slot is anonymous (`_fN`) — there the model asserts nothing. Naming
more of `synthetic_stub_fields` is what shrinks that, and it is the follow-up.

**Update, 2026-08-05 — L6 landed; the `NativeKind` work is now measurable.**
`regression-suite/bridge-ratchet.sh` + `scripts/jdk-only-bridge-ratchet.py`
freeze the unadjudicated-`Bridge` population against
`scripts/baselines/jdk-only-bridge-ratchet.json`, slack-free, with a vacuity
floor. Frozen on dev `d010d611b4` / JDK 25.0.3 / linux at **10,069 of 10,842
`Bridge` rows with no `ACC_NATIVE` target**, of which **4,755 shadow concrete
bytecode** (separately ratcheted — that is the subgroup §7 step 3's decline can
reach). Re-measured, not copied: the brief's 10,084/10,844 was 2026-08-04, before
L1/L2/L9 and the `String` residuals.

Three things worth carrying into the other lanes:

* **The baseline key is `<jdk-feature>/<os>`, and a missing entry is a refusal
  (exit 2), never a pass.** The registrars are platform-conditional, so a Linux
  baseline scoring a Windows census is the mix `scripts/jdk-only-census.sh`'s
  header exists to prevent. Only `25/linux` is committed; three of the four
  `jdk-only` CI legs report themselves ungated rather than green.
* **`JdkOnlyCensusLoadProbe` is the wrong workload for a gate, and neither
  number needs it.** Registration happens in `SharedVm::new` and the image
  adjudication parses class-path bytes without loading anything, so a one-line
  probe yields a byte-identical block — verified, not assumed. The broad probe
  hung in its `net` section on 1 of 3 runs on a loaded host, and a hung probe
  writes no census.
* **`kind_stated` is now false on all but 9 rows, and on *every* `Bridge` row.**
  L9's `java/lang/String` migration is those 9. All 10,842 `Bridge` rows still
  inherit their kind.

**Both halves are blocking**, which took a second pass to get right. The
hermetic self-test runs in `jdk-only-blockers-selftest` — 14 checks including an
injected unadjudicated `Bridge` it must reject and an adjudicated one it must
accept, so the gate is shown to fail and shown not to be always-red on every
run. The *measured* half runs in `build-and-test`'s ubuntu leg, beside
`Synthetic-stub ratchet`: wired only into the advisory `jdk-only` matrix it
printed an error and failed nothing, and neither of that job's two reasons to be
advisory applies to a `Compatible`-mode census with a committed baseline. The
`jdk-only` matrix keeps its copy as the multi-JDK/OS probe.

**Update, 2026-08-05 — L7 landed, and the lane doc is retired to
`L7-ensure-synthetic-class-migration-RETIRED-20260805.md`.** A
strict boot now fabricates **zero** compatibility classes (13 before), and the
two breadth probes drop 17 → 1 and 18 → 5. `Compatible`-mode stdout is
byte-identical on all three workloads against the pre-fix binary. Another
number joins the *"a number in a record is a claim"* list: **"52 call sites"
was 39 live in 25 files, of which 10 fire** — the rest of the gap was
`#[cfg(all(test, feature = "synthetic-jdk"))]` and `proxy_gen.rs`'s test module
being counted as production.

Three findings worth carrying into the other lanes:

* **The census could not name a native until 2026-08-05.** All seven
  native-minted classes were attributed to one forwarding line in
  `NativeContextImpl`; `#[track_caller]` now runs down through the trait
  declarations and the three allocation funnels. And `requested_by` records the
  *first* requester of a name — including one that was **refused** — so a later
  successful fabrication of the same name is attributed to the refusing site.
* **`ensure_synthetic_class` cannot be deleted by migrating call sites.** Three
  of the 39 *are* the infallible allocation funnels, with ~2,300 callers
  between them. That, not the call sites, is what step 3 is gated on.
* **A fabrication cannot be made fallible before its native is retagged**, when
  the class stands in for a real JDK method the JDK's own bootstrap calls.
  Measured: refusing `cratonvm/internal/Unmodifiable*` reaches a zero census
  and produces `NullPointerException: zone` from `java.time`, which names
  nothing. Reverted, and left as a hand-off to whoever owns
  `register_unmodifiable_natives`.

**Update, 2026-08-05 — L8 is retired, and criterion 6 is measured on every
build.** *(By the probe gate, which is not the whole of criterion 6 — the corpus
asks a different question and got a much worse answer. See the two-instrument
table under* Definition of done *.)* One new probe over the five surfaces nothing
covered (ProcessBuilder, security providers, virtual threads, agents/attach, JNI)
found four divergences on its first run, and two of those were binding failures
hiding three more underneath. Seven defects: five fixed, four filed. *(5 + 4 ≠ 7;
the arithmetic here is **UNVERIFIED** — it is not re-derivable from this
document, and the retired lane doc is where the list lives.)* **Zero were introduced by
`--jdk-only`** — every one was already wrong in `Compatible` mode and had simply
never been executed, which is this wave's central pattern confirmed again.

Two of the fixes have nothing `--jdk-only`-shaped about their blast radius:
**no JNI native on a nested class could bind** (`jni_encode` never escaped `$`,
so the VM looked for `Java_Outer$Inner_m` where the compiler emits
`Java_Outer_00024Inner_m`), and **`RegisterNatives` always returned
`JNI_ERR`** (it decoded `JClass` through `jobject_to_obj` while `FindClass`
returns a raw `ClassId` and the rest of the JNI table decodes it as one).
Between them, the `FindClass` + `RegisterNatives` idiom every `JNI_OnLoad` is
built on had never worked. Both were unreachable until a probe shipped an
actual `.so`.

The lane also closed its own open residual — `Net.poll` held the socket-map
read guard across the listener park, so a concurrent `Net.socket0` deadlocked
against it — and it did so from a **frame dump**, not from reading code:
`--stack-dump-on-timeout=N` inside an outer `timeout` turned "the run stops
after the nio line" into two named frames, six times out of six.

Three corrections to this document's own numbers:

* the three standing probes reach **674** dispatched slots, not 401, and the
  registry is **11,526** now that the `String` natives are gone;
* "take the censuses from the suites, not the probes" is wrong as stated. Three
  H2 classes reach 729 slots and the probes reach 674, but **305 are suite-only
  and 250 are probe-only** — neither is a superset. L6/L7/L12 want the union;
* item 2's floor warning gains a fifth instance, found behaviourally rather than
  by the hunter: `ProcessBuilder.redirectInput` wrote a `File` into raw slot 3,
  which on a real `java/lang/ProcessBuilder` is the `redirectErrorStream`
  **boolean**.

And two of the seven defects were in the **instruments**: the census probe
printed its ephemeral port (so it diverged from HotSpot on every run while
being documented as byte-identical), and the new probe used try-with-resources
on an executor — an unbounded `close()` — breaking the probe rules it was
written to. A gate whose first catch is its own instrument is working.

**Update, 2026-08-06 — L10 landed, and with it the last blocker.** Real-JDK mode
registers **zero** `java/util/concurrent/Executors` pool factories (8 → 0;
registry 11,876 → 11,868), so the real `Executors` bytecode constructs every
executor and CratonVM cannot mint one the real `<init>` did not build.
`probes/L10ThreadPoolInitProbe` — 62 lines, three guards the evidence record
demanded — is byte-identical to HotSpot 25 in **both** modes, and the new
`CRATONVM_DBG_TPE_SHAPE` reports the receiver-shape predicate `true` 62/62 and
38/38 with **zero** `false` across both workloads in both modes. L11's item 7 is
unblocked; the retired lane doc is
`L10-blocker-threadpool-init-DONE-20260806.md`.

**L11 item 7 spent that on 2026-08-06, the same day.** All eight receiver-shape
sites, the ninth receiver-blind `force_native_over_real_jdk_bytecode` arm and
the `threadpool_executor_has_real_workers` predicate are deleted;
`native_es_execute` is tagged `NativeKind::SyntheticStub` and
`java/util/concurrent/ThreadPoolExecutor` is on
`real_protected_stub_class_common`'s allow-list, which answers the same
question class-scoped on both dispatch paths. `CRATONVM_DBG_TPE_SHAPE` and
`probes/L10ShapeInstrumentControlProbe` went with the predicate they measured:
an instrument for a decision the VM no longer makes can only ever print
nothing, which is the same silence-is-not-zero trap the flag was designed
around. The readings themselves are kept in the L10 record. Outcome record:
`jdk-only-wave2-threadpoolexecutor-execute-receiver-shape-RETIRED-20260806.md`.

**That last reading is identical on the pre-L10 binary, and saying so is the
point.** The predicate already answered `true` for the receivers those workloads
produce. L10 changed its *domain*, not its answer: real-JDK mode no longer has a
code path that constructs an executor, so there is no input it can be false for.
A lane that had stopped at `false=0` would have satisfied step 3 by the wrong
route — concluding a property is universal because the sampled inputs satisfied
it — and the eight sites cannot be deleted on that evidence.

Five things worth carrying, and the first two are corrections to *this*
document:

* **A brief can over-report its lane as easily as under-report it.** The
  conflict matrix and the lane table both gave L10
  `native-collections/src/lib.rs` — *whole file*, 55k lines, "treat whole-file
  ownership as the unit". **The file was not touched.** The defect was one
  `matches!` arm in `native-api/src/registry.rs`, and the `ctl`/`mainLock`/
  `workers`/`workQueue` "family" the brief said to go find had been initialised
  since 2026-07-10. The measurement that would have caught this — run the probes
  against the pre-fix binary first — took ten minutes and was worth a lane's
  worth of planning. L1 and L3 found their briefs under-reported; this is the
  same lesson from the other side, and the general form is: **the brief is a
  hypothesis about where the code is, and it is the first thing to test.**
* **A green transcript is not the property this lane owed.** The census probe's
  `concurrent` section — named in the lane doc as its headline signal, and as
  "a genuine new signal" — was byte-identical to HotSpot **before** the change.
  What was actually broken was invisible to it: the construction path kept two
  fallbacks that write a two-slot shape onto a real-layout object and return it
  as if `<init>` had succeeded, which made the predicate the eight dispatch
  sites consult *conditionally* true. "Conditionally true" is what blocks
  deleting them, and no transcript can see the difference.
* **Registration beats dispatch, again.** Same shape as item 3's outcome: the
  fix is one arm of `NativeMethodRegistry::register`, invisible to every
  dispatch path at once, where a dispatch-side policy has to be restated per
  path and was already copied eight times here.
* **An absent instrument and a satisfied predicate produce the same silence.**
  The first A-arm reading was `true=0 false=0` on the pre-fix binary — which
  reads as "these sites are dead" and would have been a tidy, wrong finding. The
  binary predated the flag. A third build was needed for a real A/B. This is why
  the flag prints successes as well as failures, and the rule generalises:
  **check the instrument is in the binary before reading a zero out of it.**
* **A zero from an instrument is a statement about the workload, not about the
  code.** The real A/B, once it existed, showed the predicate answering `true`
  on every call *before* L10 as well as after. The lane is still necessary — the
  fallback that produces a false receiver was real, these two workloads just
  never took it — but "we measured `false=0`" was never the evidence it looked
  like. **A universal claim needs an argument about reachability; a counter only
  ever samples.** Pair every such reading with a negative control that makes the
  other branch fire — `probes/L10ShapeInstrumentControlProbe` did it with
  `Unsafe.allocateInstance` until both it and the flag were retired with the
  predicate on 2026-08-06 — or the zero is unfalsifiable.

**Update, 2026-08-06/07 — the strict corpus was run properly for the first time,
and it reported 14 failures.** A separate pool, lanes `SC-1`…`SC-16`, took one
lane per failure. On dev tip `84b85c624`: *37 passed, 14 failed — `RJdkStrict`
`RJdkLambdas` `RJdkHandles` `RJdkReflect` `RJdkHidden` `RJdkModule`
`RJdkForkJoin` `RJdkNio` `RJdkNet` `RJdkProcess` `RJdkSecurity` `RJdkJmx`
`RJdkJni` `RJdkFailure`.* **Fourteen vectors, fourteen failures, at most
thirteen distinct root causes** — `RJdkHidden` and `RJdkStrict` are two
separately scheduled vectors dying on one shadowed placeholder, and no other
pair has been checked for shared causation. **A failure count is an upper bound
on the defect count, never an estimate of it**; that collapse was noticed only
because two lanes' traces got compared, so diff a new trace against the filed
ones before staffing a lane. The campaign record, with the
per-lane table, the `SC-<n>` reconciliation and what still has to be measured, is
[`STRICT-CORPUS-CAMPAIGN-20260807.md`](STRICT-CORPUS-CAMPAIGN-20260807.md); the
evidence is one file per lane in
[`docs/known-issues/jdk-only/`](../../known-issues/jdk-only/).

**Nothing from that campaign has been built or re-measured.** No lane could run
`cargo` or the VM; every fix is source-level and unverified by execution, and
five lanes handed patches to files they did not own, which is the most likely
thing to be missing from a merged tree. Read every "FIXED" in those records as
*"fixed in source, unverified"* — most of them say so in their own headers. The
one exception is SC-10, which changed a **Java vector** and measured it 26/26.

Six things from it belong here rather than only in the campaign record:

* **The headline: of the 14 failures, 12 fail in `--real-jdk` too, and ZERO are
  strict-only.** `--jdk-only` is not rejecting legitimate things; it is
  *executing* paths `Compatible` mode was papering over with synthetic stubs, and
  the code underneath is wrong in both modes. The remaining work is ordinary
  subsystem bug-fixing that fixes BOTH modes, not `--jdk-only` contract work.
  This is L8's 2026-08-05 finding — "**Zero** were introduced by `--jdk-only`" —
  at corpus scale.
* **…and the corpus that established it runs ONE arm, which is a gap in the
  instrument, not a footnote.** `run.sh` schedules `JDKONLY_CLASSES` only when
  `CRATONVM_ARGS` names `--jdk-only` (`run.sh:83-86`), so these 21 vectors have
  **never been run in Compatible mode**. The corpus answers *"does strict break
  things?"* and **cannot answer *"does Compatible still work?"*** — SC-14 is the
  proof the second question has a non-empty answer set: `RJdkServices` fails in
  the default `--real-jdk` arm, passes under `--jdk-only`, and therefore never
  appears in the 14 at all. Since the end state renames today's `--jdk-only` to
  `--real-jdk`, **the arm this corpus cannot see is the one being deprecated
  *into***. Criterion 4 is not measured by it either. Run `JDKONLY_CLASSES`
  under `--real-jdk` and record how many more `RJdkServices`-shaped inversions
  fall out; that count is the blind spot's size and it is currently unknown.
  (`RJdkStrict` is the one deliberate exception — mode-divergent by design, and
  skipped in Compatible mode with a printed reason.)
* **The disease shape has a name now, and four lanes found it independently: a
  synthetic stub or placeholder shadowing real JDK bytecode that already works.**
  Registration is last-write-wins on the triple, so a placeholder registered
  *later* in the boot sequence silently outranks a correct implementation
  registered earlier — no exception, no warning, no census row that looks wrong.
  The sharpest instance is `Lookup.defineHiddenClass`: a phase-54 placeholder
  that ignored its arguments and never wrote slot 0 (`lookupClass`) shadowed the
  real WP2.3-B implementation, and **one deletion fixes two regression classes**
  (`RJdkHidden` and `RJdkStrict` — they are ONE root cause, not two; both die on
  `NullPointerException: Cannot invoke "java.lang.Class.isHidden()"`).
* **`RJdkModule` had been written off as a bad vector and is not one.** The
  correction is in *Failure modes* above, because the lesson is about method.
  It is a real CratonVM defect, in both modes, at `RJdkModule.java:57` — check 4
  of 44, so no `CK` line is emitted and nothing about JPMS is exercised.
  `--module-path` / `--add-modules` were **parsed and then read by nobody**
  (SC-9, `known-issues/jdk-only/L9-module-not-in-boot-layer.md`).
* **Two of the fourteen were only identified on 2026-08-07**, and both are
  shapes this document has not carried before. `RJdkForkJoin` is a genuine
  **hang** — rc=124 on a 300 s budget with zero output, one thread in the
  watchdog dump, main recursed inline through `compute -> invokeAll -> doExec ->
  exec` and parked at `ForkJoinTask.awaitDone(FJP,IZJ)I pc=218`, because a lazy
  `fork()` `Bridge` leaves the real `invokeAll` bytecode waiting on a worker that
  does not exist. **An rc=124 here is a hang, not a slow host — read the
  watchdog dump, not the wall clock.** `RJdkFailure` is the opposite of a missing
  feature: a wrong error **shape**, a `NoClassDefFoundError` escaping a
  `catch (ClassNotFoundException)` in a negative test, because an `Error` is not
  an `Exception`.
* **Every pre-2026-08-06 `RJdkProcess` result is void.** The vector asserted
  three separate live-process-table snapshots against a child (`cmd.exe /c exit
  3`) chosen *because* it exits immediately — three coin flips, which HotSpot 25
  also loses. A failure at `RJdkProcess.java:132`/`:134` carries zero information
  about CratonVM. The rewritten vector points the tree section at a live child
  and the exit-code section at the exiting one, **adding 7 assertions and
  removing none** (46 → 53 checks), and measured 26/26 green including 18 runs
  under load.
