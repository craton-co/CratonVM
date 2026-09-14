# W7-55 — 30 of 58 records were open because nobody edited them, not because the work was undone

**Status: COMPLETE, 2026-08-12.** This record is the reconciliation itself: what
was checked, what was wrong, and in which direction. It is a *bookkeeping*
record, not a defect record. It closes nothing in the VM and opens nothing in
the VM. Its output is the rebuilt `README.md` in this directory and thirty-one
corrected status lines.

Nothing was built or run for this pass. Every verdict below is git and source
archaeology, plus one set of vector measurements handed in from a run that
happened before it started.

> ## ADDENDUM 2026-08-12, later the same day (lane A4 / P3-B) — §8 re-adjudicated, and the record's own anchors had already rotted
>
> This record's §8 is a hand-off list, so it was re-checked against the tree by
> the rule §2 prescribes: **content, not line numbers.** Three of its bullets
> are now closed, one is refuted, one stands, and the record's own arithmetic
> has moved.
>
> **The anchors rotted within hours.** Six line citations were checked; **all
> six missed**, every one of them landing in unrelated code, because parallel
> lanes edited those files the same day:
>
> | §8 citation | where the content is now |
> |---|---|
> | `jdk-only-kind-map-25-linux.tsv:283-291` | `:379-387` |
> | `classloading/src/class_manager.rs:10824` | `:11198` |
> | `native-builtins/src/classloader.rs:13004/:13028/:13110` | gone (see below) |
> | `native-builtins/src/phases_early.rs:18236-18320` | `:18545-18560` |
> | `native-builtins/src/jca/provider_chain.rs:4043` | not at that line |
> | `native-builtins/src/phases_late/concurrent.rs:8566-8577` | `:8951` |
>
> §8's own last bullet says *"line citations rot fast here"*. It is now
> demonstrated **on this record, on the day it was written**. The half-life is
> hours, not weeks. Treat every line number in this directory as a hint and grep
> the marker text.
>
> **CLOSED — W7-20's frozen baseline.** The bullet says nine
> `LinkedListSnapshotListItr` rows *"must flip `synthetic-stub` → `bridge`"* and
> calls it build-blocking. `scripts/baselines/jdk-only-kind-map-25-linux.tsv:379-387`
> already reads `bridge` on all nine, and the file's own header comment (`:51`)
> now documents the nine rows. Nothing to do.
>
> **CLOSED — W4-1's four unit tests aimed at dead code.**
> `native-builtins/src/classloader.rs:1587` and `:9173` both record
> *"DELETED 2026-08-12: … `enforce_lookup_access` … never-registered dead"*. The
> predicate and its tests are gone; the bullet describes a file state that no
> longer exists.
>
> **REFUTED — W7-16's residual did not "get worse", it was fixed.** The bullet
> says *"the carrier still implements no interfaces
> (`classloading/src/class_manager.rs:10824`), so an erased `(ListIterator) x`
> throws `ClassCastException` — and now does so in strict mode too"*. At
> `:11198` the arm exists and reads
> `"cratonvm/internal/LinkedListSnapshotListItr" => &["java/util/ListIterator", "java/util/Iterator"]`,
> with a comment naming W7-16 and W7-20 and explaining that it reaches the
> `ClassOrigin::VmInternal` door as well as the compatibility one — i.e. it
> closes exactly the strict-mode half the bullet says is open. This is the first
> §8 bullet found **wrong in the dangerous direction** (claiming open work that
> is done *and* mis-describing the current source), which is the direction §3
> reports as never having occurred. §3's *"understated: 0"* row is a claim about
> status LINES; §8's prose bullets are not covered by it and should not inherit
> its confidence.
>
> **STANDS — the fourth W2-2 async-close surface.**
> `native-builtins/src/phases_early.rs:18560` still registers
> `java/net/SocketInputStream` (the comment at `:18545` calls itself *"the only
> registrations of `java/net/SocketInputStream` in the tree"*), and
> `:18287` still mints one with `try_alloc_concurrent_synthetic`. Still
> *plausible*, still settled by one `--dump-native-registry`.
>
> **STANDS — §6's W6-12 quote.** *"ORDER IS THE CONTRACT HERE … probing the
> policy first (the variant recorded in W6-12) would mint the class one call
> earlier"* is verbatim in `phases_late/concurrent.rs:8951`. The prescription is
> still the one that must not be applied.
>
> **§3's arithmetic is a snapshot, not a standing claim.** The directory held 62
> records when §3 was computed and holds **102** now — the same day. The 58/30
> split cannot be re-derived from a later tree and should not be quoted as a
> current rate.
>
> ### A finding for the next pass: a *comment* can be the stale record
>
> This record's whole subject is a status line that outlived its truth. The same
> failure occurs one layer down, in source comments that justify a deletion, and
> it is worse there because there is no index to reconcile against.
>
> `native-builtins/src/lang_invoke.rs` (in `register_p68_invoke_extras`) deletes
> the `MethodHandleProxies.wrapperInstanceType` registration and explains:
>
> > *"The registration deleted here keyed on `(Ljava/lang/Object;)Ljava/lang/Class;`,
> > but the real `MethodHandleProxies.wrapperInstanceType(Object)` returns a
> > `MethodType` … so the key was permanently unmatchable against real JDK
> > bytecode."*
>
> `javap java.lang.invoke.MethodHandleProxies` on JDK 25.0.3.9 says
> `public static java.lang.Class<?> wrapperInstanceType(java.lang.Object)` — the
> method has returned `Class<?>` since Java 7. **The deleted key was the correct
> one**, and the comment's reasoning is inverted. Nominated for the lane that
> owns `native-builtins/`; the fixture that asks the question is
> `regression-suite/src/RJdkProxyIface.java`'s `wrapperRoundTrip` step
> (`wrapperInstanceType(g) == Greeter.class`).
>
> Also for the next pass: `MethodHandleProxies.asInterfaceInstance`'s
> `--jdk-only` failure (`ClassFormatError: ldc: unsupported constant pool entry
> type at #26`) was flagged by `STUB-CENSUS-20260812.md` §8 as needing *"its own
> record"* and had none. It was a constant-pool decoder gap, is fixed in
> `vm/src/runtime/interpreter/constants.rs`, and is adjudicated against the
> nearest-looking record in
> `W7-9-minted-interface-abstract-methods.md`'s 2026-08-12 re-verification block
> (verdict: separate root cause).

---

> **VERIFIED AGAINST A BINARY 2026-09-04. Both items §7 flagged "to whoever
> schedules the next run" have now been run.** This is a bookkeeping record —
> *"It closes nothing in the VM and opens nothing in the VM"* — so what it owed
> was never a defect measurement. It owed the two build-and-run gaps it named.
>
> **§7 item 1: *"No `--features synthetic-jdk` binary has ever been built"***.
> One was, on 2026-09-03, and four probes were run on it:
>
> ```text
> CloseFlushSwallowProbe    --synthetic-jdk   9 failed / 13 differing   (shipping arms: 4 / 6)
> DirectByteBufferStateProbe --synthetic-jdk  30 differing              (shipping arms: 23)
> ```
>
> The class of claim §7 describes — *"scoped to that configuration and have
> therefore never been observed at all, only reasoned about"* — now has
> observations, and they were not decorative: `W7-70`'s headline defect
> (`PrintStream.close()` losing bytes on disk) is fixed on both shipping arms
> and **still live under `--synthetic-jdk`**, which is a divergence only that
> binary can see.
>
> **§7 item 2: *"The Linux and non-Windows arms of `native-io/src/process.rs`
> have never been compiled"***. They have now, on Linux:
>
> ```text
> cargo test -p cratonvm-native-io process    25 passed; 0 failed
> cargo test --workspace                      17,974 passed
> ```
>
> W6-10's five widened signatures type-check on this host. §7's stated hazard —
> *"a type error there is invisible until someone builds on a different host"* —
> did not materialise.
>
> **A qualification to §3's headline, which this campaign is in a position to
> make.** §3's finding is that all thirty wrong status lines erred the same way,
> *"claimed more open work than exists"*, with **0** in the direction of
> claiming a fix that had not landed — and it draws from that the conclusion
> that the cost is *"purely wasted effort rather than a correctness risk"*.
>
> That measured whether the PATCH was in the tree. It could not measure whether
> the patch WORKS, because nothing was run. Running them changes the picture:
>
> ```text
> H13-2 §2   ca8f03069 is an ancestor of HEAD; the defect reproduces
> G9-1       fixes landed; final-sigma still wrong in 3 of 768 checks
> W7-58      fix landed; bb_state's direct-buffer arm confirmed live
> G10-1      code landed; one doubleValue row still returns a non-double
> ```
>
> Each of those is "fixed in source, still broken in fact". §3's asymmetry
> survives on its own terms — no record claimed an *unlanded* patch had landed —
> but "under-retire, never overclaim" holds for patch presence and **not** for
> defect closure, and the second is what a reader of a status line assumes.
>
> **What this does NOT verify.** §§1-6 are git and source archaeology over 58
> records and none of it was re-derived; the thirty corrected status lines were
> not re-audited. The ADDENDUM's rotted-anchor table is a 2026-08-12 measurement
> and its line numbers have certainly moved again. §7's other seven questions are
> untouched.

## 1. The defect being fixed

`docs/known-issues/jdk-only/` is the campaign's evidence base and its work
queue. Those two roles are in tension, and the tension resolved badly:

Lanes were routinely scoped to a subset of files. A lane that found a defect in
a file it did not own wrote the patch down verbatim under a heading like
`## Out-of-file patch (not applied)` and handed it off. The hand-off then landed
— usually within hours, in a commit whose message named it — and **nobody went
back to edit the originating record.** The heading kept saying "not applied"
forever.

The same thing happened one step up, at the status line. Records written by a
lane that could not run `cargo` say "FIXED in source, not yet verified against a
binary". The verification later happened, in a suite run or a sibling lane's
measurement, and the status line stayed.

The result is an index that lists finished work as pending. That is not a
cosmetic problem. It was measured on 2026-08-12:

* An agent was handed nine "verbatim patches, recorded and NOT applied" from
  W6-9-complete-erases-the-abnormal-record.md §8 and
  W3-4-forkjointask-status-flags-and-the-eager-default.md. **All nine were
  already in the tree.** Commit `e643b5893`, titled *"apply the four cross-file
  patches wave-1 lanes could not reach"*, landed W6-9 §8 in full **hours after
  W6-9 was last edited**. The record was handed out as pending work a day later.
* A second agent was handed three inherited access-enforcement records —
  W4-1-publiclookup-allowedmodes-never-checked.md,
  W4-2-unnamed-accessor-bypasses-encapsulation.md and
  W6-8-method-invoke-exports-gate.md. **All three were already fixed.**
* A third re-ran five records' vectors and found all five green in both modes.

Three runs, most of each spent re-deriving completed work.

## 2. Method

For each record: read the status line and every concrete claim of the form
"X is not applied", "X is missing", "residual Y is live", "fix PARTIAL". For
each claim, decide the truth in the current tree, by:

* `git log -S'<distinctive literal from the patch body>' --oneline` — a patch
  that landed names its commit this way in one command. This is the highest-yield
  technique by a wide margin and it is what the next pass should reach for first.
* grep for the identifiers, comments or predicates the patch introduces. **A
  patch that introduces `pub fn os_parent_pid` and produces zero hits tree-wide
  did not land**; a patch whose three-line predicate appears verbatim did.
* `git merge-base --is-ancestor <commit> HEAD` before believing any commit hash.

Then split the record three ways — **headline closed**, **residual closed**,
**residual open** — rather than the binary the old index used.

**Adjudicate every row, not just the alarming one.** W4-2's loud rows were all
stale while its quiet one was live. That is the same shape a source-only audit
hit earlier in this campaign, and it is why nothing here was retired on a
headline.

## 3. The arithmetic

| | count |
|---|---|
| Records in the directory | 62 |
| Owned by other running lanes, not touched (`W4-4`, `W6-5`, `W7-41`, `W7-43`) | 4 |
| **Records checked** | **58** |
| Records whose status line was materially wrong | **30** |
| — direction: **overstated how much was open** | **30** |
| — direction: understated (claimed fixed, was not) | **0** |
| Records open **purely** as bookkeeping — nothing live in them at all | **5** |
| Records carrying at least one live residual | **53** |
| Out-of-file patches claimed unapplied that **are** in the tree | **18** |
| Prescribed fixes that are wrong or superseded and must not be applied | **9** |
| Claims that cannot be settled without a build or a run | **9 distinct questions** |

**The direction is the finding.** Thirty status lines were wrong and every single
one was wrong the same way: it claimed more open work than exists. Not one record
claimed a fix that had not landed. Whatever else is true of this campaign, its
records do not overclaim — they under-retire. That asymmetry is what makes the
cost purely wasted effort rather than a correctness risk, and it is also why it
went unnoticed for so long: nobody was ever burned by acting on a wrong status,
only by re-doing work.

## 4. The five records that were open purely as bookkeeping

Everything in them is closed and evidenced. Left in place rather than `git mv`'d
into the internal record tree, because retirement is the orchestrator's call and
a reconciliation pass should not be the thing that moves files out of the public
evidence base.

| Record | Why nothing is left |
|---|---|
| `W6-2-module-serviceloader-provider-factory.md` | Headline verified 44/44 both modes; no out-of-file patch was ever needed; its two "deliberately NOT done" items are argued refusals, not unfinished work; and the question it left open — *"where `--jdk-only` stops next on this vector"* — is answered: it does not stop. |
| `W7-4-differential-probe-widening-round-2.md` | Its whole deliverable was "someone run the CratonVM side". W7-32 did, then W7-33/36/37/40 acted on it. |
| `W7-11-strict-baseline-remeasured.md` | Closed at 68/0 the day it was written. |
| `W7-28-preview-classfile-gating.md` | All four handback parts applied. See §6 — this had the most misleading status line in the directory. |
| `W7-32-round-2-differential-run.md` | A measurement record, superseded by W7-40 (96 → 43 → 14 divergences). |

## 5. The eighteen "not applied" patches that were applied

Each is now marked in place in its own record, with the commit.

| Record | The patch | Landed in |
|---|---|---|
| `L16` (both guard patches) | array-descriptor `forName` must throw CNFE | `f88feaef1` |
| `W6-9` §8.1–§8.4 | four cross-file ForkJoinTask edits | `e643b5893` |
| `W6-9` §7.5 | `complete(v)`'s `setRawResult` on a cancelled task | `e643b5893` |
| `W3-4` §3 | the eager-fork default flip | `256d119b4` |
| `W4-1` (two hardening patches) | `lk_drop_lookup_mode`, `lk_in_method` | `dcfe77cb8` |
| `W4-2` (`ServiceLoader` residual) | `grant_reflective_override` | `b3aca74c8` |
| `W6-8` (three residuals + the contradicted test) | unreflect gate, `Field.get`, `new19_module_access` | `3644142d5`, `dcfe77cb8`, `b3aca74c8` |
| `W2-1` residual 1 | `LinkedListSnapshotListItr` through the VM-internal door | `6ae3ca634` |
| `W2-1` residual 2 | `ArrayDeque`'s spare ring slot | `fddf67650` |
| `W5-2` amendment | `ProcessHandle$Info.commandLine()` registered | `0ab1067ec` |
| `W7-2` §7.1 | the "REQUIRED" one-line wiring | `4752a00a4` |
| `W7-5` §6.2 | the narrowed stream registrar (folded, not by name) | `1fcbd9060` / `4752a00a4` |
| `W7-12` (both hunks) | the annotation carrier's VM-internal door | `5266bf8c7` |
| `W7-15` patches 1 and 3 | `SecretKeySpec` twin deleted; ChaCha20 generalised | `911ddb84b`, `3a594f304` |
| `W7-16` (both hunks) | retag + VM-internal mint, landed together as required | `6ae3ca634` |
| `W7-17` §6 hunk A | `HttpServerLoop` through the VM-internal door | `a8b5342a5` |
| `W7-18` patch A | `jla_start_in_container` keeps the container (fallback form) | `4c9482908` |
| `W7-21` patches B and D | ChaCha20 spec handling; `%02x` zero pad | (in tree), `c3f7b2d78` |
| `W7-23` handback | `Thread.exit()` on the terminating thread | `c3da9455d` |
| `W7-25` §6.1 and §6.2 | `LogManager` re-kinded; `RJdkLogging` scheduled | `4eaa5d321`, `3b20b83b5` |
| `W7-27` §10A | `CRATONVM_THREAD_CONTAINERS` declared through the flag layer | `78a0428ef` |
| `W7-28` (all four parts) | `--enable-preview` wired end to end | `de9bedeef`, `6ce65f98a` |
| `W7-29` (required companion edit) | `certificate_factory_p68` stops passing a null | (in tree) |
| `W7-33` R2 | `RuntimeError::EmptyStackException` | `aab87e003` |

Plus five records whose "not yet verified against a binary" caveat is
discharged by vector runs taken 2026-08-12 on the dev binary at `ba65f1a19`:
`L8` and `W4-3` (`RJdkSecurity`, 61 checks), `L16` (`RJdkFailure`, 43),
`W5-1` and `W6-6` (`RJdkJni`, 35), `W2-3` / `W4-2` / `W6-2` (`RJdkModule`, 44) —
each **in both `--jdk-only` and `--real-jdk`**. W6-2 had recorded its vector
walking 1 → 4 → 14 → 20 → 26 → ~30 of 44; it is now 44/44.

## 6. The second failure mode: a prescription that outlived its observation

Nine records prescribe a fix that is now wrong. In every case the *observation*
was right and was acted on — by someone who then chose a **better** fix and did
not come back. The residue is a patch block that reads exactly like pending work
and would be a regression if applied. These are more dangerous than a stale
"not applied", because acting on one does damage rather than merely wasting time.

All nine are catalogued in `README.md` §2.4 and marked in place. Three worth
naming here:

* **W4-3 Patch E** says to delete the `CHACHA20` / `CHACHA20POLY1305` arms and
  drop `AES/KW` and `AES/KWP` from the SunJCE seed list, because nothing
  implements them. All four have since been implemented for real (`29429b755`
  and neighbours). Applying Patch E verbatim would remove working RFC 8439 and
  RFC 5649 code **and break the ratchet test at
  `native-builtins/src/jca/provider_chain.rs:4043`.**
* **W3-6's out-of-file patch** asks for five `pub`s, a new `p60_handle_stream`
  and four rewritten bodies. None of those identifiers exists anywhere in the
  tree; the defect was instead fixed by delegating to the real
  `ProcessHandleImpl` (`0ab1067ec`), which passes its exceptions through
  untouched — a strictly better answer. Exactly one line of that patch landed
  verbatim.
* **W6-12's out-of-file patch** offers two variants. Both were considered and
  **rejected on the merits**, with the reasoning written into the source:
  `native-builtins/src/phases_late/concurrent.rs:8566-8577` says *"ORDER IS THE
  CONTRACT HERE … probing the policy first (the variant recorded in W6-12) would
  mint the class one call earlier."*

`W7-28` deserves its own line as the worst *status* offender: it still read
*"the switch that turns it off is NOT WIRED"* while all four parts of its
handback were in the tree, landed by `W7-31`, whose own status line correctly
said so. Two records, adjacent in the same directory, disagreeing about the same
four commits.

## 7. What this pass could NOT settle

Nine questions need a build or a run. They are listed with their exact commands
in `README.md` §2.6 and repeated in each record's status block. **None of them is
written anywhere as "probably fixed"** — an unverified guess in a status line is
what produced this record.

The two worth flagging to whoever schedules the next run:

* **No `--features synthetic-jdk` binary has ever been built** for the records
  that need one (`W6-12`, `W7-10`, `L8`). Several residuals in this directory are
  scoped to that configuration and have therefore never been observed at all —
  only reasoned about. That is a whole class of claim standing on source reading.
* **The Linux and non-Windows arms of `native-io/src/process.rs` have never been
  compiled** by any lane that touched them (`W5-2`, `W6-10`). W6-10's finding 4
  widened five signatures across those arms. A type error there is invisible
  until someone builds on a different host.

## 8. Findings for other people (this pass changed no `.rs` or `.java` file)

* **A fourth surface of the W2-2 async-close species, PLAUSIBLE not confirmed.**
  `native-builtins/src/phases_early.rs:18236-18320` registers
  `java/net/SocketInputStream` `read()I` and `read([BII)I` against the same
  `s2_registry()`, parks in a bare `read_retry_eintr` with no close-awareness and
  no registry re-ask, and maps `Ok(0)` to `-1` where `Socket.close()` mandates an
  exception — the exact pre-fix shape. Only *plausible* because the class name is
  the pre-JDK-13 spelling JDK 25 does not declare, and it is absent from
  `native-api/src/no_image_receiver.rs`, so it is probably reachable only in a
  `synthetic-jdk` build. One `--dump-native-registry` settles it. Recorded in
  W2-2-blocked-reader-async-close-wakeup.md.
* **W4-1 has four unit tests aimed at dead code** —
  `native-builtins/src/classloader.rs:13004`, `:13028`
  (`lk_find_virtual_private_method_with_public_lookup_throws`) and `:13110` all
  assert against `enforce_lookup_access`, which no live path reaches. They are
  green and they guard nothing.
* **W7-20's two frozen baselines are stale against the tree and the ratchet is
  slack-free**, so this is build-blocking whenever someone next runs it: nine
  `LinkedListSnapshotListItr` rows in
  `scripts/baselines/jdk-only-kind-map-25-linux.tsv:283-291` must flip
  `synthetic-stub` → `bridge`.
* **W7-16's residual got worse as a side effect of its own fix landing.** The
  carrier still implements no interfaces (`classloading/src/class_manager.rs:10824`),
  so an erased `(ListIterator) x` throws `ClassCastException` — and now does so
  in strict mode too, because the retag and the VM-internal mint landed while the
  `jdk_interfaces` arm did not.
* **W4-3's five live patches and W7-29's five live residuals are the same five
  defects seen from two ends.** Fix them once.
* **Line citations rot fast here.** L8's are the worst instance found: its
  registrations moved from `crypto_impl.rs:1336` to `:1408-1425`, and its two
  `lib.rs` anchors moved by roughly 200 and 2,200 lines. The patch *text* still
  applied verbatim in every case — only the anchors moved. This is why the
  README tells you to anchor on the marker tag.

## 9. The rule that would have prevented all of this

**If you land a hand-off patch, edit the originating record in the same commit.**

`e643b5893` is the cleanest example of the failure and of how cheap the fix
would have been. Its commit message already names what it is doing — *"apply the
four cross-file patches wave-1 lanes could not reach"* — so the author knew
exactly which records the patches came from. One additional hunk per record,
in the same commit, would have saved three agent-runs.

The secondary rule, for readers rather than writers: **`git log -S` before you
believe a record.** One command, one literal from the patch body. It is now the
first bullet of `README.md` §3.
