# JDK-only mode — open wave-2 work list

**Status:** OPEN, reduced 2026-08-04. Filed 2026-07-31 from wave-1
implementation findings; re-verified against the re-landed tree the same day.

---

## 2026-08-04 pass — what closed, what moved, what did not

Two records left this directory, one item was retracted, and every remaining
record carries a **What changed on 2026-08-04** section stating what was done
and what it did *not* do. Read that section before working an item; the body
below it is the original filing.

**Closed and moved to `docs/internal/`:**

| Was | Now |
|---|---|
| 9 — the observability surface | [`jdk-only-observability-surface-FIXED-20260804.md`](../../internal/jdk-only-observability-surface-FIXED-20260804.md) |
| 10 — `System.exit` bypasses the census | [`jdk-only-system-exit-census-FIXED-20260804.md`](../../internal/jdk-only-system-exit-census-FIXED-20260804.md) |

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
* **8** — one list plus one *stated* exception, with the divergence asserted by
  a test instead of described in two comments. **The `StringJoiner` defect did
  not reproduce**: with the class protected on both paths — the exact merge this
  record says reintroduces it — 40,000 `add()` calls under `-Xmx64m` came back
  byte-identical to HotSpot in both modes. That is a microprobe and the defect
  was found in a suite, so the asymmetry stays until H2 and Hibernate confirm
  it; the code is unchanged.
* **7** — the marker undercount that made this tier-1 is gone: a census constant
  names all eight sites plus the ninth, the probe has one implementation instead
  of three, and a gate fails on a partial sweep. No site is deleted; that still
  needs real `ThreadPoolExecutor` field initialisation first.
* **5** — `AnonymousObject$N` migrated to `VmInternal` and verified
  (`compatibility-stub` 14 → 13, `vm-internal` 0 → 1, total unchanged).
  `Proxy$Instance`'s origin question is answered — `VmInternal`, not
  `GeneratedProxy`, and the in-code marker is corrected — but flipping it is not
  attempted.
* **1** — `register_with_kind` exists and the census carries `kind_stated`, so
  "chosen" and "inherited" are finally distinguishable per row. Nothing is
  reclassified; contract §8 makes that its own wave.
* **2** — the two `unknown` overlay verdicts drop from ranked-HIGH on evidence
  (two of three checks run, both clean); the third is instrumented. The
  cross-crate sweep is untouched and is the bulk of the item.
* **11 §5** — **retracted**: its premise (the memo needs policy-qualifying) does
  not hold. Checking it found a larger defect in its place — seven force-native
  dispatch sites bypassing `resolve_dispatch` and the census entirely — which is
  fixed. **11 §13** — closed, at 14 occurrences rather than the 5 filed.

**Unchanged and open:** 4 (the migration itself), 11 §1, §2, §4, §6, §8, §9,
§10, §11, and §3's residual.

Four new guards landed, each **verified by injecting a violation and watching it
fail**, then reverted: the site census, the no-hand-inlined-probe scan, the
allow-list divergence test, and the String-policy verdict table.

---

Normative contract: [`docs/feature-designs/jdk-only-mode.md`](../../feature-designs/jdk-only-mode.md)
(owned by the orchestrator; do not edit). Wave 1 is **measurement, not
deletion** (contract §10). Everything in this directory is a gap wave 1
deliberately deferred rather than papered over, with the evidence that makes it
actionable.

