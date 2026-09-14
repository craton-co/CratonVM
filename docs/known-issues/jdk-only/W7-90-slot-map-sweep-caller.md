# W7-90 — the slot-map sweep had no caller; wiring it, and what it still cannot see

Status: `cratonvm_native_api::read_alias::verify_declared_slot_maps` now has two
triggers. Until this lane it had **none**, while seven `SlotMap`s — W7-76's
`java/nio/ByteBuffer`, W7-77's `Month`/`StringJoiner`/`Method`/`Thread`, W7-75's
`Continuation`/`ForkJoinPool` — were published to it. A detector with no caller
is indistinguishable from one reporting all-clear, which is this campaign's
dominant species; it had taken up residence inside the instrument built to
detect that species.

> **RE-VERIFIED 2026-08-12, and this record's own §2.2 argument lands on it.**
> Both triggers are present in the tree and correct — `vm-cli/src/main.rs:4281`,
> `vm/src/vm/vm_init.rs:7965`, `native-builtins/src/lang_system.rs:137` with its
> three call sites at `:1354`, `:2628`, `:2664`, and links 7–9 of
> `native-api/tests/read_alias_coverage.rs`. HANDOFF-20260812.md's
> *"`verify_declared_slot_maps` still has no caller"* and W7-77's §7.2 as
> originally written are **stale**, not open.
>
> Two things in this record are, however, wrong as written, and both are the
> kind of wrong it warns about elsewhere:
>
> * **"Seven `SlotMap`s" is now EIGHT.** W7-88 published
>   `SSC_P58_SLOT_MAP` (`java/nio/channels/ServerSocketChannel`,
>   `native-builtins/src/phases_late/net_channels.rs:55`). §4.6.1 and the
>   corrected §4.7 totals are below. The population is a moving number and every §4 figure
>   in this file is a snapshot; count `declare_slot_map(` before quoting one.
> **THE THIRD DOOR IS NOW WIRED (2026-08-12, later pass).** `§2.2.1`'s two
> patches are applied and link 10 is in the tree:
> `native-builtins/src/lang_system.rs`'s helper is `pub(crate)`, and all three
> `ForkedBooter` bodies in `native-builtins/src/test_frameworks.rs` call it with
> their own trigger label immediately above their `std::process::exit`
> (`:3684`→`ForkedBooter.acknowledgedExit`, `:3710`→`ForkedBooter.exit1`,
> `:3735`→`ForkedBooter.exit`). Link 10
> (`the_surefire_exit_paths_sweep_before_they_terminate`) was simulated RED on
> all three bodies before the patch, so it goes green **because** of it, not
> alongside it. §2.2.1 and §7 below are updated in place; the sweep now has
> **seven** exit-path call sites plus the launcher, and a Surefire fork is no
> longer silent.
>
> * **§2.2 closed three of four doors, and the fourth is the one it named.**
>   §2.2's whole argument is that `System.exit` is how "SbRunner, Surefire's
>   `ForkedBooter`, every Spring Boot application" ends — and a **Surefire fork
>   does not reach `System.exit` at all** on this VM. Four triples on
>   `org/apache/maven/surefire/booter/ForkedBooter` are registered from
>   `register_essential_natives_with_shims` (`native-builtins/src/lib.rs:10758`,
>   `:10764`, `:10770`, `:10776` — LIVE in **both** modes) onto three bodies in
>   `native-builtins/src/test_frameworks.rs` that each end in their own
>   `std::process::exit` (`:3684`, `:3710`, `:3735`) and never call
>   `native_system_exit`. §2.4 has the measurement and the patch; it is **not
>   applied here** — `test_frameworks.rs` is out of this lane's file set.
>   This is `reference_classpath_exclusion_leaked_through_four_process_wide_lookups`
>   arriving one layer out, in the paragraph that cites it.

