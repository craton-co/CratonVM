# Wave consistency audit — `feat/c2-review-remediation`, 2026-08-01

**Scope:** the 71 commits in `origin/dev..HEAD` (89k inserted lines, 210 files).
**Question asked:** not "is any single change correct" — the workspace compiles
and the unit suites are reported green — but "do these independently-written
changes contradict *each other*". Duplicate fixes where only one is live,
arguments a later commit invalidated, two golden tables for one invariant,
hand-applied cross-file halves, docs that describe code that is not there, and
refusals that stack into an inert subsystem.

**Method:** `git log --oneline origin/dev..HEAD`, then `git show` per commit,
then reading the *current* tree at every cited site. Every finding below states
whether both sides were read or whether it is an unconfirmed pattern match.

**A method limitation that shapes everything here.** Every commit in this wave
is authored `Roadmap Orchestrator <orchestrator@craton.local>`, and several
commits batch multiple lanes (`d311d6339`, `4d8c68341`, `87c609516` — whose
message describes a difftest corpus while its diff is `jit/src/x64/isel.rs` and
two architecture docs). So hazard #4 (*ownership seams — orchestrator-applied
edits vs. what a lane specified*) could only be checked where a commit
self-identifies as such (`dc9eceeff`) or where a doc records the specification
verbatim (`docs/jit/code-cache-lifetime.md` §6). There may be other
hand-applied edits that are indistinguishable from lane work in the history.

---

## Findings

