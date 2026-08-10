# JDK-only mode — open wave-2 work list

**Status:** OPEN, reduced 2026-08-04, 2026-08-06, and **measured by execution
2026-08-07**. Filed 2026-07-31 from wave-1 implementation findings; re-verified
against the re-landed tree the same day. **Two of the five tier-1 rows (items 5
and 7) closed 2026-08-06** — see the pass note below the table.

> ## 2026-08-07 — the strict corpus was built, run, and closed: 53 passed, 1 failed
>
> Most records in this directory say "FIXED in source ... not yet verified,
> no binary was built in this session". A binary was built (dev `5d22671e3`,
> default features) and `CRATONVM_ARGS="--jdk-only" bash regression-suite/run.sh`
> was run three times, with the identical result each time. The campaign's 14
> failures are down to **3**:
>
> | vector | was | now |
> |---|---|---|
> | `RJdkModule` | strict-only, `module service providers: []` | **FIXED** — module ServiceLoader providers now resolve; 44 checks, matching HotSpot |
> | `RJdkFieldModule` | both modes, JLS 6.6.2 over-refusal | **FIXED** — `Method.invoke` no longer over-refuses a protected receiver |
> | `RMapGcStress` | fails in every mode and every build | still open; dev's own, not this campaign |
>
> Corpus after the two fixes: **53 passed, 1 failed**, ABBA-interleaved.
> Compatible mode unchanged at 30/1. A `--features synthetic-jdk` binary run
> `--jdk-only` is still worse (48/6) — a separate, previously unmeasured
> configuration.
>
> **What this does and does not license.** Every record whose corpus vector is
> in `JDKONLY_CLASSES` and is not one of those three now has its vector passing,
> so its "unverified by execution" caveat is discharged **at the vector level**.
> It is not a per-assertion audit: a record claiming something narrower than its
> vector asserts still owns that claim. Do not mass-flip statuses to FIXED on
> the strength of this line — check what your record actually claims.
>
> **If you re-measure, pass `--java-home`.** `run.sh` gives every CratonVM
> invocation one; a hand-run that omits it measures the host's default JDK
> instead of the JDK 25 image, which on this host inverted the per-mode verdict
> for `RJdkModule`. Trace the real command rather than reconstructing it:
> `ONLY="RJdkModule" bash -x regression-suite/run.sh 2>&1 | grep <binary>`.
>
> Campaign-level record:
> the retired `STRICT-CORPUS-CAMPAIGN` record.

> ## Looking for the plan? It is not here.
>
> **This directory is the evidence base** — what is broken, how it was measured,
> what the blast radius is. One record per defect; a record moves to
> the internal record tree when it is fixed.
>
> **The parallel execution plan is
> the retired jdk-only wave-2 execution plan** —
> twelve lanes with an explicit file-ownership map, a conflict matrix, and a
> verification protocol. Nine of the twelve can start simultaneously. Read that
> to decide *what to work on*; read this to understand *what you are fixing*.
>
> Normative contract: [`docs/feature-designs/jdk-only-mode.md`](../../feature-designs/jdk-only-mode.md).
>
> **Read the mechanism facts first:**
> [`docs/architecture/natives-over-real-jdk-classes.md`](../../architecture/natives-over-real-jdk-classes.md).
> Eight facts every lane in this campaign rediscovered at cost — how a native
> actually comes to run instead of real JDK bytecode (**not** the "four doors"
> rule several records below still state), why a Cargo feature is not a runtime
> mode, `register()`'s last-registration-wins semantics, what a by-name field
> read cannot report, why a slot index against a real layout is heap corruption
> rather than a wrong answer, what the registration censuses are scoped to, and
> the measurement rules. Its §9 lists the records in this directory it corrected
> and the claims it could not correct from `docs/`.

---

## 2026-08-04 pass — what closed, what moved, what did not

Two records left this directory, one item was retracted, and every remaining
record carries a **What changed on 2026-08-04** section stating what was done
and what it did *not* do. Read that section before working an item; the body
below it is the original filing.