> **RE-VERIFIED 2026-08-12 (lane A1, `--jdk-only` field-updater lane). STAYS
> OPEN.** Source-level only; this lane may not invoke `cargo` and ran nothing.
> The wiring claims all hold; the record stays open on §6 and §7, which are
> about what the instrument still cannot see and have not moved.
>
> **Every call site claimed above is present, but every line number in the
> block above it has drifted.** Anchor on the symbol:
>
> | claimed | actually at, today | symbol to anchor on |
> |---|---|---|
> | `vm-cli/src/main.rs:4281` | `:4282` | `vm.sweep_declared_slot_maps("main-returned")` |
> | `vm/src/vm/vm_init.rs:7965` | `:7961` (doc comment) | `Vm::sweep_declared_slot_maps` |
> | `native-builtins/src/lang_system.rs:137` | `:150` | `pub(crate) fn sweep_declared_slot_maps_before_exit` |
> | its call sites `:1354`, `:2628`, `:2664` | `:1480`, `:2754`, `:2790` | labels `"System.exit"`, `"Runtime.exit"`, `"Runtime.halt"` |
> | `test_frameworks.rs:3684`, `:3710`, `:3735` | `:3690`, `:3724`, `:3751` | labels `"ForkedBooter.acknowledgedExit"`, `"ForkedBooter.exit1"`, `"ForkedBooter.exit"` |
>
> The helper really is `pub(crate)`, all three `ForkedBooter` bodies really do
> call it with their own label, and links 7–10 are all four in
> `native-api/tests/read_alias_coverage.rs` (`:589`, `:673`, `:734`, `:809`).
> §2.2.1's "NOW WIRED" and §7's "DONE" are accurate. This is the third time
> a fixed-line-band citation in this family has gone stale within a day; the
> `linebands` lesson applies to *records*, not only to source-witness tests.
>
> **The map population is still EIGHT** — re-counted today, `declare_slot_map(`
> outside `native-api/src` and outside tests matches exactly 8 sites:
> `jdk25_concurrency.rs:2058` (`SYNTHETIC_THREAD_SLOT_MAP`),
> `lang_reflect.rs:2021` (`METHOD_LEGACY_SLOT_MAP`),
> `phases_early.rs:11523` (`MONTH_SLOT_MAP`),
> `phases_late/concurrent.rs:8376` (`NEW15_CONT_SLOT_MAP`) and `:8656`
> (`NEW15_FJP_SLOT_MAP`), `phases_late/net_channels.rs:78`
> (`SSC_P58_SLOT_MAP`), `native-collections/src/lib.rs:30409`
> (`SJ_STUB_SLOT_MAP`), `native-io/src/lib.rs:5691` (`BB_SLOT_MAP`). §4.7's
> corrected eight-map row is therefore still the current figure, and §4.6.1's
> addition is the most recent one. The instruction to *"count
> `declare_slot_map(` before quoting one"* was followed rather than trusted, and
> it came back the same.
>
> **§6.10 has not moved and is what keeps this record open.** Nobody has yet run
> `CRATONVM_DBG_LAYOUT_ALIAS=1` over a workload, so every row in §4 is still a
> prediction and not a transcript. Nothing in this pass changes that, and no
> claim here should be read as evidence that the sweep produces the predicted
> census — only that the code that would produce it is wired.

**Nothing here was built or run.** This lane writes code, tests and docs; the
orchestrator builds. The three new gates were re-implemented outside the tree
and run against the real worktree and against six mutated copies (§5). The one
thing executed is `javap -p` against Eclipse Adoptium 25.0.3.9 on this Windows
host — the same oracle and convention as W4-4-slot-index-species-sweep.md,
W7-49-slot-index-recensus.md, W7-59-layout-detector-coverage.md and W7-69.

Branch `fix/verify-declared-slot-maps-caller-20260812`.

**Nothing this lane reports is repaired here.** §4 enumerates the 29 predicted
rows for follow-up lanes and stops. A lane that wires an instrument and then
gets lost repairing its output delivers neither, and W7-75 §7.4 and W7-77 §5.5
both state in code that these maps publish a **belief, not the truth**,
deliberately — so the rows are expected and must not be silenced by editing a
declaration to agree with `javap`.

## 1. The defect

`native-api/src/read_alias.rs` has two entry points. `observe_read` is wired at
six call sites in `native-io` and fires on a real receiver. `declare_slot_map` /
`verify_declared_slot_maps` is the other half: a native publishes its
`const F_x: usize = k` table as `(slot, field name)` pairs and the whole table is
swept against the loaded class. Publication was wired — link 6 of
`native-api/tests/read_alias_coverage.rs` gates it and is green. **Reading the
registry was not.**

Three lanes met this and each closed with the same residual, in the same words:

| record | its own residual |
|---|---|
| W7-69 §7.2 | *"`verify_declared_slot_maps` has no caller. The sweep exists, is tested, and is published to by `register_io_natives` — but nothing calls it yet. Choosing its trigger … needs a build, and wiring it blind would be a call nobody has seen run."* |
| W7-76 §8.3 | *"now it matters more: the map this lane completed is swept by a function nothing calls, so the three `wrong-field` rows §5.3 predicts are a prediction, not a transcript."* |
| W7-77 §7 | *"Four more maps are now published to a sweep nobody invokes … the four rows' `SlotMap`s are checked only [by a source gate], not that the sweep ever runs."* |

Link 6 is the exact shape of the trap: it proves a map reaches the registry and
says nothing about anything reading it. `rows == 0` from a sweep nobody calls and
`rows == 0` from seven maps that agree with their classes are opposite findings
and the census could not tell them apart.

## 2. Where the trigger went, and why

The constraint is the one W7-69 §2 established and it is a property of this tree,
not a matter of taste: **the class must be loaded when the sweep runs**, and at
*registration* time most are not. `vm/src/vm/vm_init.rs` calls
`class_manager.bootstrap_core_classes()` before it registers natives;
`java.nio.DirectByteBuffer` is package-private, is not on that list, and is
loaded on demand, so its `declared_fields` comes back empty — and `declared == 0`
is the overload `layout_alias`'s own module header records as *unmeasured, not
cleared*. A registration-time sweep inherits that hole and enlarges it from a
corner case to most of the population. Registration is also last-write-wins, so
it would report overwritten triples as defects.

That rules out both of the two "free" options the task offered (at registration,
or at VM init once the boot classes are up). The third — behind the flag on
first use — is worse: it puts work on the read path the instrument is supposed to
observe, and "first use" is the moment *least* likely to have the classes
loaded.

So: **after the workload**, at every point the process can leave.

### 2.1 Trigger A — the launcher, immediately after `main(String[])` returns

`vm-cli/src/main.rs`, in `run()`, between `phase_exec.end()` and the
`catch_unwind` result match:

```rust
if cratonvm_native_api::layout_alias::enabled() {
    let _ = vm.sweep_declared_slot_maps("main-returned");
}
```