| # | Finding | Commits | Evidence | Severity | Recommended action |
|---|---|---|---|---|---|
| **1** | **Every VM-less JVMTI event is now silently dropped in production.** `8ebe8e604` re-scoped JVMTI per VM and left six call sites deliberately unattributed, arguing *"their events land on the unattributed manager, which is exactly where they land today"*. That was true only while `SharedVm::new` called `install_global_manager`. `dc9eceeff` — the orchestrator's hand-applied half — replaced that call with `install_manager_for_vm(vm.vm_identity, ..)`. Nothing in production now installs the `UNATTRIBUTED_VM` (`0`) row, and the VM-less `fire_*` free functions resolve it through `global_manager()` → `manager_for_vm_exact(0)`, which has **no fallback**. | `8ebe8e604`, `dc9eceeff` | `vm/src/runtime/jvmti.rs:3011` `global_manager()` = `manager_for_vm_exact(UNATTRIBUTED_VM)`; callers `fire_vm_init` `:3259`, `fire_vm_death` `:3266`, `fire_class_load` `:3283`, `fire_class_prepare` `:3291`, `fire_gc_start` `:3299`, `fire_gc_finish` `:3306`. Their only production call sites are `vm/src/vm/vm_init.rs:3298, 3301, 3377, 3380, 3408, 7393`. The only remaining `install_global_manager` caller in the workspace is `vm/src/runtime/jvmti.rs:4971`, inside `#[cfg(test)]`. The test that would have caught it, `an_unclaimed_vm_falls_back_to_the_unattributed_row`, calls `ensure_global_manager()` itself first — it populates the row the production path no longer populates, so it passes vacuously. | **HIGH** | Either make `install_manager_for_vm` also seed the unattributed row on first install, or convert the six VM-less `fire_*` free functions to take a `vm_identity` (the two hook adapters need the cross-crate signature change `jvmti-vm-scoping.md` §"left unattributed" describes). Add a test that asserts `global_manager()` is reachable *after a real `SharedVm::new`*, not after a test-only install. |
| **2** | **The 2 368-line code-cache lifecycle subsystem is inert.** `vm/src/jit/code_cache_lifecycle.rs` records installs, retirements, allocation failures, capacity and free space — and no production code calls any recorder. `pending_retirements()` therefore always returns 0, so the two `sweep_if_quiescent()` call sites in `conservative_roots.rs` are guarded by a condition that can never be true. The module reads as retirement coverage and is a no-op. | `d311d6339` / the `code_cache_lifecycle` lane; `aa3c737e0` also edits the file | Only callers outside the module: `vm/src/jit/conservative_roots.rs:732-733` and `:831-832`, both `if pending_retirements() != 0 { sweep_if_quiescent() }`. `record_install`/`record_retirement`/`record_allocation_failure`/`record_capacity_bytes`/`record_free_space`/`process_lifecycle()` appear nowhere else in the workspace; the only in-module uses are at `:2341`/`:2346`/`:2364`, inside `#[cfg(test)] mod tests` (`:1574`). The lane's own doc says so — `docs/jit/code-cache-lifecycle.md` §Reconciliation 2 gives the required `record_install` call and notes `jit` cannot depend on `vm` — but nobody applied either option. | **HIGH** | Wire it (option 2 in that doc — call from the vm-side publication point) or delete it. Leaving it in place means a future auditor reads `sweep_if_quiescent` at a GC root-scan site and concludes retired bodies are swept. |
| **3** | **A new flag was added after the flag-declaration lane closed, so `types/tests/flag_declaration_guard.rs` is red.** `b24eee75d` (09:28) declared 26 flags and drove the guard's offender count to zero. `66f0b6039` (09:49) added `CRATONVM_JIT_STRICT_INSTALL_EPOCH`, read through `cratonvm_types::flags::runtime_var`, and did not declare it. | `b24eee75d`, `66f0b6039` | Read site `jit/src/lib.rs:8934` (non-comment line, exact whole-string literal — matches the guard's `exact_literals`). Absent from `types/src/flag_groups.rs` (`INVENTORY`/`SCALARS`), from `types/tests/flag-surface.txt`, from the guard's `ALLOWED`, and from `docs/CONFIG.md`. Consequence beyond the red test, per the guard's own module docs: an undeclared flag is served by live `getenv`, is unreachable from `CRATONVM_<GROUP>=token`, and cannot be arranged by `flags::with_thread_overrides` — so any test that tries to turn the install barrier off gets the developer's ambient environment instead. | **HIGH** | Add the token to `flag_groups.rs::INVENTORY` and the name to `flag-surface.txt`. Note the polarity trap `b24eee75d` documents: this gate is `runtime_var(..).map(\|v\| v != "0").unwrap_or(true)`, i.e. default-ON with `"0"` meaning off — the inventory row's `off_key`/`off_word` must spell that, not the reverse. |
| **4** | **A test-fixture control was deleted, not fixed, while the bug it discriminates is left open.** `e3824ee07` `#[ignore]`s `a_rewritten_compile_publishes_osr_metadata_in_interpreter_bci_space` — correctly, the failure is real — and its triage note says the most likely explanation is *"the fixture `compile_accum_fixture` compiles is not the one `shape_int_accum_loop` returns"*. In the same commit it **deletes** `the_fixture_loop_is_what_the_planner_is_offered`, which is precisely the test that pins what the planner is offered. The commit message accounts for the deletion only as *"Also removes a duplicated `#[test]` attribute."* | `e3824ee07` | Pre-image `git show e3824ee07^:jit/src/x64.rs` has `the_fixture_loop_is_what_the_planner_is_offered` at `:39942` — exactly once, not duplicated. The commit's own diff is three hunks (`@@ -25141`, `@@ -39941,19 +39964,0`, `@@ -40205,0 +40211,27`); no duplicated `#[test]` attribute is removed anywhere in it. The deletion also took the doc comment belonging to the *next* test (`the_rewriter_is_off_by_default_and_armed_per_thread`). | **MEDIUM-HIGH** | Restore `the_fixture_loop_is_what_the_planner_is_offered` and run it; it answers the ignored test's open triage question in one step. If the fixture really has drifted, the ignored test's constants are describing a different method and the wrong-code conclusion needs re-deriving. |
| **5** | **The hand-applied OSR epoch witness is narrower than the specification it implements, and its own API doc still denies it exists.** `docs/jit/code-cache-lifetime.md` §6.1 specifies: *"add, at the top of that compile block (**before the constant-pool resolvers run**)"*. `da666127a` placed it immediately before `x64::compile_with_param_slots`, i.e. after all constant-pool resolution — which includes `load_class_concurrent` calls, the very GC/redefine points the barrier is for. | `66f0b6039`, `da666127a` | Witness at `vm/src/runtime/interpreter/invoke.rs:15181`. Constant-pool resolution for multianewarray / typecheck / static-field / field / invoke / indy runs at `:14252`–`:14790`, all above it. Compare `jit/src/lib.rs:11687`, where `try_compile` opens the witness as its first statement with the comment *"before any constant-pool resolver runs"*. Separately, `open_compile_epoch_witness`'s own doc comment (`jit/src/lib.rs:8884-8892`) still reads *"A backend entered directly (the OSR compile in `vm/src/runtime/interpreter/invoke.rs` …) simply gets the narrower finalize-to-publish window … see `docs/jit/code-cache-lifetime.md` for the one-line change that widens it"* — describing a state `da666127a` partially changed. The RAII binding itself is correct: `let _compile_epoch = …`, not `let _ = …`, so the guard is not dropped immediately. | **MEDIUM** | Move the witness to the top of `compile_osr_artifact`'s compile block as specified, and update the `open_compile_epoch_witness` doc comment and §6.1 to match. |
| **6** | **`docs/jit/aarch64-parity.md` carries the wrong ARM barrier encodings — the exact two literals another lane corrected.** Two golden tables for one invariant; the doc is the wrong one. | `8f2c76db4` (doc), `d82fc4ea5` (test fix) | `docs/jit/aarch64-parity.md:123-125` records `DMB ISH 0xD50333BF`, `DMB ISHST 0xD50332BF`. `d82fc4ea5` corrected the test to `0xD5033BBF` / `0xD5033ABF`, and the emitter agrees: `jit/src/aarch64.rs:1431` is `0xD503_30BF \| ((option & 0xF) << 8)`, so `CRm=0b1011` → `0xD5033BBF`. Arithmetic checked by hand. The unchanged `DMB SY` row (`CRm=0b1111` → `0xD5033FBF`) is right in both places, which is why the disagreement is only visible on two rows. | **MEDIUM** | Correct the two literals in the doc. The doc's stated purpose is to be the manual-derived reference the test is checked against, so leaving it wrong inverts the check's direction. |
| **7** | **`HashCodeTable::update_after_gc` gained new behaviour and still has no production caller — and ~19 side-table safety comments cite it as their justification.** `5750caf5f` found this ("Nothing calls that function, so those justifications are false today") and then made the uncalled function *better* (remap → remap-and-sweep, plus a survival-predicate parameter) rather than wiring it or correcting the comments. | `5750caf5f` | Definition `gc/src/compact_header.rs:523`. Callers: `:1561` and `:1589`, both inside `mod tests`. The justification comments remain at `native-builtins/src/crypto_impl.rs:4474,4635`, `jca/cipher.rs:105,195`, `jca/message_digest.rs:52`, `jca/signature.rs:40,97`, `lang_invoke.rs:200`, `lib.rs:35753`, `securerandom.rs:47,190,223`, `wildfly_security.rs:715,746`, `xml_stax.rs:36,167`, `native-io/src/nio_selector.rs:2325`, `native-io/src/socket_channel.rs:3941`. | **MEDIUM** | `docs/gc/old-sweep-liveness.md` §"the audit's claim is verified" is the honest record; the tree is not. Either wire the function or sweep the 19 comments so they cite what actually keeps those tables valid (the heap's identity-hash minting). Both a doc test and a `#[allow(dead_code)]`-style annotation would be better than the current silence. |
| **8** | **Four docs still list as OPEN, with recipes, defects that a later commit in the same wave closed.** Each is the shape this branch has been bitten by before: a later session re-derives or re-fixes work that is already done. | see evidence | (a) `docs/known-issues/classloading-identity-audit.md:62` and §"Open 3 — array classes are always bootstrap-defined" — closed by `78a5c3024` 27 min later (`classloading/src/class_manager.rs:8242-8276`). Still partly true: `78a5c3024` deliberately narrowed the fix to user loaders, and the doc does not say so. (b) `docs/known-issues/vm-process-global-state-round-2.md:181` §"Still open — `FIELD_WATCHPOINTS`, DO NOT fix in isolation" with a "correct scope for the next pass" — executed by `8ebe8e604` 29 min later. (c) `docs/known-issues/vm-jit-cache-keying.md:79` and §260 mark `JIT_SIGNALS.exception` **OPEN** — closed by `f64f14ffa` 31 min later. (d) `docs/jit/helper-abi-audit.md:44` and §"remaining items" say *"the runtime validator is not armed … nothing in the workspace calls `validate_abi`"* — armed by `f64f14ffa` at `vm/src/jit/helpers.rs:13437`. | **MEDIUM** | Add a closing line to each, naming the commit. `docs/jit/code-cache-lifetime.md` §6.5 already asks for exactly this treatment of `code-cache-lifecycle.md` §Reconciliation 1 (finding 9) — the wave knows the pattern and did not apply it to itself. |
| **9** | **`docs/jit/code-cache-lifecycle.md` §Reconciliation 1 describes a fixed defect as an open "Required edit".** | `65495bfac` (fix), `66f0b6039` (flagged it) | The doc quotes `drain_deferred_jit_owners_if_quiescent` reading `is_zero()` before taking the lock and says *"Required edit: take `deferred_jit_owners().lock()` first"*. The current `jit/src/lib.rs` does exactly that, with the reasoning in the function body. `docs/jit/code-cache-lifetime.md` §6.5 flags this explicitly ("Leaving a fixed defect described as open is how a later session spends a day re-fixing it") and the retirement was not done. | **LOW-MEDIUM** | Retire §Reconciliation 1 as §6.5 asks. |
| **10** | **Dangling doc reference introduced by the JVMTI lane.** `vm/src/runtime/jvmti.rs:2975` points readers at `docs/known-issues/jvmti-delivery-threading.md` for "the current census" of unattributed hooks. That file does not exist. It is the census that would have surfaced finding 1. | `8ebe8e604` | `ls docs/known-issues/jvmti-delivery-threading.md` → no such file. The census content is in `docs/known-issues/jvmti-vm-scoping.md` §"left unattributed". | **LOW** | Repoint to `jvmti-vm-scoping.md`, or write the census file — and make it list the six `vm_init.rs` sites from finding 1 by line. |
| **11** | *(suspicion, not confirmed)* **Two independent DirectByteBuffer field-resolution paths may now exist.** `64e6d61b4` deleted the address-keyed `DIRECT_BUFFERS` table and rebuilt DBB field access in `vm/src/native/jni.rs` as descriptor-qualified-by-name with a fixed-slot path for the synthetic stub. `820c4dc4f` (24 min later, different crate) records as *left open* "a process-global `OnceLock` of DirectByteBuffer field indices where every other cache in that file is keyed per VM". | `64e6d61b4`, `820c4dc4f` | I did not trace the two to a common resolver. `DirectByteBuffer` appears in 12 files across `vm/src`, `native-io/src` and `native-builtins/src`. The risk shape, if real, is the one `64e6d61b4`'s own message describes: under the real JDK, slot 0 is `java.nio.Buffer.mark` (an `int`), not the address, so a second resolver still using fixed slots reads the wrong field. | **UNKNOWN** | Have whoever owns `native-io/src/direct_buffer.rs` check whether its field-index cache resolves the same way `jni.rs` now does, and whether it is per-VM. |

---

## Checked and found consistent — skip these next time

Each of these is a pairing that *looked* like a contradiction from the commit
messages and is not, verified by reading both sides in the current tree.

1. **`register_native_root_source` deleted vs. two lanes registering through it.**
   `cb0c0a6f4` wired the `ObjectStreamClass` cache in as `root_source!("osc-cache", ..)`;
   `64e6d61b4` then deleted `register_native_root_source`, its registry and its
   fan-out, claiming to retire "the last caller". Both survive:
   `vm/src/memory/native_roots.rs:335` (`osc-cache`) and `:347` (`instrument-transformers`)
   are in `VM_ROOT_SOURCES`, and `instrument_transformer_chain_is_a_registered_root_source`
   pins the second. `scan_osc_cache`/`remap_osc_cache` are distinct functions, so
   `every_root_source_pairs_two_distinct_halves` covers the classic swap.

2. **The named GC pairing — GCAUD-2's fail-closed abort vs. the in-place sweep fix.**
   `071afa11e` makes `OldGen::compact` abandon the whole compaction when Phase 0's
   live-set closure escapes the object walk, so "major GC reclaims NOTHING" while
   a live object references a freed old-gen block. `5750caf5f` closes the live set
   in the in-place sweep, which is the arm that *creates* that state. The first is
   **not** made unreachable: the sweep's closure prevents new freed-but-referenced
   blocks but cannot un-free an existing one — it reports `OLD_SWEEP_ESCAPE_HITS`
   and carries on, deliberately and with the asymmetry explained at the call site
   (`gc/src/gen_heap.rs`, the `if escaped` arm). The compactor's refusal stays
   load-bearing for pre-existing and `scan_region`-anomaly cases. Both arms now run
   the same `close_live_set_over_old_gen` fixpoint, and the sweep half carries a
   positive control (`close_live_set_promotes_a_referenced_target_and_leaves_real_garbage_dead`)
   that fails if the closure ever promotes everything — i.e. the "reclaims nothing
   forever" failure mode is pinned.

3. **Monitor lowering vs. the monitor refusal.** `dfd6a30c9` added "refuse a graph
   with monitor ops when the helper table has no monitor entry"; `833ae0310` then
   made `IrBuilder` emit those ops and `ir_lower` lower them through
   `runtime_lowering::emit_monitor_stub`; `0f07ab7db` added a *second*, different
   refusal (no precise deopt resume for a monitor-bearing graph, because every
   `FrameState` hard-codes `monitors: Vec::new()`). The first guard is now inert in
   production and `0f07ab7db`'s comment at `jit/src/ir_lower.rs:7100` says so
   explicitly rather than leaving it as false coverage. Monitor-bearing methods
   still compile and still deoptimize, via whole-method re-run. Consistent.

4. **`jit_pending_exception` scan/remap halves.** `f64f14ffa` moved the storage to
   `JvmThread` and added the remap half in `vm/src/memory/gc.rs`; `dc9eceeff` added
   the scan push at `vm/src/memory/roots.rs:540-547`. Both halves present.

5. **Two `loaded_classes` re-key implementations.** `6b00448bd` re-keys in
   `upgrade_synthetic_class` (`class_manager.rs:8683-8744`); `78a5c3024` re-keys an
   array class over the same component (`:8242-8276`), explicitly "mirroring" it.
   The duplication is deliberate and the array copy is guarded by
   `if array_loader != ClassLoaderId::Bootstrap`, so the insert-then-remove order
   cannot delete the entry it just wrote. Both use the same
   `stale_alias_is_ours` precondition.

6. **Install-barrier refusal frequency.** `66f0b6039`'s barrier could in principle
   starve compilation. `bump_jit_install_epoch` is reached only from
   `bump_redefine_epoch` and `JitCache::clear_all`, whose production callers are
   `vm/src/vm/vm_exec.rs:4736-4737` (redefine) and `vm/src/vm/vm_init.rs:3588`
   (teardown). Not a hot path; no "refuses everything" stacking.

7. **Default-off stacking on the x64 compile path.** Linear-scan regalloc
   (`39d5b1426`, write-through, off), range-BCE (`1078030f2`, off), the AVX2 emitter
   (`e734be64e`, behind the pre-existing admission gate) and the loop rewriter
   (`6e84a5e12`, per-thread opt-in) are all *optimisations* whose off state restores
   the previous path. I found no combination that leaves a compile path with no
   tier. The aarch64 backend is a different story (item 8 below) but is a platform
   that, per its own doc, has never executed.

8. **The aarch64 refusal stack.** `8f2c76db4` adds hard bails for any backward
   branch target, frames ≥ 4096 bytes, and `emit_oop_map_for_safepoint`, on top of a
   backend that already refuses every object-model opcode and all `invoke*`. That is
   very close to "refuses everything", and the doc says the compilable population is
   "leaf pure arithmetic, i.e. mostly loops". The commit states each gate has a
   negative control; I did not independently run or read all of them. Recorded as a
   deliberate posture on an unexecuted platform, not as a defect.

9. **`native-io` async completion handlers.** `820c4dc4f` roots them via
   global-root handles taken at enqueue time, not via a `VM_ROOT_SOURCES` row, so the
   absence of an `async_socket` entry in `native_roots.rs` is correct rather than a
   missing half.

10. **The opcode corpus arithmetic.** `docs/testing/opcode-coverage.md` claims
    197 of 202 generated, with `jsr`/`ret`/`jsr_w`/`nop`/`swap` unreachable by
    construction. 202 − 5 = 197; internally consistent, and `difftest/src/opcorpus.rs`
    landed in the wave (1 440 lines) even though the commit that *says* it did
    (`87c609516`) contains none of it.

---

## What I could not check, and why

* **Nothing was built or run.** Findings 3 and 1 predict a red test and a silent
  behaviour loss respectively; both are derived by reading the guard's own matching
  rules and the resolution chain, not by executing them. Finding 3 in particular
  contradicts the premise that the unit suites are green — either
  `types/tests/flag_declaration_guard.rs` was not in the run, or my reading of
  `exact_literals` is wrong. Run `cargo test -p cratonvm-types --test flag_declaration_guard`
  first; it is a two-minute disconfirmation.
* **Lane → commit attribution is not recoverable.** One author, several batched
  commits. Hazard #4 was checked only for `dc9eceeff` (self-identified) and against
  `code-cache-lifetime.md` §6 (a specification recorded verbatim). Other
  hand-applied edits are invisible.
* **The large JIT analysis files were not audited.** `jit/src/deopt.rs` (+7 392),
  `escape_analysis.rs` (+3 692), `scev.rs` (+3 257), `x64/isel.rs` (+6 848),
  `x64/licm.rs` (+2 821), `x64/simd_analysis.rs` (+2 367), `x64/vec_emit.rs`
  (+2 431), `regalloc.rs` (+1 577), `range_analysis.rs` (+1 384) — roughly a third
  of the wave by volume. I sampled only the interfaces these touch that other
  commits cite. A cross-file defect confined to that set would not appear here.
* **`docs/flag-tokens.md` vs. `flag_groups.rs::INVENTORY`.** The doc is generated
  (`tools/flag-census/render-tokens.sh`) and its section counts sum to ~649 tokens
  against 674 lines in `flag-surface.txt`, but tokens and variable names are not
  the same unit (an entry may carry both `on_key` and `off_key`), so the two numbers
  are not directly comparable. Re-run the generator and diff; that is the only
  reliable check.
* **The `GOLDEN_HELPER_OFFSETS` / `HELPER_FN_SIGS` tables** (62 rows, 53 signature
  slots) are const-asserted against the live struct, so a disagreement is a compile
  error and the green build covers them. I did not re-derive the 62 literal offsets
  by hand.
* **`docs/jit/instruction-patterns.md` vs. `x64/isel.rs`** — two descriptions of the
  same tiler, landed in the same mis-scoped commit (`87c609516`). Not cross-checked.
* **Whether any of the four "exhaustive list" tests added this wave**
  (`native-builtins/tests/shim_inheritance_guard.rs`,
  `native-collections/tests/gc_side_table_root_audit.rs`,
  `reader/tests/exception_table_ranges.rs`, `types/tests/flag_declaration_guard.rs`)
  agree with each other where their subjects overlap. They appear to be disjoint,
  but I did not enumerate their contents.