**Closed and moved to the internal record tree:**

| Was | Now |
|---|---|
| 9 — the observability surface | `jdk-only-observability-surface-FIXED-20260804.md` |
| 10 — `System.exit` bypasses the census | `jdk-only-system-exit-census-FIXED-20260804.md` |
| 8 — the real-protected-stub allow-lists | `jdk-only-real-protected-stub-allowlists-FIXED-20260804.md` |

## 2026-08-06 — two records left this directory

| Was | Now |
|---|---|
| `strict-boot-refuses-five-classes-the-corpus-needs-20260805.md` | `jdk-only-strict-boot-refused-five-classes-FIXED-20260806.md` |
| `step1-bytecode-available-attempted-and-reverted.md` | `jdk-only-step1-bytecode-available-RESOLVED-20260806.md` |

The first was marked CLOSED on 2026-08-05 with four of its five classes fixed
and a header that said five; its own verification section said otherwise two
paragraphs later. The fifth, `cratonvm/internal/SystemLogger`, is reached from
`java.io.ObjectInputFilter$Config.<clinit>` — unconditionally, before any
property is read — so refusing it cost every `ObjectInputStream` construction in
the VM, i.e. all of deserialization, not one probe section. The refusal now
lands on a real `jdk.internal.logger.SimpleConsoleLogger` built through its own
constructor. `scripts/jdk-only-strict-probes.sh` PASSES with **no divergent
strict-arm section left** in either of the two probes that carried them, the
baseline is re-frozen, and the corpus is 33/16 under `--jdk-only` against 32/17.

The second is the more useful of the two to read before working item 4. Its own
proposal — resolve `bytecode_available` at step 1 the way the invoke will — was
implemented and **measured**: arming it takes the `--jdk-only` corpus from
**32 passed / 17 failed to 3 / 46**, because `Charset.forName` hands out an
instance of the ABSTRACT `java.nio.charset.Charset`, `System.props` is null,
`SharedSecrets.javaLangAccess` is null, and `String`'s coder does not match its
`value[]`. §1.4's lever is registration, not dispatch. What landed is the
observation (step 1 now records every `Bridge` that runs in front of real bytes;
it recorded nothing before) plus a dial,
`CRATONVM_ENFORCE_NATIVE_SHADOW=1`, so item 4 can re-take that measurement one
subsystem at a time.

**Found on the way, fixed, not filed here:** `vm/src/jit/helpers.rs`'s third
`note_site_identity` call site had the `CRATONVM_DBG_SITE_ALIAS` comment but not
the `if`, so an ordinary run printed `[site-alias]` lines to stderr and grew an
unbounded thread-local map on every raw-entry native dispatch. Pre-existing on
`dev`; the strict gate saw it in BOTH CratonVM arms of `JdkOnlyPlatformProbe`,
which is the gate's third arm doing exactly what it is for.

## 2026-08-05 — three records added

[`l5-native-io-bridge-residuals.md`](l5-native-io-bridge-residuals.md) — the 117
`native-io` `Bridge` registrations that L5's `register_with_kind` migration
declined to claim, because the JDK 25 image declares no `ACC_NATIVE` target for
them. Reclassification questions, not migration ones. The largest is 25 `Bridge`
registrations on VM-minted `cratonvm/synthetic/Process*` classes — the
`Function$Identity` shape (a surviving `Bridge` whose receiver class §5 forbids)
found in a second place.

[`l5bc-awt-builtins-bridge-residuals.md`](l5bc-awt-builtins-bridge-residuals.md)
— the same question at crate scale, after L5b/L5c repeated the migration for
`native-awt` and `native-builtins`: 7,748 `Bridge` registrations across those two
crates that the image does not back. It also records three things the migration
measured rather than assumed — that the tree's only `bridge` marker outside
`native-io` names a method a Linux JDK 25 image does not declare, that
`native-collections` has **zero** rows a migration could ever state, and that
`ABSENT` on a platform-named class means "not measured here", not "dead".