Four reasons, in the order they decided it:

1. **It is the point in the process with the most classes loaded.** Everything
   the workload touched is still there; nothing has been torn down.
2. **It is above the `match result`,** so a workload that *panicked* out of
   `main` still produces its census. That is the same argument the
   missing-natives dump twenty lines below already makes for being
   unconditional: a crashing program is exactly when you want the census.
3. **It is where every other self-gated census in this launcher already
   prints** — `site_stats::dump`, `scan_prof::dump`, `define_census::dump`,
   `dump_method_stats_to_stderr`. A reader who turns on a debug flag looks at
   shutdown stderr.
4. **A `NativeContext` exists there for free.** `Vm::sweep_declared_slot_maps`
   (`vm/src/vm/vm_init.rs`) builds the `NativeContextImpl` from `&self.shared`
   and `&mut self.main_thread`, exactly as `begin_main_thread_blocking_region`
   twelve lines above it does. No new plumbing, no fabricated thread.

### 2.2 Trigger B — the three self-terminating natives

`native-builtins/src/lang_system.rs`, one shared helper called from
`native_system_exit`, `native_runtime_exit` and `native_shutdown_halt0`,
immediately before `std::process::exit`:

```rust
fn sweep_declared_slot_maps_before_exit(ctx: &dyn NativeContext, trigger: &str) {
    if cratonvm_native_api::layout_alias::enabled() {
        let _ = cratonvm_native_api::read_alias::sweep_declared_slot_maps_at(ctx, trigger);
    }
}
```

Trigger A alone would have been a lane that closed its own residual and left the
hole where it matters. **`System.exit` never reaches the launcher line**, and
that is how nearly every fixture this instrument is aimed at ends: SbRunner,
Surefire's `ForkedBooter`, every Spring Boot application. A sweep wired only to
the return path reports nothing on exactly the runs that would produce a real
transcript — link 7's failure wearing a different hat, which is why all three
natives are wired and not just the first. Closing three of four doors has looked
identical to closing none in this repo before
(`reference_classpath_exclusion_leaked_through_four_process_wide_lookups`).

Placement inside each native is **after** its soft-return escape hatches
(`CRATONVM_SOFT_EXIT`, the `ForkedBooter.exit(1)` guard), so a soft-returned exit
does not consume the census the launcher would print later — `already_reported`
dedupes on `(class, slot, expected, site)`, so a sweep that ran early would leave
the launcher's printing `rows=0`.

`native_shutdown_halt0`'s first parameter was `_ctx` and is now `ctx`. That is
the only signature-adjacent change in the lane and it is a rename, not a
signature change.

### 2.2.1 Trigger C — the Surefire fork. **NOW WIRED (2026-08-12).**

**Status: APPLIED.** The section below is kept in its original diagnostic voice
because the argument is the deliverable; the two patches it proposed are in the
tree, and "**Not fixed here**" further down now reads "fixed in a later pass on
this branch". What changed:

* `native-builtins/src/lang_system.rs` — the helper is `pub(crate) fn`, with the
  reason (seven `std::process::exit` sites, not three) written into its doc
  comment so the next reader of `lang_system` learns about `test_frameworks`
  without having to find this record.
* `native-builtins/src/test_frameworks.rs` — one
  `crate::lang_system::sweep_declared_slot_maps_before_exit(&*ctx, "…")` per
  body, immediately above the `std::process::exit`, **below** each body's
  `nbflags().soft_exit` early return, each carrying its own trigger label. The
  shared helper is reused rather than a fourth gate open-coded, because the
  no-`else` property link 9 asserts lives in that helper and a copy would not
  inherit it.
* `native-api/tests/read_alias_coverage.rs` — link 10, plus a correction to the
  module header's item 9, which read as though `System.exit` were the whole
  self-terminating population.

