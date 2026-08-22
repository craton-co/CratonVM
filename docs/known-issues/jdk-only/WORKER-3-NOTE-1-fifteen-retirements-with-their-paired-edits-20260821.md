# WORKER-3-NOTE-1 — 15 registrations retired with every paired edit that makes them safe, four live wrong answers fixed, two vectors that ask questions nothing asked before

**Status: MEASURED.** Three acceptance arms plus a census arm, two clean release
builds (control at `22cb4338d`, branch at this tip), a
`--dump-native-registry --explain-jdk-only --jdk-only` dump from each, a
triple-granularity registry diff between them, and a nine-image `javap` sweep.
WORKER 3, 2026-08-21, Linux build host. Companions: `WORKER-3-NOTE-2` (the
sweep), `WORKER-3-NOTE-3` (the StringBuilder mechanism and the refusals).

`H25` adjudicated all 214 registrations in three of this lane's four files,
retired **zero**, and produced six refusals — every one of them blocked on a
file it did not own or on a measurement it could not take. This lane owns
`lang_invoke.rs` as well (**337 registrations**, not 214), and the Linux build
host carries the nine JDK images the sweep needed. **Every one of `H25`'s
blocked refusals is resolved here, in the direction the evidence pointed —
which for four of the six was KEEP.**

---

## 1. Acceptance

MEASURED on the Linux build host, `HEAD 409f5f630`, working tree clean
(`git status --porcelain` empty at the start of the run), against a clean
release build of this branch and a clean release build of the base commit
`22cb4338d` as the control. `JDK=/data/toolchain/jdk-25`, `TIMEOUT=600`.

| arm | control @ `22cb4338d` | **this branch** | the brief's figure (Windows) |
|---|---|---|---|
| `CRATONVM_ARGS=--jdk-only` | 104 / 105 | **107 / 107** | 105 / 105 |
| `SUITE=all` | 103 / 105 | **106 / 107** | 104 / 105 |
| `SUITE=core` | 64 / 65 | **67 / 67** | 65 / 65 |

These three are the A/B that attributes this lane's delta, both arms
taken before H0's tip moved. **The state that actually lands is in
§1.0: 107/107, 107/107, 67/67, zero failures in every arm.**

**The denominators moved because this branch adds two vectors** (§4), 65 → 67
and 105 → 107. Nothing was removed from any list.

**The control is not 105/105, and that is the first thing this lane measured.**
`RJdkOptionalShape` fails on Linux at the base commit, in all three arms, on
`ProcessHandle.Info.arguments()` — and it is the ONLY difference between this
host's control and the brief's figures. §3.4 is its cause and it is fixed here.
A lane that had not run the control first would have credited that pass to
itself or blamed its own diff for the failure.

The one remaining failure, `RJdkFunctionCombinators` on `SUITE=all`, is H0's —
`H24-3` diagnosed it to `register_comparator_natives` in
`native-collections/src/lib.rs` — and it fails identically on the control.

### 1.0 Re-verified on the MERGED state, after H0's tip moved

`claude/jdk-only-mode-handoff-09b48c` advanced by four commits while this lane
was working, and one of them (`ecd4f56e1`) closed `RJdkFunctionCombinators` —
the last standing `SUITE=all` failure — by guarding the `java/util/Comparator`
family on a real image. H0's tip was merged into this branch (`b74c73269`,
clean auto-merge; the only file both sides touched is `run.sh`, and the two
edits are in different blocks), and everything was rebuilt and re-run.

**MEASURED on the merged state, `HEAD b74c73269`, clean tree:**

| arm | **merged branch** | H0's tip alone (its own figure) |
|---|---|---|
| `CRATONVM_ARGS=--jdk-only` | **107 / 107, 0 failed** | 105 / 105 |
| `SUITE=all` | **107 / 107, 0 failed** | 105 / 105 |
| `SUITE=core` | **67 / 67, 0 failed** | 65 / 65 |