The census asks one class, on one platform — **RETIRED 2026-08-10**, its five
open items closed under
`fixed-bugs/jdk-only-census-one-class-one-platform-FIXED-20260810.md`
— and then the two instruments that answer both of those. The census asks about
one class in one image, so **1,939 of the 2,542 "method not declared" rows are
actually inherited** (19 of them from an `ACC_NATIVE` supertype) and **59
registrations are a genuine bridge only on Windows**, provable from the Linux
host because CratonVM adjudicates an image it cannot run. Corrects three
verdicts in the two records above.

**Found and fixed while working this list, not filed here before:**
`--jdk-only` could not start a thread (`jdk-only-section7-step3-unsatisfiedlinkerror-FIXED-20260804.md`).
§7 step 3's decline fell through to `UnsatisfiedLinkError` rather than to the
bytecode, so every `new Thread(…)` died, every `ExecutorService` had no live
workers, a workload that joined on one **hung**, and `FileChannel.size()`
silently returned `0`. Pre-existing on `dev`. It was surfaced by the schema-3
census on that instrument's first run — the three `java/lang/Thread` rows say
`start0` is `ACC_NATIVE` and `run`/`start` are shadows of concrete bytecode —
and the other 4,795 shadowing `Bridge` registrations can reach the same path.

**The finding that came out of closing item 9, and that changes how the rest of
this list should be worked.** `requested_by` now names the *Rust* call site that
asked for each fabrication, not only the Java frame — because on a strict boot
essentially every compatibility class comes from a native asking for an
allocation shape, and has no Java frame at all. Item 4 scopes its migration at
"52 live call sites in 27 files". **Three of them fire on a strict boot.** Take
the census from the workload you care about before sizing any of this from a
grep.

**Substantially reduced, still open:**

* **3** — the statically-unreachable h2-bnf block is alive (a landed, measured
  fix that had never once executed); both halves of the policy are named
  functions side by side; a 29-shape table pins the `(cold, warm)` verdict pair.
  Deletion still needs RKC16N.6 fixed.
* **8** — **CLOSED**, see above.
* **7** — the marker undercount that made this tier-1 is gone: a census constant
  names all eight sites plus the ninth, the probe has one implementation instead
  of three, and a gate fails on a partial sweep. No site is deleted; that still
  needs real `ThreadPoolExecutor` field initialisation first.
* **5** — `AnonymousObject$N` migrated to `VmInternal` and verified
  (`compatibility-stub` 14 → 13, `vm-internal` 0 → 1, total unchanged).
  `Proxy$Instance`'s origin question is answered — `VmInternal`, not
  `GeneratedProxy`, and the in-code marker is corrected — but flipping it is not
  attempted.
* **1** — **the census this item was blocked on has been taken.** Schema 3 adds
  `image_declaring_method`, adjudicating every registration against the bytes on
  the class path rather than against whatever the run happened to load, and
  `scripts/jdk-only-adjudicate.py` reads it. Measured on JDK 25: **11,909
  registrations**, `kind_stated` false on **all** of them (`register_with_kind`
  has zero callers), and **10,084 of 10,844 `Bridge` rows have no `ACC_NATIVE`
  target** — 4,796 shadow concrete bytecode, 1,321 are abstract, 2,489 name a
  method the class does not declare. The record's "1,195 `native-collections`
  registrations" framing is off by an order of magnitude and by scope: this is a
  whole-tree problem, and `native-collections` is 1,338 of it. Nothing is
  reclassified; contract §8 makes that its own wave, and it can now be cut into
  subsystem batches from data.