`&*ctx` reborrows the bodies' `&mut dyn NativeContext` as the `&dyn
NativeContext` the helper takes — the same expression `lang_system.rs:1354`
already uses.

**One body serves two triples.** `native_surefire_forkedbooter_exit1` is
registered for both `exit()V` (`lib.rs:10764`) and `exit1()V` (`:10776`), so the
four registrations reach **three** bodies and carry **three** labels. Reading
three labels as three triples, or four triples as four bodies, both get the
arithmetic wrong.

### 2.2.1.1 The original finding, as filed

`std::process::exit` in the native crates has **seven** call sites, not three.
The four §2.2 does not cover are all on one class:

| registered at | triple | body | ends in |
|---|---|---|---|
| `lib.rs:10758` | `ForkedBooter.acknowledgedExit()V` | `native_surefire_forkedbooter_acknowledged_exit` | `exit(0)` `test_frameworks.rs:3684` |
| `lib.rs:10764` | `ForkedBooter.exit()V` | `native_surefire_forkedbooter_exit1` | `exit(1)` `:3710` |
| `lib.rs:10770` | `ForkedBooter.exit(I)V` | `native_surefire_forkedbooter_exit_code` | `exit(code)` `:3735` |
| `lib.rs:10776` | `ForkedBooter.exit1()V` | `native_surefire_forkedbooter_exit1` | `exit(1)` `:3710` |

All four sit in `register_essential_natives_with_shims`
(`native-builtins/src/lib.rs:7103`).

`register_essential_natives_with_shims` runs in **both** modes, so these are not
the synthetic-only population — they are the live teardown of every Surefire
fork this VM runs. Registration is the gate on the cold and reflective paths, so
real `ForkedBooter` bytecode never runs and `System.exit` is never reached: on a
Surefire fork the launcher's post-`main` line is skipped **and** trigger B is
skipped, and the sweep prints nothing at all.

That matters more than the arithmetic suggests. §2.2's justification for wiring
all three of `lang_system`'s natives rather than one was *"a sweep wired only to
the return path reports nothing on exactly the runs that would produce a real
transcript"*, and it named Surefire as the case. The `--jdk-only` corpus's
Spring Boot and Tomcat fixtures go through Surefire, so the fixture population
this instrument exists for is precisely the population still uncovered.

~~**Not fixed here** (`test_frameworks.rs` and `lang_system.rs` are other lanes'
files).~~ **Fixed in a later pass on this branch — see the status block at the
top of §2.2.1.** The patch is two edits, and it deliberately reuses the existing
helper rather than open-coding a fourth gate — the shared helper is what link 9
asserts has no `else`:

1. `native-builtins/src/lang_system.rs:137` — `fn` → `pub(crate) fn`.
2. `native-builtins/src/test_frameworks.rs` — one call immediately above each of
   the three `std::process::exit` lines, **below** each body's
   `nbflags().soft_exit` early return, carrying its own trigger label
   (`"ForkedBooter.acknowledgedExit"`, `"ForkedBooter.exit1"`,
   `"ForkedBooter.exit"`). Per-call labels, not the helper's name: link 9's own
   §5.1 lesson is that a name-only predicate stays green when two of three call
   sites are deleted.

A fourth link (`the_surefire_exit_paths_sweep_before_they_terminate`) belongs
beside links 7–9 and is written out in §7. **It is now link 10 in that file.**

### 2.3 Cost, and Compatible mode

**No behaviour changes in any mode, with the flag on or off.** Every trigger is
`if layout_alias::enabled() { … }` with **no `else`**, the return value is
printed and dropped, and `verify_declared_slot_maps` re-checks the flag before it
touches a class, a name or a lock. With the flag off the added cost is one
relaxed `OnceLock` load and one predictable branch, **once per process, on a
teardown path** — there is no hot path here at all, which is a weaker claim to
have to make than the six `observe_read` sites had to.

The sweep itself, with the flag on, is 7 maps × 38 slots of class-metadata reads.
It allocates no Java object, takes no VM lock the emitter does not already take,
and never loads or fabricates a class: `class_id_by_name` returning `None` is
counted as `unresolved` and skipped.

**No `CRATONVM_*` flag was added.** `CRATONVM_DBG_LAYOUT_ALIAS` is reused, so
none of `types/src/flag_groups.rs`, `types/tests/flag-surface.txt`,
`docs/flag-tokens.md` or `docs/config/flag-inventory.md` needs a change and
`cargo test -p cratonvm-types` is unaffected. Link 2 of
`read_alias_coverage.rs` keeps it that way and was re-checked green after this
change — which mattered, because that gate scans the file with comments stripped
and **string literals kept**, so naming the flag in the new summary line would
have turned it red. The summary says "the layout-alias debug flag" instead.

### 2.4 `usize` → `SweepReport`

`verify_declared_slot_maps` returned the row count. Three different findings
render as `0` under that signature — *the flag is off*, *nothing was ever
published*, *everything published agrees* — and a fourth hides inside a non-zero
one (*most maps' classes were never loaded, and the rows you see are from the
two that were*). It now returns

```rust
pub struct SweepReport { ran, maps, resolved, unresolved, slots, rows }
```

and `SweepReport::summary_line()` prints all of it:

```
[read-alias] declared slot-map sweep at main-returned: maps=7 resolved=5 \
  unresolved=2 slots=32 rows=24 (unresolved maps name a class no loader has: \
  UNMEASURED, not clean)