Related non-known-issue docs: [`docs/jdk-only-runtime-services.md`](../../jdk-only-runtime-services.md),
[`docs/jdk-only-audit.md`](../../jdk-only-audit.md),
[`docs/jdk-only-native-review.md`](../../jdk-only-native-review.md),
[`docs/jdk-only-migration.md`](../../jdk-only-migration.md),
[`docs/jdk-only-ambient-category-audit.md`](../../jdk-only-ambient-category-audit.md),
[`docs/jdk-only-object-layout-audit.md`](../../jdk-only-object-layout-audit.md).
The last two did not exist when these records were first written and are the
evidence base for items 1 and 2.

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
| 1 | [`NativeKind` is ambient and defaults to `SyntheticStub`](native-kind-is-ambient-and-defaults-to-syntheticstub.md) | `register()` takes no kind; it is inherited from a mutable registry field defaulting to `SyntheticStub`. The dominant defect is the **opposite** of under-tagging: 1,195 `native-collections` registrations are tagged `Bridge` by a single `set_category` line, and **not one** of them targets an `ACC_NATIVE` method. Under-tagging is real too and already caused one boot regression (2026-07-14, `java.util.Properties`). Both directions are silent at the point of the mistake. |
| 2 | [Fabricated object layouts leak into native code](fabricated-object-layouts-leak-into-native-code.md) | Index-based field access against assumed synthetic layouts. On real bytes the index still resolves and points at a different field. `StringJoiner.add()` silently no-ops; `EnumSet.of()` returns an object with a null iterator. Two `breaks-under-strict` and two `unknown` sites are marked; three whole crates were never swept. |
| 3 | [The forced-native `String` policy exists in three places, in disagreeing forms](forced-native-string-policy-two-lists-that-disagree.md) | A 21-name positive list (cold path) versus a 7-pair exclusion (warm path), plus a JIT direct-call ladder. The disagreement has already made a landed, measured h2-bnf performance fix into **statically unreachable code**. |
| 4 | [`ensure_synthetic_class` cannot enforce policy, only record it](ensure-synthetic-class-cannot-enforce-only-record.md) | Returns a bare `ClassId`, so under `--jdk-only` it records the violation and fabricates anyway, across 52 live non-test call sites in 27 files. The fallible siblings now exist but have **zero callers**, so nothing changed operationally. Strict boot *silently loses* `Enumeration$Impl` / `Comparator$Native` instead of failing. |
| 5 | [VM-internal classes are mislabelled `CompatibilityStub`](vm-internal-classes-mislabelled-compatibility-stub.md) | `AnonymousObject$N` and `Proxy$Instance` are stamped `CompatibilityStub` to avoid flipping the derived `is_synthetic_stub` bool that 181 read sites across 20 files depend on. Correct deferral — but it makes contract §11's zero-stub criterion unachievable by construction, and two of those read sites gate native-vs-bytecode dispatch. |
| 7 | [The `ThreadPoolExecutor.execute` receiver-shape case is copied eight times](threadpoolexecutor-execute-receiver-shape-special-case-copies.md) | Wave 1's markers name four. There are **eight** dispatch sites in the `vm` crate plus one unconditional `force_native` arm they all exist to override. A mechanical "delete every marked site" sweep leaves half the duplication enforcing a policy the other half no longer applies. The marker undercount is unchanged by the re-land. |
| 8 | [The real-protected-stub allow-lists diverge](real-protected-stub-allowlists-diverge.md) | Two copies, 11 classes vs 10: one includes `java/util/StringJoiner`, the other deliberately omits it with a documented heap-corruption reason. Wave 2 must **reconcile**, not merge; both naive directions reintroduce a known defect. *Demoted from 7 to 8:* the re-land added the missing cross-reference to the including copy, so the "a reader who finds one has no way to know the other exists" trap is retired. The divergence itself is untouched. |

Retired item 6: [cached invoke targets retain and revalidate `NativeKind`](../../internal/cached-invoke-targets-drop-the-nativekind-FIXED-20260801.md)
was fixed on 2026-08-01. The interpreter invoke cache now carries the id and
kind, re-applies central policy, and counts both static and virtual warm hits.
The JIT MIC/PIC-slot half remains independently tracked by item 11 §1.

**Every row in the tier-1 table above predates the 2026-08-04 pass.** The "why
it is dangerous" column still describes the defect each record was filed for
accurately; what changed is how much of each is left, and in one case (item 8's
"two copies") the shape. See the summary at the top of this file, and the
*What changed on 2026-08-04* section in each record.

### Tier 2 — the instruments the tier-1 items must be measured with

**Both closed 2026-08-04**, and moved to `docs/internal/`:
[the observability surface](../../internal/jdk-only-observability-surface-FIXED-20260804.md)
and [the `System.exit` census](../../internal/jdk-only-system-exit-census-FIXED-20260804.md).

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
schema-2 census and the class-origin census from a real-JDK run of the workload
you actually care about.** Every item below is evidence-driven, the instruments
now produce that evidence (`requested_by` naming Rust call sites, `kind_stated`
separating chosen from inherited kinds, a live violation trace), and the one
concrete result so far — 52 grep-visible `ensure_synthetic_class` call sites, 3
of which fire on a strict boot — suggests the grep-derived sizes in these
records are systematically wrong in the same direction.

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
   * **The `StringJoiner` heap-reference-integrity defect** (HIB-CV-32 family).
     **Probably already gone** — it did not reproduce on 2026-08-04 under the
     exact merge that is supposed to trigger it, with collector pressure and a
     HotSpot control. What is missing is a suite run (H2, Hibernate), because
     the defect was found in a suite and a microprobe has repeatedly failed to
     predict a real library here. Cheapest of the three blockers to retire, and
     the one most likely to be retired already.
   * **Real `ThreadPoolExecutor` field initialisation** so
     `Executors.new*ThreadPool()` returns objects built by the real `<init>`.
     Until this is fixed, reclassifying `native_es_execute` drops it under
     `--jdk-only` and strict mode loses thread pools.
4. **Items 3, 7, 8, and item 11 §4/§8/§9/§11** — delete the hard-coded lists,
   each with its own regression corpus. Gated on 1 and 3.
5. **Items 4 and 5** — migrate the remaining `ensure_synthetic_class` callers,
   settle `Proxy$Instance`, then delete `is_synthetic_stub`. Drive the migration
   from the `requested_by` census, not from a grep.
6. **Item 2** — finish the layout sweep across `native-builtins`,
   `native-collections`, `native-io` and `vm/src/native/`. Independent of the
   rest and can run in parallel, but it is the item most likely to surface new
   blockers.

## Standing constraints for anyone working this list

* `native-builtins/tests/stub_ratchet.rs` asserts `BASELINE_SYNTHETIC_STUBS =
  157` **exactly**, with `SLACK = 0`, and separately asserts only
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
  `docs/internal/` when it is fixed, not when it is planned.