* **2** — **step 1 stopped being a hand sweep.** The runtime detector for this
  exact defect already existed (`CRATONVM_DBG=overlay,overlay-all`) and had
  never been run broadly. It produced a work list of **13 classes / 24 slots**,
  each with class, slot, value kind, real descriptor and frequency — and
  **identical under `--real-jdk` and `--jdk-only`**, so this is a
  `Compatible`-mode defect too, not a strict-mode one. Two entries are now
  fixed: `VarHandle` slots 0/1 (the VM was handing real `AtomicBoolean` /
  `AtomicReference` `<clinit>`s a `VarHandle` whose `vform` it had nulled) and
  `Properties` slots 5/6/7 (`try_set_jdk_map_field` resolved every field name
  against a hard-coded `java/util/HashMap` and wrote that index into whatever
  receiver it was given). **19 slots remain**, listed in the record.
  `CRATONVM_DBG=overlay-bt` was added to name the *Rust* writer, because the
  Java frames mislead. The two `unknown` overlay verdicts also drop from
  ranked-HIGH on evidence (two of three checks run, both clean).
* **11 §5** — **retracted**: its premise (the memo needs policy-qualifying) does
  not hold. Checking it found a larger defect in its place — seven force-native
  dispatch sites bypassing `resolve_dispatch` and the census entirely — which is
  fixed. **11 §13** — closed, at 14 occurrences rather than the 5 filed.

**Unchanged and open:** 4 (the migration itself), 11 §1, §2, §4, §6, §8, §9,
§10, §11, and §3's residual.

**One thing this pass established that no record says:** strict mode had never
been run against ordinary Java. The first breadth workload pointed at it —
`probes/JdkOnlyCensusLoadProbe.java`, nine sections of collections, streams,
`Properties`, io, nio, net, executors — found that `--jdk-only` could not start
a thread, and had been unable to for as long as anyone can date. Criterion 6
("strict corpus green") is not a formality to tick after the list is done; it is
where the defects are. Run the probe under both modes with a HotSpot control
before trusting any strict-mode claim in this directory.

Four new guards landed, each **verified by injecting a violation and watching it
fail**, then reverted: the site census, the no-hand-inlined-probe scan, the
allow-list divergence test, and the String-policy verdict table.

---

Normative contract: [`docs/feature-designs/jdk-only-mode.md`](../../feature-designs/jdk-only-mode.md)
(owned by the orchestrator; do not edit). Wave 1 is **measurement, not
deletion** (contract §10). Everything in this directory is a gap wave 1
deliberately deferred rather than papered over, with the evidence that makes it
actionable.

Related non-known-issue docs:
[`docs/jdk-only-runtime-services.md`](../../jdk-only-runtime-services.md),
[`docs/jdk-only-native-review.md`](../../jdk-only-native-review.md),
[`docs/jdk-only-migration.md`](../../jdk-only-migration.md).

The registration census behind items 1 and 2, and the object-layout survey they
rest on, were one-off audits; they have been retired and their durable findings
are stated in [`docs/README.md`](../../README.md) and
[`docs/architecture/natives-over-real-jdk-classes.md`](../../architecture/natives-over-real-jdk-classes.md).

---

## Citation status — read this before trusting a `file:line`

These records were first written on 2026-07-31 against a working tree whose
uncommitted wave-1 edits were then destroyed by an external `git restore`. The
work was **re-landed** into this worktree (`C:\craton\wt-jdk-only`, branch
`feat/jdk-only-mode`) by fresh agents. The re-land is equivalent in design and
different in detail: line numbers moved, several helpers were renamed, and a few
constructs were replaced by differently-shaped ones.

Every `file:line` in this directory has since been re-verified against the
re-landed tree and corrected. What that pass established:

* **Anchor on the marker tag, not the number.** Wave-2 sites carry
  `// JDK-ONLY-WAVE2:`, deferred observations carry `// JDK-ONLY-NOTE:`,
  per-registrar category verdicts carry `// JDK-ONLY-CLASSIFY:`, and
  field-slot verdicts carry `// JDK-ONLY-LAYOUT:`. Those tags are stable; the
  numbers are not.
* **The dispatch resolver re-land left exactly 14 `JDK-ONLY-WAVE2` markers**
  across `vm/src/vm/vm_exec.rs` (6) and `vm/src/runtime/interpreter/invoke.rs`
  (8). They are enumerated in the records that own them. Two of the fourteen
  are a **cross-linked pair** for the `java/lang/String` policy that both say,
  in terms, that they must be deleted together; two more are the
  real-protected-stub allow-list copies, both now annotated *"RECONCILE, not
  assume"*.