```

`unresolved` is the run-time twin of `SlotAnswer::Unknown` and is the number this
record most wants a future reader to look at first. Nothing in the workspace
consumed the old `usize`, so this breaks no caller — which is itself the defect
this lane is closing, stated as an API fact.

## 3. The oracle

`javap -p` against Eclipse Adoptium 25.0.3.9, transitive over the superclass
chain, `static` excluded, declaration order within a class — the convention W4-4,
W7-49, W7-59, W7-69, W7-75 and W7-77 all use.

**Four of the seven layouts were re-derived here rather than copied**, by a
chain-walking scanner written for this lane, and **all four reproduce exactly**:

| class | fields | agrees with |
|---|---:|---|
| `java.nio.ByteBuffer` | 11 — `mark(0) position(1) limit(2) capacity(3) address(4) segment(5)` from `java.nio.Buffer`, then `hb(6) offset(7) isReadOnly(8) bigEndian(9) nativeByteOrder(10)` | W7-58, W7-59, W7-69 — three prior routes |
| `java.lang.reflect.Method` | 20 — `override(0) accessCheckCache(1)` (`AccessibleObject`), `parameterData(2) declaredAnnotations(3)` (`Executable`), then `clazz(4) slot(5) name(6) returnType(7) parameterTypes(8) exceptionTypes(9) modifiers(10) …` | W7-77 §1 |
| `java.lang.Thread` | 19 — `eetop(0) tid(1) name(2) interrupted(3) contextClassLoader(4) holder(5) …` | W7-77 §1, and W7-69 §4.3's "5 vs 19" |
| `java.util.StringJoiner` | 7 — `prefix(0) delimiter(1) suffix(2) elts(3) size(4) len(5) emptyValue(6)` | W7-77 §1 |

Four independent reproductions with zero disagreements is the reason the
remaining three — `Month` (W7-77 §1), `Continuation` and `ForkJoinPool` (W7-75
§1) — are cited rather than re-derived a fourth time. Each of those records
states the same oracle and says it re-derived rather than copied. A lane that
disagrees with a row below should re-run `javap -p` before editing anything:
four censuses in this area were each wrong about some rows on 2026-08-12, and
the `ForkJoinPool` row in particular turns on `AbstractExecutorService`
declaring **no** instance field, which is the one number a careless count gets
wrong.

## 4. The expected census — enumerated, not repaired

Predicted from the published maps × the layouts above. **This is a prediction,
not a transcript**; nothing was run.

Every predicted row is `wrong-field`. **Not one `absent-slot` row is expected**:
every slot in every published map is inside the real class's width (Method's
highest is 12 against 20 fields; `ByteBuffer` 6 against 11; `Continuation` 4
against 10; `ForkJoinPool` 1 against 16; `StringJoiner` 4 against 7; `Thread` 5
against 19; `Month` 0 against 3). An `absent-slot` row in a real transcript is
therefore **new information** and should be read as such.

### 4.1 `java/nio/ByteBuffer` — 3 rows of 8 slots (W7-76)

`BB_SLOT_MAP`, `native-io/src/lib.rs BB_FIELD_*`. Declared from
`register_io_natives`, which both arms of `vm_init`'s fork call.

| slot | the map believes | the real class has | verdict |
|---:|---|---|---|
| 0 | `hb` | **`mark`** | wrong-field — the calibration case |
| 1 | `position` | `position` | clean |
| 2 | `limit` | `limit` | clean |
| 3 | `capacity` | `capacity` | clean |
| 4 | `mark` | **`address`** | wrong-field |
| 4 | `address` | `address` | clean — the deliberate non-firing control |
| 5 | `segment` | `segment` | clean |
| 6 | `offset` | **`hb`** | wrong-field |

Exactly the three the task predicts and the three `BB_SLOT_MAP`'s own doc comment
predicts. Slot 4 appearing twice with two beliefs is deliberate (W7-76): `slots`
is a slice, not a map, and `already_reported` keys on the expected name, so the
correct belief and the incorrect one are two entries and only one prints.

### 4.2 `jdk/internal/vm/Continuation` — 5 rows of 5 slots (W7-75)

`NEW15_CONT_SLOT_MAP`, declared from `register_new15_continuation` ←
`register_new15_loom`, reached in **both** arms.

| slot | map | real | note |
|---:|---|---|---|
| 0 | `scope` | **`target`** | swapped pair — both references, both resolve |
| 1 | `target` | **`scope`** | swapped pair |
| 2 | `state` (int) | **`parent`** (`Continuation`) | |
| 3 | `pin` (int) | **`child`** (`Continuation`) | |
| 4 | `preempted` (int) | **`tail`** (`StackChunk`) | |

### 4.3 `java/util/concurrent/ForkJoinPool` — 2 rows of 2 slots (W7-75)

`NEW15_FJP_SLOT_MAP`, declared from `register_new15_forkjoinpool_common`, both
arms. Real `parallelism` is index **15**.

| 0 | `parallelism` (int) | **`termination`** (`CountDownLatch`) |
| 1 | `active` (int) | **`saturate`** (`Predicate`) |

### 4.4 `java/lang/reflect/Method` — 11 rows of 12 slots (W7-77)

`METHOD_LEGACY_SLOT_MAP`, declared from `register_wp2_1_natives`, both modes.
Every slot disagrees except `exceptionTypes`(9).

| slot | map | real |
|---:|---|---|
| 0 | `clazz` | **`override`** |
| 1 | `name` | **`accessCheckCache`** |
| 2 | `returnType` | **`parameterData`** |
| 3 | `modifiers` | **`declaredAnnotations`** |
| 4 | `slot` | **`clazz`** |
| 6 | `override` | **`name`** |
| 7 | `parameterTypes` | **`returnType`** |
| 8 | `callerSensitive` | **`parameterTypes`** |
| 9 | `exceptionTypes` | `exceptionTypes` — **clean** |
| 10 | `annotations` | **`modifiers`** |
| 11 | `parameterAnnotations` | **`signature`** |
| 12 | `annotationDefault` | **`annotations`** |

### 4.5 `java/util/StringJoiner` — 3 rows of 5 slots (W7-77)

`SJ_STUB_SLOT_MAP`, declared from `register_string_joiner_natives_with_category`,
both modes.

| 0 | `delimiter` | **`prefix`** — swapped pair |
| 1 | `prefix` | **`delimiter`** — swapped pair |
| 2 | `suffix` | `suffix` — clean |
| 3 | `elts` | `elts` — clean |
| 4 | `emptyValue` | **`size`** (int) |

### 4.6 The two synthetic-only maps — 5 rows of 6 slots (W7-77)

`MONTH_SLOT_MAP` (`register_phase52_time_enums`) and
`SYNTHETIC_THREAD_SLOT_MAP` (`register_jdk25_concurrency_natives`) sit under
`register_synthetic_overrides`, so **in real-JDK mode they are never declared and
these rows can never appear.** In synthetic mode they *are* declared, and the
class the loader has is the fabricated one — so what the sweep answers there is
"has the fabricated layout drifted from what these constants believe", which is
exactly the drift that put the virtual flag at slot 4 until 2026-08-05. Against
the **real** JDK classes the maps would read:

| `java/time/Month` 0 | `value` (int) | **`name`** (`Enum.name`, a `String`) |
| `java/lang/Thread` 0 | `name` | **`eetop`** (long) |
| `java/lang/Thread` 1 | `priority` | **`tid`** (long) |
| `java/lang/Thread` 3 | `target` | **`interrupted`** |
| `java/lang/Thread` 4 | `contextClassLoader` | `contextClassLoader` — **clean** |
| `java/lang/Thread` 5 | `isVirtual` | **`holder`** |

### 4.6.1 `java/nio/channels/ServerSocketChannel` — 4 rows of 4 slots (W7-88)

**Added to this section 2026-08-12.** `SSC_P58_SLOT_MAP`,
`native-builtins/src/phases_late/net_channels.rs:55`, declared from
`register_p58_nio_channels` ← `register_phase58_natives` ← `lib.rs:23973`, which
is inside `register_synthetic_overrides` — so like `Month` and `Thread` this map
is **never declared in real-JDK mode** and these four rows can only appear under
`--synthetic-jdk`. The real transitive layout is 10 fields:
`closeLock(0) closed(1) interruptor(2) interruptedTarget(3)` from
`AbstractInterruptibleChannel`, then `provider(4) keys(5) keyCount(6) keyLock(7)
regLock(8) nonBlocking(9)` from `AbstractSelectableChannel` — cited from that
map's own doc comment, which states the same `javap -p` oracle and convention.

| slot | map | real |
|---:|---|---|
| 0 | `open` | **`closeLock`** |
| 1 | `bound` | **`closed`** — the flag a real `isOpen()` reads |
| 2 | `fd` (int) | **`interruptor`** (`sun.nio.ch.Interruptible`) |
| 3 | `socket` | **`interruptedTarget`** |

Four of four disagree and none is out of range (highest slot 3 against a width
of 10), so §4's "not one `absent-slot` row is expected" survives the addition.

### 4.7 Totals

**Corrected 2026-08-12** — the seven-map figures below the line are what this
lane wrote and are kept so a reader comparing transcripts can see the shift.

| population | maps | slots | wrong-field rows | clean |
|---|---:|---:|---:|---:|
| all **eight**, against the real JDK 25 classes | 8 | 42 | **33** | 9 |
| the five declared in **real-JDK** mode | 5 | 32 | **24** | 8 |
| the **three** synthetic-only, against the real classes | 3 | 10 | 9 | 1 |

| ~~as written 2026-08-12, before `SSC_P58_SLOT_MAP`~~ | ~~7~~ | ~~38~~ | ~~29~~ | ~~9~~ |
|---|---:|---:|---:|---:|
| ~~the two synthetic-only~~ | ~~2~~ | ~~6~~ | ~~5~~ | ~~1~~ |

The real-JDK-mode row is unchanged, and that is the point of splitting it out:
every map added since this lane landed has been synthetic-only, so the figure a
Compatible-mode run can produce has not moved.

**A run reports a subset of its mode's figure, and `unresolved` names the
difference.** A trivial `Hello, world` in real-JDK mode is unlikely to have
loaded `StringJoiner`, `Continuation` or `ForkJoinPool`, so it should print
something like `maps=5 resolved=2 unresolved=3` — and `unresolved=3` is the whole
reason the summary line carries it. Reading `rows=14` there as "the other maps
are clean" is the mistake this line exists to prevent.

## 5. Proving the RED

Three new gates in `native-api/tests/read_alias_coverage.rs`, links 7–9, each its
own test so a break names which link broke.

7. **`the_declared_slot_map_sweep_has_a_caller`** — some file outside
   `native-api/src` calls `sweep_declared_slot_maps_at` or
   `verify_declared_slot_maps`, with comments stripped. **Fails** the moment the
   funnel is unwired again, which is precisely the state the tree was in for the
   whole of 2026-08-12.
8. **`the_post_main_sweep_runs_after_the_workload`** — `vm-cli`'s `run()`
   contains `vm.sweep_declared_slot_maps(` and it sits **below** the
   `main(String[])` invoke. **Fails** when the call is deleted, and when a
   refactor lifts it above the invoke — where it silently becomes the
   registration-time check W7-69 §2 rejected while still printing a census.
9. **`the_exit_paths_sweep_before_they_terminate`** — `native_system_exit`,
   `native_runtime_exit` and `native_shutdown_halt0` each call the helper
   **carrying their own trigger label**, before their `std::process::exit`; and
   the shared helper's `if layout_alias::enabled()` block has no `else`.
   **This link enumerates three natives by name and is green on a tree where
   four other registered natives exit without sweeping** — §2.2.1. A gate that
   lists its own population cannot report that the population was wrong, which
   is the same shape as link 6 and is why link 10 is written out in §7.

The existing six links were re-checked green against the changed tree, links 1–3
mechanically (§5.2).

### 5.1 Simulated red, and the two design choices the simulation forced

This lane cannot run `cargo`. All three predicates are text scans, so all three
were re-implemented outside the tree — including `strip_comments`, `match_brace`
and `fn_body` byte for byte — and run against the real worktree and against six
mutations.

| arm | link 7 | link 8 | link 9 |
|---|---|---|---|
| the tree as landed | GREEN | GREEN | GREEN |
| **the tree as it was before this lane** (both triggers reverted) | **RED** | **RED** | **RED** |
| the launcher's call moved *above* the `main` invoke | GREEN | **RED** | GREEN |
| one of the three exit sweeps deleted (`Runtime.halt`) | GREEN | GREEN | **RED** |
| an `else` added to the exit helper's gate | GREEN | GREEN | **RED** |
| both triggers replaced by comments that name the sweep | **RED** | **RED** | GREEN |

The second row is the one that matters most: **all three gates are red on the
pre-change tree.** That is the strongest available form of "prove the RED first"
— the failure they exist to catch is not hypothetical, it is the state this
repository was in an hour ago.

Two predicates were written the way they are *because* of earlier simulations in
this file, and both would otherwise have been vacuous:

* **Link 9 matches each call together with its own trigger string literal**, not
  by the helper's name. Three natives share one helper, so a name-only predicate
  stays green when two of the three are deleted. This is link 5's lesson
  applied before paying for it a second time: `bb_resolve_heap_array` held a
  second `observe_read`, and deleting the calibration one left both `find` and
  `rfind` satisfied.
* **Link 7 strips comments and excludes `native-api/src` entirely.** Six `///`
  lines in the native crates name `verify_declared_slot_maps`, and
  `read_alias.rs` itself contains the definition, its doc links and the
  wrapper's own internal call — a predicate that counted either would be green
  on a tree where nothing calls anything. The last mutation row proves it:
  replacing both triggers with TODO comments that spell the function name leaves
  link 7 red.

### 5.2 The existing links, re-checked

* Link 1 (one detector) — the only file in `vm`, `gc`, `native-api`, `types`,
  `jit` and the six native crates emitting a `wrong-field` / `absent-slot`
  direction is still `native-api/src/read_alias.rs`. The new summary line is
  deliberately **not** a second emitter: it carries no `direction` field, it is
  a denominator, and it is produced by the same module.
* Link 2 (no new flag) — `read_alias.rs` contains no `CRATONVM_` in code after
  comment stripping, and still gates on `layout_alias::enabled()`. Re-checked
  because the new `summary_line` adds string literals, which that predicate does
  **not** strip.
* Link 3 (the oracle walks the chain) — `field_name_at` untouched, still
  contains `super_of(` and `declared_at(`.
* Links 4, 5, 6 — no `observe_read` call site, no calibration site and no
  `SlotMap` declaration was touched by this lane.

## 6. What the sweep still cannot see

The number that must **not** silently improve because a caller ran: W7-69 §4.1
counted **11,948 constant-slot accesses in the native crates, 6 of them
observed.** This lane did not move either figure. The sweep covers a different
and much smaller population — the **38 slots in 7 published `SlotMap`s** — and
those two censuses answer different questions. A future reader who sees a sweep
printing 24 rows and concludes the read-side census now has coverage will be
wrong by three orders of magnitude.

1. **11,942 of 11,948 constant-slot reads still state no expected field.** The
   sweep can only see slot maps that have been *converted* to a `SlotMap`: 7 of
   the 337 slot-map `const` runs W7-69 §4.2 counted. The other 330 name no
   field, and 244 of them name no class either. Unchanged.
2. **The sweep is keyed on a class NAME, not on a receiver.** It asks what
   `java/nio/ByteBuffer` declares; the natives run with `HeapByteBuffer`,
   `DirectByteBuffer` and `ByteBufferAsIntBufferL` receivers. Inherited slots are
   stable (fields are laid out superclass-first in declaration order), so the
   check is **sound but partial** — it cannot reach a slot past the named class's
   own width, and it can say nothing about a native registered on an interface.
   Only `observe_read` answers the receiver question.
3. **`class_id_by_name` returning `None` is two answers** — "no loader has it"
   and "several do, ambiguously". Both count as `unresolved`. The sweep gives up
   on both, which is what that method's own doc says a caller that will not act
   on a miss should do, but it means `unresolved` is not purely "not loaded".
4. **A guard is a runtime predicate and the sweep is not.** `Method`'s 11 rows
   and `StringJoiner`'s 3 are correctly guarded (`method_class_has_named_layout`,
   `sj_read_elements_real`) and will still print — the sweep reports that the map
   disagrees with the class, never whether the code holding the map is reached
   with that receiver. **Guarded is not clean, and a printed row is not a bug.**
