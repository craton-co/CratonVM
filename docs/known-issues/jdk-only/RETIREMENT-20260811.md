# 2026-08-11 — retirement audit: 30 records moved out, 22 kept

> **CORRECTION, 2026-08-12 (W7-55-record-reconciliation.md) — three of the
> "kept" rows give a reason that was already false when this audit ran.** The
> *reasons for keeping* are what put a record in front of the next reader, so a
> wrong one costs a run. The moves themselves were not re-audited and are not in
> question; only these three reasons are.
>
> * **`W4-1`** (row at the "Kept — 22" table): *"two optional hardening patches
>   on `lk_drop_lookup_mode` / `lk_in_method` that are still unapplied"*. **Both
>   landed on 2026-08-07 in commit `dcfe77cb8`**, four days before this audit —
>   `native-builtins/src/classloader.rs:9571` and `:9543` — and W4-1's own text
>   already said so. W4-1's only live item is a deletion-only cleanup of the dead
>   `classloader.rs` lookup block, plus four unit tests aimed at it.
> * **`W4-2`**: *"the `ServiceLoader` + encapsulated-provider interaction and
>   `is_package_exported_to` failing closed … are both still open"*. The first
>   landed in `b3aca74c8` (`grant_reflective_override`,
>   `native-builtins/src/service_loader.rs:1564`); the second is adjudicated
>   **unreachable**, by W4-2's own three-writer argument. W4-2's actual live item
>   — array classes reporting module `java.base` regardless of component type,
>   `classloading/src/class_manager.rs:9568` — is not mentioned in this audit at
>   all.
> * **`W6-8`**: *"`Field.get`/`Field.set` ask the `opens` question
>   unconditionally … and the whole `Lookup.unreflect*`/`find*` family has no
>   module check of any kind"*. First half wrong — `dcfe77cb8` dissolved the
>   `is_public` split at `native-builtins/src/lang_class.rs:1337`. Second half
>   half-wrong — the *mode* check landed in `3644142d5`
>   (`lk_enforce_unreflect_access`, `native-builtins/src/lang_invoke.rs:4452`,
>   six call sites); only the *module* half remains, deliberately.
>
> The rest of this audit's own findings — in particular its table of fourteen
> records that claimed a hand-off patch was never applied when it was in the
> tree — were confirmed and extended by the 2026-08-12 pass, which found
> **eighteen more**.

**What this is.** A per-record read of every record that was in
`docs/known-issues/jdk-only/` on 2026-08-11, against the rule stated at the
bottom of the README — *this directory holds **unfixed** issues only* — and
against the licence the 2026-08-07 strict-corpus run does and does not grant.
Thirty records were moved into the internal record tree with `git mv`, so
history follows and the move is reversible with a second `git mv`. **Nothing was
deleted.** Each moved record carries a closing note at its top saying what
discharged it and on what evidence.

**The evidence that discharges most of them, named once.** Three
ABBA-interleaved runs of `CRATONVM_ARGS="--jdk-only" bash regression-suite/run.sh`
against a dev `5d22671e3` release binary on 2026-08-07, identical each time:
**53 passed, 1 failed**. The one failure is `RMapGcStress`, which fails in every
mode and every build and is not this campaign's. Compatible mode was unchanged
at 30/1. Every `RJdk*` vector named below is in `JDKONLY_CLASSES`
(`regression-suite/run.sh:95`); none of the records audited here owns a
`CORE_CLASSES` vector.

**What that run does not license, and how this audit honoured it.** It
discharges the "unverified by execution" caveat **at the vector level** only. It
is not a per-assertion audit. So every record below was read for three things:

* **(a) the vector it owns**, and whether that vector is in `JDKONLY_CLASSES`.
  Four records own no vector at all (`L20`, `W5-4`, `W6-3`, `W6-4`); for those
  the corpus run says nothing and the discharge had to come from somewhere else,
  which is stated per row.
* **(b) whether the record claims something narrower than its vector asserts** —
  a specific mechanism, a fixed arm plus a diagnosed one, a deliberate scoping
  decision. Records that do keep that claim and stayed.
* **(c) live residuals** — `Out-of-file patch`, `Required out-of-file change`,
  `Still open`, `Not fixed here`, `left open`, `OPEN`, `PARTIAL`, `DIAGNOSED`.