* **Several wave-1 gaps closed during the re-land.** The most significant:
  `real_declaring_method` is now populated rather than `null`; the two
  divergent schema-2 census writers were unified into one; the JIT's inline
  caches now refuse to publish native entries under `JdkOnly`; the JIT's
  by-name native fast paths are policy-checked and counted. Each affected
  record says so where it applies, and the ranking below reflects the move.
* **`vm-cli/src/main.rs` was being edited while this pass ran.** Its citations
  are given by function and marker name only.
* Claims that could not be re-verified are marked **UNVERIFIED** inline rather
  than deleted or asserted.

---

## Ranked work list

Ranked by *danger*, not by effort. The first tier causes **silent wrong
behaviour** — no exception, no log line, no failing test.

### Tier 1 — silent misbehaviour

| # | Record | Why it is dangerous |
|---|---|---|
| 1 | `NativeKind` is ambient and defaults to `SyntheticStub` — **RETIRED 2026-08-06** | `current_category` is an `Option` now and is scoped by the save/restore the tree already used, so it restores the *absence* of a choice; nine registrars that had no scope state their kind; no registration in a real boot runs on the default; and `scripts/jdk-only-kind-map.py` freezes the kind of every registration, which is what the aggregate ratchet could never do (a `Bridge`→`SyntheticStub` mass re-tag makes its numbers FALL). It also closed 58 triples `--jdk-only` was admitting by registration order — including the `Function$Identity` copies L7 missed. **The reclassification it pointed at is not closed**: 9,571 unadjudicated `Bridge` registrations, now in [`l5bc-awt-builtins-bridge-residuals.md`](l5bc-awt-builtins-bridge-residuals.md). |
| 2 | [Fabricated object layouts leak into native code](fabricated-object-layouts-leak-into-native-code.md) | Index-based field access against assumed synthetic layouts. On real bytes the index still resolves and points at a different field. `StringJoiner.add()` silently no-ops; `EnumSet.of()` returns an object with a null iterator. Two `breaks-under-strict` and two `unknown` sites are marked; three whole crates were never swept. |
| 4 | `ensure_synthetic_class` cannot enforce policy, only record it — **RETIRED 2026-08-10** | Returned a bare `ClassId`, so under `--jdk-only` it recorded the violation and fabricated anyway. The entry point is deleted, along with the `NativeSystemAccess` trait method, the `NativeContextImpl` override and all three infallible allocation funnels; the grep gate matches zero sites, tests included. See `fixed-bugs/jdk-only-ensure-synthetic-class-deleted-FIXED-20260810.md`. |

Items 5 and 7 left this table on 2026-08-06, together with wave-2 lanes L10 and
L11 item 7:

| Was | Now |
|---|---|
| 5 — VM-internal classes mislabelled `CompatibilityStub` | `jdk-only-wave2-vm-internal-classes-mislabelled-RETIRED-20260806.md` |
| 7 — the `ThreadPoolExecutor.execute` receiver-shape copies | `jdk-only-wave2-threadpoolexecutor-execute-receiver-shape-RETIRED-20260806.md` |

Item 7 came down to the per-**instance** question the eight receiver-shape
probes existed to answer having no receivers left. L10 landed that at
registration the same day: real-JDK mode registers no `Executors` pool factory,
so the real `Executors` bytecode builds every executor and a fabricated receiver
CANNOT be minted — a statement about the code, not about a `false=0` reading.
`native_es_execute` is then tagged `SyntheticStub` and `ThreadPoolExecutor` is
on the real-protected-stub allow-list, which answers the question class-scoped
on both dispatch paths; all nine sites and the probe helper are deleted, and a
gate fails if one grows back. The native itself is NOT deleted — strict mode
declines to admit it, and the `--features synthetic-jdk` build still runs it.