5. **`Unknown` is still one answer to two questions** at slot granularity: a
   class with zero declared instance fields is indistinguishable from one that is
   not loaded. Inherited from `layout_alias`; closing it needs a
   `class_is_loaded` predicate `NativeContext` does not have.
6. **The census is mode-dependent, and the two synthetic-only maps can never
   report against a real class.** `Month` and `Thread` are declared only under
   `register_synthetic_overrides`. Conversely, a clean sweep in synthetic mode
   is evidence about the *fabricated* layouts and nothing else.
7. **A hard exit can lose the rows but keep the summary.** The per-slot rows go
   through `tracing::warn!`; `std::process::exit` does not unwind and does not
   flush a subscriber. The summary is an `eprintln!` for exactly that reason. The
   fix is not a second emitter — link 1 exists to prevent that — it is to prefer
   trigger A when a workload can be made to return.
8. **The dedup makes a second sweep in one process print `rows=0`.**
   `already_reported` keys on `(class, slot, expected, site)` and is
   process-global. Triggers A and B are mutually exclusive in practice, but a
   soft-returned `System.exit` reaches B and then A, and A will print `rows=0`
   with a non-zero `resolved`. That combination means "already reported above",
   not "clean".
9. **Embedders have no trigger.** `libcratonvm`, `cratonvm-embed` and the JNI
   Invocation API (`DestroyJavaVM`) are unwired. Only the `cratonvm` / `java`
   launcher, `lang_system`'s three exit natives and — since the later pass of
   2026-08-12 — `test_frameworks`'s three `ForkedBooter` bodies sweep. A
   JNI-embedded run reports nothing
   — and reports it silently, which is the same shape as the defect this lane
   closed, one layer out.