**Every scheduled vector passes in every arm**, which is the bar the brief
raised on 2026-08-21 (*"there is no failing set now: any red vector you produce
is yours"*). The +2 in each denominator is this lane's two vectors.

The census on the merged state, over the original 105-vector schedule: **1403
native-won / 471 bytecode-won**, `synthetic-native-registered` **1610** — the
1622 → 1610 fall is H0's Comparator guard, not this lane's, and this lane's
native-won figure is unchanged from its own control at 1403.

The registry diff was re-run on the merged binary and is byte-identical to the
pre-merge one: −15 registrations, −6 triples, **0 triples appeared, 0 slot
owners moved file**.

**And then the branch moved again, so this was done twice.** While the above was
building, WORKER 2 pushed seven commits to
`origin/claude/jdk-only-mode-handoff-09b48c` — `native-collections/src/lib.rs`
(+582: `TreeMap`'s pinned `modCount`, the view carriers' `this$0`, `Hashtable`'s
real node type and bucket sizes, CHM's real table on bulk reads) and its own
record. Disjoint from every file this lane touched, and merged clean.

**MEASURED on `d828a329d`, this lane's work ON TOP of WORKER 2's:**

| arm | result |
|---|---|
| `CRATONVM_ARGS=--jdk-only` | **107 / 107, 0 failed** |
| `SUITE=all` | **107 / 107, 0 failed** |
| `SUITE=core` | **67 / 67, 0 failed** |

Census over the original 105 vectors: **1403 / 471**, `synthetic-native-registered`
**1610** — identical to the previous merged state. Registry diff identical:
−15 / −6 / 0 appeared / **0 slot owners moved file**.

*Two fixes that each pass alone are not a tested combination* — which is why the
whole verification was repeated rather than reasoned about, both times.

**The A/B in §1 and §1.2 was deliberately NOT re-taken on the merged base.**
Attributing this lane's delta needs H0's fix in NEITHER arm; folding it into
both would add a −12 synthetic-registration movement to a comparison that
claims −9 shadowed. Two questions, two runs, and the numbers are labelled with
which is which.

**One caveat on every census figure in this record: they are LINUX figures.**
The brief publishes 1387 native-won / 481 bytecode-won from Windows; this host's
control measures 1403 / 472 at the same commit. The platform difference is real
and predates this branch — do not diff a figure here against a figure there.

### 1.1 The registry, diffed at TRIPLE granularity

A row count cannot answer the question a retirement has to answer: did any
triple lose its LAST registration, and did any slot owner change hands? Both
dumps were taken with `--dump-native-registry --explain-jdk-only --jdk-only` and
diffed.

```text
  registrations     10367 -> 10352   (-15)     exactly the 15 retired
  distinct triples   9496 ->  9490   ( -6)
  triples that APPEARED                     0
  triples that lost only a DUPLICATE        8   winner unchanged in every case
  triples whose SLOT OWNER MOVED FILE       0   <- the H22 / trap-4 check
```

The six triples that lost their last registration are the six intended ones,
and the dump's own image adjudication agrees with the reason each was retired:

```text
  AbstractStringBuilder/StringBuilder/StringBuffer.repeat(Ljava/lang/String;I)…   DECLARED NOWHERE, invocations=0
  java/lang/System.runFinalizersOnExit(Z)V                                        DECLARED NOWHERE, invocations=0
  java/lang/Thread.destroy()V                                                     DECLARED NOWHERE, invocations=0
  java/lang/Thread.stop()V                                                        real bytecode, invocations=0, WAS 2 REGISTRATIONS
```

### 1.2 The registration ratchets, A/B against the control

`native-builtins/tests/duplicate_registration_gate.rs` counts shadowed
registrations, and `H24-3` N2's rule is that a re-freeze must be MEASURED and
fully attributed. So both configurations were run at the base commit and at this
tip rather than compared against the frozen constant:

| ratchet | config | control | branch | delta |
|---|---|---:|---:|---:|
| `BASELINE_SHADOWED` | no-management | 936 | 927 | **−9** |
| `BASELINE_SHADOWED` | management (shipping) | 987 | 978 | **−9** |
| `BASELINE_SYNTHETIC_STUBS` | both | 1610 / 1621 | 1610 / 1621 | 0 |
| `BASELINE_KIND_DISAGREEMENTS` | both | 33 | 33 | 0 |

**−9 is exactly the nine shadowed losers retired** (§2.4 and §2.1's `lib.rs`
pair). Every gate passes.

**The frozen constants were NOT re-seeded, deliberately.** `BASELINE_SHADOWED_NO_MANAGEMENT`
reads `Some(1150)` in source and the base commit measures **936** — the
constant is already 214 stale, from deletions that landed before this branch.
The ratchet fires on an INCREASE, so it passes either way, and re-freezing it
here would fold 214 rows this lane cannot attribute into a commit that claims
−9. Reported instead, as `H24-3` N2's rule requires.

Two `cratonvm-native-builtins` unit tests fail on this branch and **fail
identically at the pristine base commit**, verified in a separate worktree
(§5).

## 2. The retirements, and the paired edit each one needed

**15 registrations, 11 triples.** Not one was retired on a row count; each row
below names the evidence and the thing that had to move with it.

| what | rows | why it is safe |
|---|---:|---|
| `Thread.stop()V` | **2** | Retiring the winner ALONE promotes a loser that silently interrupts the thread. Both went. |
| `Thread.stop0` duplicate | 1 | The `lib.rs` loser only; the winner is KEPT because JDK 17 declares `stop0`. |
| `Thread.destroy()V` | 1 | Declared by no supported image; the native raised the `NoSuchMethodError` the VM raises anyway. |
| `System.runFinalizersOnExit(Z)V` | 1 | Declared by no supported image; the flag it set had one writer and no reader. |
| `repeat(Ljava/lang/String;I)` ×3 classes | 3 | No image has ever declared that overload. |
| `appendCodePoint(I)` duplicate ×3 classes | 3 | The winner delegates to the loser's own function. |
| `MethodHandle.type`, `MethodHandles.lookup`, `privateLookupIn`, `Lookup.lookupClass` | 4 | `owns_slot: false` against registrars that run after them on every boot arm. |

### 2.1 `Thread.stop()V` — `H25-3` R1, the sharpest instance of trap 4 anyone found

MEASURED from the dump: the triple is registered twice.

```text
  stop ()V   native-builtins/src/lib.rs:14080            owns_slot=false
  stop ()V   native-builtins/src/deprecated_lang.rs:375  owns_slot=true
```

The **winner** throws `UnsupportedOperationException`. The **loser** calls
`ctx.thread_interrupt(target)` and **returns normally**. A census-driven
retirement of the winner — which is what the row counts recommend — turns a
method that correctly refuses into one that silently interrupts a thread, and
the census falls by one so the change scores as a win.

MEASURED across nine images: every one declares `public final void stop()`
**with a `Code` attribute**, and on JDK 21 and 25 that bytecode is

```text
  0: new  class java/lang/UnsupportedOperationException
  7: athrow
```

So retiring **both** leaves the real bytecode, which throws the same exception
the winner threw — and does so with the JDK's own (absent) message rather than
the invented `"Thread.stop() is not supported"`. Behaviour-preserving on 21/25,
a fidelity improvement, and on JDK 17 it restores the real deprecated path.

The paired edits: two registrations in two files in one commit, plus the three
places that asserted it stays registered forever (§2.5).

### 2.2 The two rows no supported image declares

`Thread.destroy()V` and `System.runFinalizersOnExit(Z)V`. `javap -p --system`
over all nine images finds neither; both were removed in JDK 11.

This is the fourth verb `H14-1` had no name for, `H25-1` sized at 342 and could
not act on, and **`WORKER-3-NOTE-2` measured down to 192**. These are the first
two rows retired on that evidence, and the multi-image part is load-bearing:
**four of the six `java/lang` rows that look identical to these on a JDK-25
census — `stop0`, `suspend0`, `resume0`, `countStackFrames` — ARE declared by
JDK 17 or JDK 21 and are KEPT.**

`RUN_FINALIZERS_ON_EXIT` went with its native: `grep -rn` found one writer (the
retired native) and no reader outside its own test. **A write-only flag set by
an unreachable native is two absences agreeing with each other.**

### 2.3 `repeat(Ljava/lang/String;I)` — a near-miss, three times

`javap -p -s --system` over the nine images:

```text
  JDK 17            no `repeat` of any descriptor on AbstractStringBuilder
  JDK 21, JDK 25    repeat(CI)   repeat(II)   repeat(Ljava/lang/CharSequence;I)
```

There has never been a `repeat(String,int)` overload. `String` implements
`CharSequence`, so `sb.repeat("x", 3)` compiles to the `CharSequence`
descriptor — which is registered on the line immediately above the one deleted.
Three registrations, `owns_slot: true`, `invocations: 0`, that could not be
named by any call site javac emits.

### 2.4 The seven shadowed losers

Three `appendCodePoint(I)` (one per class, 113 lines from their winner in ONE
registrar function) and four in `register_phase54_method_handle`. All seven
carry `owns_slot: false`; none has ever run.

They were not dead weight, they were landmines. Two of the four — the
`MethodHandles.lookup()` and `privateLookupIn` stubs — allocated a bare
`MethodHandles$Lookup` and returned it **with slot 0 never written**, which is
verbatim defect #1 in `native-builtins/tests/duplicate_registration_gate.rs`'s
own header: *"the returned Lookup's slot 0 was never written, so lookupClass()
answered null. Broke RJdkHidden AND RJdkStrict."* A lane retiring the WINNER on
a census row would have put that back into service.

The file's own header comment already stated the rule they violated: *"we do not
provide stub overrides here — the real ones take precedence."*

### 2.5 The manifests that made `H25-3` R2 a refusal

Six `deprecated_lang.rs` rows are asserted to stay registered by
`deprecated_verify.rs` and by `vm/tests/t8_deprecated_conformance.rs` — two
files in two crates, neither of them this lane's, and a retirement in
`deprecated_lang.rs` alone turns both red. That is why `H25` refused.

`deprecated_api_manifest()` now carries the column `H25-3` N2 asked for:

```rust
enum ImageStatus { Declared, AbsentFromAllSupportedImages }
```

and `t85_1_all_deprecated_apis_registered` asserts **both directions** — a
`Declared` row must be registered, and an `AbsentFromAllSupportedImages` row
must **not** be. A new test fails if the column ever becomes decorative (zero
rows in the second state). The three `t8_*_registered` tests that pinned the
retired rows now pin their ABSENCE, with the measurement in the doc comment.

The three retired rows also had to leave `jdk25_deprecated_api_checklist()`, and
that is not cosmetic: `register_missing_deprecated_shims` registers a throwing
shim for every checklist entry that is not already registered, so leaving them
would have re-registered exactly what was retired, under a different body.

## 3. The four live wrong answers

None of these is a census row. All four were found by asking a question the
corpus had never asked.

1. **`sb_state` read `count` from the wrong slot** on JDK 21 and 25 — returning
   `maybeLatin1`, a boolean, as a builder's length — while `sb_set_count`
   already wrote it by name. See `WORKER-3-NOTE-3` §2.1.
2. **`sb_ensure_capacity` DISCARDED the payload** of a real compact-layout
   builder while converting it, and could panic the process on a zero-length
   request against one. `WORKER-3-NOTE-3` §2.2.
3. **`sb_read_chars` returned empty for a real-layout builder**, and its caller
   is the constructor JDK 25's `StringBuilder.toString()` invokes — so
   `toString()` answered `""` for a builder holding text.
   `WORKER-3-NOTE-3` §2.3.
4. **Three reference arrays reported `Object[]` where the JDK declares a typed
   array**: `ClassLoader.getDefinedPackages()`, `ClassLoader.getPackages()` and
   `ProcessHandle.Info.arguments()`. The last is the SOLE reason
   `RJdkOptionalShape` fails on this host, and it is the one deviation between
   the acceptance figures in the handoff brief (taken on Windows) and this
   branch's control run.

## 4. The two vectors

Both are in `CORE_CLASSES`, so both arms schedule them and the denominators move
**65 → 67** and **105 → 107**.

* **`RStringBuilderContent`** (34 checks) — the corpus never asserted the
  CONTENT of a built string, which is why `H14-3` could price the largest
  registrar in this block at zero vectors. It found defect 3 above within a
  minute of first running.
* **`RLangPackages`** (27 checks) — `H25-3` N3 by name. `H25-3` R4 refused to
  adjudicate `i2_register_classloader_package_natives` because a grep over all
  107 vectors found no call to `getDefinedPackage`, `getPackages()` or
  `getPackage()` anywhere, so an armed run would have returned a zero carrying
  no information. It found defect 4 on its first run.

**Both vectors were wrong before they were right, and the HotSpot oracle caught
it both times** — `RLangPackages` asserted the pre-JDK-9 contract that a class
in the unnamed package has no `Package`, and then guessed `isSealed() == false`
for `java.lang`. Neither wrong assertion ever reached CratonVM. *A vector's own
premise is code that can be wrong*, and the oracle is what makes that cheap.

## 5. What this does NOT establish

* **The armed `StringBuilder` arm is not green** and this lane retired nothing
  in `register_string_builder_natives`'s catastrophic core. `WORKER-3-NOTE-3`
  §3 names the residual and the next failure.
* **Two `cratonvm-native-builtins` unit tests fail on this branch and they are
  NOT this branch's** —
  `lang_class::tests::null_receiver_on_an_instance_field_outranks_the_access_refusal`
  and
  `lang_math::tests::canonical_wrapper_if_cached_follows_the_configured_integer_bound`.
  Both were re-run at the pristine base commit in a separate worktree and fail
  identically there. Neither touches anything this lane edited (`lang_math.rs`
  is not in this lane's diff at all). Recorded rather than assumed.
* **No `--synthetic-jdk` build was made or run.** `register_deprecated_lang_natives`
  and `register_phase54_method_handle` both run on the synthetic arm, so the
  retirements affect it, and the argument that they are safe there is the same
  registrar-order argument made for the real arm — ARGUED from the call sites in
  `vm_init.rs` and `phases_late.rs`, MEASURED only on the real arm.
* **The census delta is not a proof of anything on its own.** `H22` predicted
  −24 and measured −15; this lane predicted **0** and the reasoning is in §6.
* **No JDK 17 or JDK 21 arm was run.** The images were read with `javap`; the VM
  was pointed at 25 throughout. So "JDK 17 declares `suspend0`" means the
  registration can fire there, not that its body is right when it does.
* **`RJdkFunctionCombinators` is untouched.** It is H0's, and it fails on
  `SUITE=all` here exactly as it did on the control.

## 6. The census prediction, written down before the arm ran

`H14-1` §5 and `H25-1` §3 both warn that this population is invisible to the
census, and `H22` measured a −24 prediction against a −15 result because **the
census counts shadows actually DISPATCHED, not registrations removed.**

Of the 15 registrations retired here, **every one has `invocations: 0`** and
none is a dispatched shadow: seven were `owns_slot: false` and could not be
reached at all, three name a descriptor no image declares, two name a method no
image declares, and the three `Thread.stop`/`stop0` rows sit behind a gate
nothing outside a test can open.

**Predicted census delta: 0. That is a PASS, not a failure.** A second
confound had to be removed to see it at all: this branch ADDS two vectors, and
the census is a UNION over vectors, so the headline figure rises for no source
reason. The census arm is therefore taken over exactly the original 105-vector
schedule.

MEASURED, the union census over the original 105-vector schedule:

| | control @ `22cb4338d` | this branch |
|---|---:|---:|
| `native-shadows-bytecode`, native-won | 1403 | **1403** |
| bytecode-won | 472 | 471 |
| `synthetic-native-registered` | 1622 | 1622 |
| `compatibility_classes` | 0 | 0 |

**Native-won delta: 0, as predicted.** The −1 on the bytecode-won column is not
attributed and is not claimed as an effect of this branch.

Over the full 107-vector schedule the same run reports 1413 / 476, which is the
denominator change and not a regression — **quote the 105-vector figure when
comparing against anything the brief published.**

One caveat on the 105-vector arm: the `ONLY=` list was built by scraping
`CORE_CLASSES` out of `run.sh`, which assigns that name twice, so a literal
`$PRUNED` was scheduled as a 106th class and failed to load. It ran nothing and
contributed no shadows; the census is over the 105 real vectors.

## 7. NOMINATIONS

* **N1 — `WORKER-3-NOTE-2` N1 first: reconcile the supported-image set.** Every
  retirement here is conditional on JDK 17 being supported; if it is not, four
  more `java/lang` rows become retirable and the class-granular table needs the
  same question asked of it.
* **N2 — the reference-array component type is a species, not three bugs**
  (§3.4). Three instances turned up in one lane's reach, two of them in the
  first two vectors to ask. `new_array(Reference, n)` is the wrong allocator
  wherever the JDK declares a typed array, and a grep for it against declared
  return types would find the rest.
* **N3 — retire `System$1.currentThread0()`**, registered twice in two files and
  dead on every image (`WORKER-3-NOTE-2` §3). Neither file is this lane's.
* **N4 — a lane brief should state the files and let the lane discover their
  contents.** `H25-3`'s closing correction earned that rule and this lane is the
  second data point: its brief inherited `H25`'s three files, and the fourth
  (`lang_invoke.rs`) holds 123 registrations, four shadowed landmines and four
  do-not-retire rows that no record had ever named.

---

## INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-3-NOTE-1` — 107/107, 107/107, 67/67 on the merged tip. 15 registrations retired with their paired edits (both
  `Thread.stop` copies, two rows no image declares, three near-misses, seven
  shadowed losers), four live wrong answers fixed, two corpus vectors added.
  `deprecated_api_manifest` gains the `ImageStatus` column. **CLOSED for the
  rows it names; see NOTE-3 for the StringBuilder residual.**