Item 5 needed the prerequisite the record named: `is_synthetic_stub` was
answering two different questions, so `Class::dispatch_lacks_class_file` now
answers the dispatch one and `origin` answers the census one.
`java/lang/reflect/Proxy$Instance` is `VmInternal` (census: 420 rows before and
after, `compatibility-stub` 14 → 13, exactly one class moved), the fabricated
`$$Lambda`/`$ProxyN`/`Generated*Accessor*` families are classified where they
are minted, and **`Class::is_synthetic_stub` is deleted** — contract §5's "in a
later wave", done.

Item 8 left this table on 2026-08-04:
the real-protected-stub allow-lists (`jdk-only-real-protected-stub-allowlists-FIXED-20260804.md`)
are one predicate now.

Item 3 left it the same day:
the forced-native `String` policy (`forced-native-string-policy-two-lists-that-disagree-FIXED-20260804.md`)
is gone — all four copies of it, the fourth having gone uncounted by this
record. The lists were removed after being MEASURED inert (a binary without
them produced a byte-identical 392-case `String` transcript in both modes and
identical invocation counts on every exercised slot), and the policy now lives
at registration: `NativeMethodRegistry::register` drops every `java/lang/String`
`Bridge` in real-JDK mode. It closes **L9** as well — RKC16N.6 does not
reproduce, because the class-load defect behind the April symptom was fixed by
RKC16N.9 and nobody went back to the workaround.

Retired item 6: cached invoke targets retain and revalidate `NativeKind`
was fixed on 2026-08-01. The interpreter invoke cache now carries the id and
kind, re-applies central policy, and counts both static and virtual warm hits.
The JIT MIC/PIC-slot half remains independently tracked by item 11 §1.

**Every row in the tier-1 table above predates the 2026-08-04 pass.** The "why
it is dangerous" column still describes the defect each record was filed for
accurately; what changed is how much of each is left, and in one case (item 8's
"two copies") the shape. See the summary at the top of this file, and the
*What changed on 2026-08-04* section in each record.

### Tier 2 — the instruments the tier-1 items must be measured with

**Both closed 2026-08-04**, and moved to the internal record tree:
the observability surface (`jdk-only-observability-surface-FIXED-20260804.md`)
and the `System.exit` census (`jdk-only-system-exit-census-FIXED-20260804.md`).

Their outputs are what the tier-1 items should now be worked from. In
particular: `requested_by` names the Rust call site of every fabrication, the
schema-2 census carries `kind_stated`, `--trace-jdk-only` reports class-origin
violations live, and a run that ends in `System.exit` leaves a census behind.

### Cross-cutting

| # | Record | Contents |
|---|---|---|
| 11 | [Additional wave-2 markers not in the original inventory](additional-wave2-markers-not-in-the-original-inventory.md) | 13 further findings, re-verified and re-scored against the re-land. Four of them moved materially: the JIT's inline caches are now closed-by-refusal under `JdkOnly` rather than unchecked; the JIT's by-name native fast paths are policy-checked and counted; `build_helpers` now publishes the policy before the first compile; and the three documentation-gap items are all closed. Still open: the process-global JIT policy and `JNI_NATIVE_METHODS`, the seven thin direct-call ladders (two of them in the `String` family), the interface-substitution map, the `redefine_immune_*` predicates, `check_override`'s 217-disjunct / ~2,650-line chain, and three stale doc paths in load-bearing comments. |

---

## Dependency order for wave 2

The items are not independent. Steps 1 and 2 of the original order — finish the
instruments, make the census survive `System.exit` — are **done**; what follows
is the order for what remains.

The single most useful thing to do before starting any of it: **take the
schema-3 census and the class-origin census from a real-JDK run of the workload
you actually care about**, and read them with `scripts/jdk-only-adjudicate.py`.
Every item below is evidence-driven, and the instruments now produce that
evidence: `requested_by` naming Rust call sites, `kind_stated` separating chosen
from inherited kinds, `image_declaring_method` adjudicating every registration
against the class-path bytes whether or not the run touched the class, and a
live violation trace.