10. **Everything in §4 is a prediction.** The first real product of this
    instrument is still a `CRATONVM_DBG_LAYOUT_ALIAS=1` run over a workload,
    which nobody has done — for this lane, W7-69, W7-75, W7-76 or W7-77. That
    run is now one command and it is the single highest-value next step.
11. **Last-write-wins is settled only at the registrar level.** A map declared
    from a registrar the arm calls is swept; whether a later registrar overwrites
    the specific triples that read those slots is per-triple and needs a build.
    W7-77 §2.1 found one map (`util_time.rs`'s second `MONTH_FIELD_VALUE`) that
    is dead by call order alone and publishes nothing.

## 7. For follow-up lanes

Nothing below was repaired here.

* ~~**Trigger C — the four `ForkedBooter` exit triples (§2.2.1).**~~
  **DONE 2026-08-12.** Both edits and the gate are in the tree; see the status
  block at the top of §2.2.1. The gate below is reproduced as landed, so a
  future reader can see what shape it took — it is link 10 of
  `native-api/tests/read_alias_coverage.rs` and was simulated **RED on all three
  bodies** before the patch:

  ```rust
  /// LINK 10. `System.exit` is not the only way a fixture leaves. Four triples on
  /// `ForkedBooter` are registered from `register_essential_natives_with_shims`
  /// — LIVE in both modes — onto three bodies that each end in their own
  /// `std::process::exit` and never reach `native_system_exit`. A Surefire fork
  /// therefore skips the launcher's post-`main` line AND link 9's three natives,
  /// which is the population W7-90 §2.2 named as its whole reason for existing.
  ///
  /// Each call is matched WITH its own trigger label, not by the helper's name:
  /// three bodies share one helper, so a name-only predicate stays green when two
  /// of the three are deleted (§5.1).
  #[test]
  fn the_surefire_exit_paths_sweep_before_they_terminate() {
      let src = strip_comments(&read("native-builtins/src/test_frameworks.rs"));
      // The label is matched WITH its quotes, exactly as link 9 does: an
      // unquoted "ForkedBooter.exit" is a prefix of "ForkedBooter.exit1".
      for (body, label) in [
          (
              "native_surefire_forkedbooter_acknowledged_exit",
              "\"ForkedBooter.acknowledgedExit\"",
          ),
          ("native_surefire_forkedbooter_exit1", "\"ForkedBooter.exit1\""),
          ("native_surefire_forkedbooter_exit_code", "\"ForkedBooter.exit\""),
      ] {
          let b = fn_body(&src, body)
              .unwrap_or_else(|| panic!("{body} not found in test_frameworks.rs"));
          let sweep = b.find("sweep_declared_slot_maps_before_exit(").unwrap_or_else(|| {
              panic!(
                  "{body} terminates the process with std::process::exit and never \
                   sweeps. A Surefire fork skips the launcher trigger AND the three \
                   lang_system natives, so the declared slot-map sweep prints nothing \
                   on exactly the fixtures W7-90 section 2.2 was written for."
              )
          });
          assert!(
              b[sweep..].contains(label),
              "{body}'s sweep call must carry its own trigger label {label:?}; three \
               bodies share one helper and a name-only match stays green when two of \
               the three calls are deleted"
          );
          let exit = b.find("std::process::exit").unwrap_or_else(|| {
              panic!("{body} no longer exits — re-derive this gate before deleting it")
          });
          assert!(sweep < exit, "{body} sweeps AFTER it has already exited");
      }
  }
  ```

  `fn_body`, `strip_comments` and `read` are already in that file, used by links
  7–9. Simulated against the tree as it stands: **RED on all three bodies**,
  which is the failing observation, not a prediction.
* **Embedders still have nothing** (§6.9), unchanged.

* The 24 real-JDK-mode rows of §4.1–§4.5. `Continuation` and `ForkJoinPool` were
  given name-first resolution by W7-75 and `ByteBuffer` slot 0 by W7-58, so those
  rows now describe a fallback rather than a live defect — but the fallback is
  still reachable on a receiver whose by-name resolution fails, and only a
  transcript can say whether one exists.
* `ForkJoinPool` slots 0 and 1 remain the GC-visible pair: an `Int` written into
  a slot the collector scans as an oop (`termination`, `saturate`). Highest
  severity in the table.
* Trigger coverage for embedders (§6.9) — one call in `libcratonvm`'s teardown
  and one in `DestroyJavaVM`, both needing a `NativeContext` that path does not
  currently hold.
* Splitting `unresolved` into "not loaded" and "ambiguous" (§6.3), which needs
  `NativeContext` to expose more than `class_id_by_name`.