**(c) was checked against the source, not against the record.** That turned out
to be the most valuable half of the audit: **fourteen records claim a patch was
never applied, or a defect never fixed, that is in the tree today.** Those
disagreements are listed in their own section below, because they are worth more
than the moves.

---

## Moved — 27 to `fixed-bugs/`

| Record | Now | (a) vector | (b) narrower claim? | (c) residual, checked against source |
|---|---|---|---|---|
| `L1-reflect-setaccessible-invoke.md` | `jdk-only-L1-reflect-setaccessible-invoke-FIXED-20260811.md` | `RJdkReflect` ✓ | no — the claim is the caller step on `Method.invoke`, exactly what the vector exercises | its out-of-file patch **is** applied (`lang_class.rs:696`, `:8716`); its "Known residual" was re-homed to `L15`, which stays |
| `L2-methodtypeform-lambdaforms-null.md` | `jdk-only-L2-methodtypeform-lambdaforms-null-FIXED-20260811.md` | `RJdkHandles` ✓ | no | "Required out-of-file change (not applied)" **is** applied — `vm_exec.rs:23032` |
| `L3-definehiddenclass-returns-null.md` | `jdk-only-L3-definehiddenclass-returns-null-FIXED-20260811.md` | `RJdkHidden`, `RJdkStrict` ✓ | no — DIAGNOSED-only, and the diagnosis was the whole deliverable | all **three** "Required out-of-file changes" are applied, including the one it called merely recommended |
| `L4-jmx-iterator-remove-default-method.md` | `jdk-only-L4-jmx-iterator-remove-default-method-FIXED-20260811.md` | `RJdkJmx` ✓ | **yes** — it fixed one arm and diagnosed the other, so its claim is per-arm | both arms are fixed: the `--real-jdk` half in-lane, the `--jdk-only` half by L13. Baseline re-freeze superseded by the bridge-reclassification wave |
| `L5-files-copy-alreadyexists-and-filevisitoption-dispatch.md` | `jdk-only-L5-files-copy-alreadyexists-and-filevisitoption-dispatch-FIXED-20260811.md` | `RJdkNio` ✓ | no | patches A, B **and** the OPTIONAL C are all applied; "What this does NOT fix" (64 unmeasured checks) is discharged by the vector passing all 78 |
| `L6-so-rcvbuf-getoption.md` | `jdk-only-L6-so-rcvbuf-getoption-FIXED-20260811.md` | `RJdkNet` ✓ | no | none — in-lane only, no out-of-file patch, only a falsifier section |
| `L7-adler32-missing-natives.md` | `jdk-only-L7-adler32-missing-natives-FIXED-20260811.md` | `RJdkJni` ✓ | no | none; the record itself says no baseline needs re-freezing |
| `L9-module-not-in-boot-layer.md` | `jdk-only-L9-module-not-in-boot-layer-FIXED-20260811.md` | `RJdkModule` ✓ (44/44) | **yes** — filed PARTIAL, resolution layer only | both wiring patches applied; all seven items of "What remains" closed by W2-3 / W3-3 / W4-2 / W5-3 / W6-2 / W6-11 |
| `L11-altmetafactory-marker-interfaces.md` | `jdk-only-L11-altmetafactory-marker-interfaces-FIXED-20260811.md` | `RJdkLambdas` ✓ | no | all four "patches handed to the orchestrator" are in the tree |
| `L12-forkjoinpool-no-workers-awaitdone-hang.md` | `jdk-only-L12-forkjoinpool-no-workers-awaitdone-hang-FIXED-20260811.md` | `RJdkForkJoin` ✓ | **yes** — "the fix is incomplete for the whole test" | patches A and B applied, each carrying the record's own `// L12:` comment; the incompleteness it named was closed by L19 and W2-8 |
| `L13-arrayitr-remove-writethrough.md` | `jdk-only-L13-arrayitr-remove-writethrough-FIXED-20260811.md` | `RJdkJmx` ✓ | no | the only pending item was a baseline re-freeze, superseded by the bridge-reclassification wave |
| `L14-serviceloader-instance-caching.md` | `jdk-only-L14-serviceloader-instance-caching-FIXED-20260811.md` | `RJdkServices` ✓, and its failing arm was the DEFAULT one | no | none — its closing section is a falsifier, not an open item |
| `L18-function-identity-not-synthetic.md` | `jdk-only-L18-function-identity-not-synthetic-FIXED-20260811.md` | `RJdkLambdas` ✓ | no | the out-of-file patch to `native-api/src/registry.rs` **is** applied (`:6442-6470`) |
| `L19-countedcompleter-lazy-fork-starvation.md` | `jdk-only-L19-countedcompleter-lazy-fork-starvation-FIXED-20260811.md` | `RJdkForkJoin` ✓ | **yes** — the cure shipped behind a gate that was OFF | the gate flipped on 2026-08-07, exactly as the record prescribed, knob kept as an opt-out |
| `W2-5-methodhandles-arrayelement-combinators.md` | `jdk-only-W2-5-methodhandles-arrayelement-combinators-FIXED-20260811.md` | `RJdkHandles` ✓ | **yes** — "one second-order failure … diagnosed but deliberately not fixed" | that second-order failure got its own lane (W3-1), landed, and is now on by default |
| `W2-6-inflater-swallows-corrupt-input.md` | `jdk-only-W2-6-inflater-swallows-corrupt-input-FIXED-20260811.md` | `RJdkJni` ✓ | no | its "Adjacent, out of this lane's files" item was taken by W5-1 and W6-6 and fixed |
| `W2-7-fabricated-success-where-the-spec-mandates-failure.md` | `jdk-only-W2-7-fabricated-success-where-the-spec-mandates-failure-FIXED-20260811.md` | `RJdkJni`, `RJdkFailure`, `RJdkProcess` ✓ | no — it is an inventory with a disposition per row | the one OUT-OF-FILE row (#4, `ProcessHandle.current()`) is applied; the row it handed to another lane was closed by W2-3 |
| `W2-8-forkjointask-invoke-returns-computes-null.md` | `jdk-only-W2-8-forkjointask-invoke-returns-computes-null-FIXED-20260811.md` | `RJdkForkJoin` ✓ | **yes** — filed as "cure specified", not landed | the cure **is** in `phases_early.rs:8489-8509`; the wall behind it belongs to W3-4, which stays open |
| `W3-1-invokeexact-must-not-fabricate-a-zero.md` | `jdk-only-W3-1-invokeexact-must-not-fabricate-a-zero-FIXED-20260811.md` | `RJdkHandles` ✓ | **yes** — "behind an opt-in flag that defaults to today's behaviour" | the out-of-file patch is applied, and the flag default flipped 2026-08-07 to opt-OUT |
| `W3-2-non-nestmate-hidden-class-nest-host.md` | `jdk-only-W3-2-non-nestmate-hidden-class-nest-host-FIXED-20260811.md` | `RJdkHidden` ✓ | no | "Residual on this vector" was an expectation the pass settles; the "optional companion patch" is on code the record itself proves dead |
| `W3-3-named-module-must-not-read-unnamed.md` | `jdk-only-W3-3-named-module-must-not-read-unnamed-FIXED-20260811.md` | `RJdkModule` ✓ (44/44) | no | the companion patch to `vm/src/config.rs::parse_add_reads` **is** applied; both "Still open on this vector" items closed by W5-3 and W6-2 |
| `W3-7-sslcontext-bogus-protocol.md` | `jdk-only-W3-7-sslcontext-bogus-protocol-FIXED-20260811.md` | `RJdkSecurity` ✓ | no | both out-of-file patches applied; "the next failure behind it" was taken by W4-3 and fixed |
| `W5-3-module-resource-encapsulation.md` | `jdk-only-W5-3-module-resource-encapsulation-FIXED-20260811.md` | `RJdkModule` ✓ (44/44) | **yes** — half the fix was out-of-file | "The patch that is not mine" **is** applied (`native-builtins/src/lib.rs:18376-18388`), comment and all |
| `W6-1-varhandle-vartype-coordinatetypes.md` | `jdk-only-W6-1-varhandle-vartype-coordinatetypes-FIXED-20260811.md` | `RJdkHandles` ✓ | **yes** — five defects fixed, a sixth explicitly NOT fixed | the "Required out-of-file change (not applied)" is applied, **and defect 6 is fixed too** — see the disagreements below |
| `W6-3-slot-index-species-residuals.md` | `jdk-only-W6-3-slot-index-species-residuals-FIXED-20260811.md` | **none** — a sweep record | **yes** — its own verdict is per-site | its one reported residual (`classloader.rs::alloc_lookup`) is applied; the other two "Reported, not edited" items belong to `W4-4` and `W4-1`, both of which stay |
| `W6-4-duplicate-registration-gate.md` | `jdk-only-W6-4-duplicate-registration-gate-FIXED-20260811.md` | **none** — an instrument record | **yes** — the gate is the deliverable, not a vector | the gate ships and is seeded (1206 shadowed registrations, 53 kind disagreements); its one live defect was REFUTED in its own margin by W6-12, whose `Phaser` sibling was fixed. "What this gate does not cover" states scope, not defects |
| `W6-7-forkjointask-quietly-family.md` | `jdk-only-W6-7-forkjointask-quietly-family-FIXED-20260811.md` | `RJdkForkJoin` ✓ | **yes** — "latent, not live" for most of the family | the registrations are in `phases_late/concurrent.rs`; its "Recorded, not fixed" item (`complete(Object)V`) was taken by W6-9, which **stays open** with a fix in flight |

## Moved — 3 to `retired/`

These three are not defect records at all, which is why they are retired rather
than filed as fixed.

| Record | Now | Why it is not a known issue |
|---|---|---|
| `L10-rjdkprocess-vector-overassertion.md` | `jdk-only-L10-rjdkprocess-vector-overassertion-RETIRED-20260811.md` | (a) `RJdkProcess` ✓. (b)/(c): the record's own headline is that **no VM code was changed** — the *vector* was a broken oracle that real HotSpot 25 also fails intermittently. A corrected test is not an unfixed CratonVM issue. The one under-assertion it noted and left alone belongs to the vacuous-test sweep, which stays open. |
| `L20-regression-risk-review.md` | `jdk-only-L20-regression-risk-review-RETIRED-20260811.md` | (a) owns no vector — SOURCE REVIEW ONLY, nothing built, nothing run. (b) it makes seven SAFE verdicts, all narrower than any vector. (c) its two patches are both explicitly conditional and neither condition fired. Its question — *can this campaign break something that works today?* — was answered by execution: 53/1 strict, Compatible unchanged at 30/1. |
| `W5-4-invokeexact-default-decision.md` | `jdk-only-W5-4-invokeexact-default-decision-RETIRED-20260811.md` | (a) owns no vector. (b)/(c) its entire subject is a **decision that has since been taken**: it recommended FLIP and left the flip unapplied. `CRATONVM_MH_STRICT_INVOKEEXACT` was flipped on 2026-08-07 to the exact shape §6 prescribed — `vm/src/vm/vm_exec.rs:1588` treats only the literal `"0"` as off, and `types/src/flag_groups.rs:1264` declares it with `off_word: Some("0")` and no `off_key`. |

---

## Record-vs-source disagreements found — the fourteen

Every one of these is a record saying a change was **not** made, or a defect
**not** fixed, where the tree says otherwise. Four were already known before
this audit (L2, L3, W6-3, W6-7); ten are new.

| Record | What it says | What the source says |
|---|---|---|
| `L1` | out-of-file patch "reported to the orchestrator" | applied — `native-builtins/src/lang_class.rs:696`, `:8716` |
| `L2` | "Required out-of-file change (**not applied** — outside this lane's files)" | applied — `vm/src/vm/vm_exec.rs:23032`, with the record's comment verbatim |
| `L3` | "**No code change was made**"; three required out-of-file patches | all three applied — the `lang_invoke.rs` placeholder is gone (`defineHiddenClass` now registered once, `classloader.rs:9946`), `lookup_define.rs:467-479` mangles from `this_class`, and `alloc_lookup_for` writes by name |
| `L4` | the `--jdk-only` arm is "DIAGNOSED, NOT FIXED — the patch this needs (not applied)" | applied by L13 — the `Arrays$ArrayItr` side table and the `Intrinsic` `java/util/Iterator.remove()V` registration are in `native-collections/src/lib.rs` |
| `L5` | patches A and B REQUIRED, C OPTIONAL, none applied | **all three** applied, including C (`native-io/src/lib.rs:12255`) |
| `L9` | "**Not landed** — two wiring patches this lane does not own" | both landed — `vm/src/vm/vm_init.rs:993`, `native-builtins/src/jboss_jdkspecific.rs:287,326` |
| `L11` | four "Out-of-file (patches handed to the orchestrator)" | all four in the tree |
| `L12` | two "Out-of-file patches (**must be applied** for this fix to do anything)" | both applied, each with the record's `// L12:` comment |
| `L16` | fix "PARTIAL … the two one-line guard patches … are in `native-builtins/`, which this lane does not own" | **both applied** — `classloader_real.rs:991` and `classloader.rs:2939` both call `cratonvm_classloading::array_descriptor_element_class`. *(This record still stays — see the keep-list.)* |
| `L18` | fix written "as an out-of-file patch", not built | applied — `native-api/src/registry.rs:6442-6470` |
| `W2-8` | "cure **specified**", four-line arm change plus a helper, lane could not build | the cure is in `native-builtins/src/phases_early.rs:8489-8509`, `method_exists` guard and memoisation included |
| `W3-3` | "Companion patch outside this lane's files" | applied — `vm/src/config.rs::parse_add_reads` folds `ALL-UNNAMED` to the empty-string sentinel |
| `W5-3` | "The patch that is not mine" | applied — `native-builtins/src/lib.rs:18376-18388`, comment verbatim |
| `W6-1` | "Required out-of-file change (**not applied**)" *and* defect 6 "**NOT FIXED** — outside this lane's files" | **both** done: `is_method_handles_varhandle_factory_native_override` (`native_override.rs:716`, `vm_exec.rs:23008`), and defect 6 is closed by `vh_strict_reference_return` + `boxed_primitive_supertypes` (`vm_exec.rs:1717-1760`), whose kill switch is documented as restoring "the pre-W6-1 silent wrong answer" |

Two records were stale in the **other** direction — they describe a decision as
pending that has since been made:

* `L19` and `W5-4` — `CRATONVM_FJP_EAGER_FORK` and
  `CRATONVM_MH_STRICT_INVOKEEXACT` both had their defaults flipped on
  2026-08-07 (`native-builtins/src/phases_late/concurrent.rs:7107`,
  `vm/src/vm/vm_exec.rs:1588`). Both flips kept the knob as an opt-out, which is
  what each record asked for.
* `W3-4` is the third record in that family. It is **not** moved here — it is
  open and another lane is working it.

**The rule this establishes.** Before believing a record that says a patch was
never applied, grep for the patch's distinguishing token. Fourteen records were
wrong about it, and **not one** hand-off recorded as pending turned out to be
genuinely un-landed. A lane that cannot edit a file writes the patch down and
hands it off; nobody goes back and edits the record when the hand-off lands.

---

## Kept — 22, with the reason each stays

**Seventeen kept on the audit's own verdict:**

| Record | Why it stays |
|---|---|
| `L8-securerandom-provider.md` | (c) live residual: `native-builtins/src/crypto_impl.rs:1381` still registers `SecureRandom.<init>([B)V` to a no-op that stamps neither `algorithm` nor `provider`, and wins under synthetic-jdk. The record names the clean fix; it has not been made. |
| `L15-nestmate-access-field-and-constructor.md` | (c) "Open: constructor access is unchecked" — `Constructor.newInstance` still has the caller-step gap L1 named. This is the record L1's residual was re-homed to. |
| `L16-classnotfound-vs-noclassdeffound-shapes.md` | (c) "Residual divergence, not fixed": our `ClassNotFoundException` message names the **array descriptor**, HotSpot's `Class.forName` names the **element**. Closing it is a `lang_class.rs` change with a wider blast radius. (Its two guard patches *are* applied — the PARTIAL status is stale, the residual is not.) |
| `W2-1-strict-refuses-the-synthetic-stream-stack.md` | (b) the fix is one guard; the record's durable half is the inventory showing the stream stack is **still SPLIT**, plus a four-step staged retirement path that has not been walked. |
| `W2-2-blocked-reader-async-close-wakeup.md` | (c) "Residual, not fixed here": `net_phase_e.rs::re1_socket_read_stream` still parks in `read()` with no close-awareness — verified in source. |
| `W2-3-module-descriptor-answers-empty-sets.md` | (c) "What still has no data source": `modifiers()`, `Requires.compiledVersion()`, `version()`, `rawVersionString()`, `mainClass()` are all still unanswerable. `RJdkModule` asserts none of them, which is exactly the case the licence carves out. |
| `W3-6-processimpl-missing-natives.md` | (c) two sections of live residual: "Deliberately not fixed" (`os_process_start_time`, `info0` on Windows) and "Found, not fixed — out of lane" (`children()`/`descendants()`/`parent()` fabricating on the `ProcessHandle` interface, live under synthetic-jdk). |
| `W4-1-publiclookup-allowedmodes-never-checked.md` | (c) "Residual (deliberately not enforced)" plus two optional hardening patches on `lk_drop_lookup_mode` / `lk_in_method` that are still unapplied — the same two sites W6-3 filed as latent. |
| `W4-2-unnamed-accessor-bypasses-encapsulation.md` | (c) "Not fixed here, same family": the `ServiceLoader` + encapsulated-provider interaction and `is_package_exported_to` failing closed for an unregistered target are both still open. |
| `W4-3-security-getalgorithms-short-list.md` | (b)/(c) "Known divergence": the returned set is a plain `HashSet`, not `Collections.unmodifiableSet`, and two real SHAKE digests are deliberately not advertised. Both are decisions the record deferred rather than closed. |
| `W4-4-slot-index-species-sweep.md` | (c) "Latent, not currently live" names `build_string_set`, `ModuleLayer.modules()` and three more 3-slot `HashSet` allocations that must be switched before anything promotes them to the real-JDK path, plus two unfixed GC stale-local sites in `register_p59_module`. |
| `W5-1-loadlibrary-allowlist-too-wide.md` | (c) "Known residual — NOT fixed": no loader-scoped `loadedLibraryNames` bookkeeping exists, so the dynamic already-loaded rule cannot fire. |
| `W5-2-two-silently-skipped-process-checks.md` | (c) "Residuals (unmeasured by any vector, recorded not fixed)": `Info.user` null on both platforms, `Info.totalTime` `-1` on Linux. Precisely the case the licence excludes — no vector asserts them. |
| `W6-2-module-serviceloader-provider-factory.md` | (b)/(c) "Deliberately NOT done": the constructor-form provider subtype check is absent because the lane could not measure the blast radius on the JDK's own boot modules. |
| `W6-6-nativelibraries-load-fabricated-success.md` | (c) "Known residual (not introduced here, and not closable from this file)" — the same missing loader-scoped bookkeeping as W5-1. |
| `W6-8-method-invoke-exports-gate.md` | (c) its own inventory still carries **OPEN** rows: `Field.get`/`Field.set` ask the `opens` question unconditionally and over-deny public fields, and the whole `Lookup.unreflect*`/`find*` family has no module check of any kind. |
| `W6-12-stampedlock-split-brain.md` | (c) "One residual worth recording rather than fixing blind": the `java/util/Collections` family is served by two registrars at different fidelities, so `unmodifiableSet(s).add(x)` throws while `unmodifiableList(l).add(x)` succeeds. |

**Five reserved — not read, not moved, not touched.** Other lanes are editing
these on sibling branches that merge with this one. They are open, with fixes in
flight: `W3-4-forkjointask-status-flags-and-the-eager-default.md`,
`W6-5-vacuous-tests.md`, `W6-9-complete-erases-the-abnormal-record.md`,
`W6-10-process-enumeration-syscall-cost.md`,
`W7-1-treemap-views-and-iterator-remove-contract.md`, and anything matching
`W7-2-*` through `W7-6-*`, some of which arrive at merge.

---

## Known consequences of the moves, not resolved here

* **Eight Rust comments cite a record by its old public path.** Two of them
  name records that moved: `native-api/src/registry.rs:6441` (L18) and
  `native-builtins/src/lang_reflect.rs:1585` (L1), plus
  `native-builtins/src/phases_late/reflect_invoke.rs:1865` (W6-1) and two
  citations of W6-4 — `native-api/tests/shadowed_registration_detection.rs:5`
  and `native-builtins/tests/duplicate_registration_gate.rs:669`, the latter
  inside a *failure message* a developer reads when the gate trips. The moved
  filenames carry a `jdk-only-` prefix, so `types/tests/doc_citation_paths.rs`
  does not fail on them (its basename rule finds no unique relocation target),
  but the citations are dead for a reader. Repointing them is a Rust edit and
  was out of scope for this docs-only pass; they should be rewritten
  prefix-lessly, e.g. `jdk-only-L18-function-identity-not-synthetic-FIXED-20260811.md`.
* **`docs/architecture/natives-over-real-jdk-classes.md` §9** cites seven moved
  records by their old path (`W6-3`, `W3-2`, `L13`, `W6-4`, `W6-1`, `L10`,
  `W3-7`) in its "records this page corrected" table. That file is owned
  elsewhere and was left alone.
* Records that stay may reference a moved sibling by relative filename. Those
  links now 404 in a viewer; the target is always
  `jdk-only-<same stem>-FIXED-20260811.md` (or `-RETIRED-`) in the internal
  tree.