**The grep-derived sizes in these records are systematically wrong, and always
in the same direction.** Three measurements now say so: 52 grep-visible
`ensure_synthetic_class` call sites of which **3** fire on a strict boot; "about
8,000" registrations against a measured **11,909**; and a `native-collections`
mis-tagging scoped at 1,195 registrations that is really **10,084** spread over
the whole tree. Do not size anything here from a `rg` count.

1. **Item 1** — make every native's kind an explicit, per-registration fact.
   `register_with_kind` and the `kind_stated` census column exist now; the
   migration and the reclassification do not, and contract §8 scopes them as
   their own subsystem-per-PR wave. Nothing else in tier 1 can be done safely
   before this: items 3, 7, 8 and item 11 §4/§11 all end with "let
   `resolve_dispatch` decide from `NativeKind` + `Method::code()`", which
   requires the kinds to be true.
2. **Item 11 §1** — the JIT's MIC/PIC slots still store a raw entry pointer with
   no kind beside it, and pay for the gap with a blanket refusal that costs
   `JdkOnly` runs every inline-cached native call. Independent of item 1 in
   principle, but the fix is the same shape and worth doing next to it.
3. **The three blockers, each of which is real engineering rather than
   cleanup.** They gate items 3, 8 and 7 respectively, and none of them is a
   JDK-only change:
   * **RKC16N.6** — real-JDK `java/lang/String` bytecode resolution during JDK
     `<clinit>`s. Until this is fixed, both `String` lists have to stay.
   * ~~**The `StringJoiner` heap-reference-integrity defect**~~ — retired
     2026-08-04. It did not reproduce under the exact merge that was supposed to
     trigger it, and the merge is landed.
   * ~~**Real `ThreadPoolExecutor` field initialisation**~~ — retired
     2026-08-06 by L10, at REGISTRATION: real-JDK mode registers no
     `java/util/concurrent/Executors` pool factory at all, so the real
     `Executors` bytecode constructs every executor and CratonVM has no code
     path that can mint a half-built one. Item 7 spent that the same day —
     `native_es_execute` is reclassified and all nine sites are gone.
4. **Item 11 §4/§8/§9/§11** — delete the hard-coded lists, each with its own
   regression corpus. Items 3, 7 and 8 are done (2026-08-04 / 2026-08-06).
5. **Item 4** — migrate the remaining `ensure_synthetic_class` callers. Drive
   the migration from the `requested_by` census, not from a grep. (Item 5 is
   done: `Proxy$Instance` is settled and `is_synthetic_stub` is deleted.)
6. **Item 2** — finish the layout sweep across `native-builtins`,
   `native-collections`, `native-io` and `vm/src/native/`. Independent of the
   rest and can run in parallel, but it is the item most likely to surface new
   blockers.

## Standing constraints for anyone working this list

* `native-builtins/tests/stub_ratchet.rs` asserts `BASELINE_SYNTHETIC_STUBS`
  **exactly**, with `SLACK = 0` (555 as of 2026-08-06; the constant carries its
  own re-freeze history — the figure moves, so read it there rather than here),
  and separately asserts only
  `total >= 8_000` as a vacuity floor. The floor is not a claim about the exact
  total — do not cite one. The strict-mode siblings in the same file assert
  zero `SyntheticStub` registrations and `strict_total >= 7_500`; that second
  number is a collapse detector, not a measurement, for the same reason.
* `Compatible` mode must remain byte-for-byte unchanged (contract §5, §10). Most
  of the dangerous mistakes catalogued here are `Compatible`-mode behaviour
  changes made while intending to fix strict mode.
* No process globals for this feature's state (contract §2). Two of the items in
  this directory are existing violations; do not add a third.
* **JMX and `java.util.function.Function$Identity` are already retagged
  `Bridge`** and are *not* among the residual 157. Any plan that starts from
  "retag JMX" is working from a stale report. `Function$Identity` has a
  *successor* defect instead — see item 1.
* `docs/known-issues/` holds **unfixed** issues only. A record moves to
  `` when it is fixed, not when it is planned.
