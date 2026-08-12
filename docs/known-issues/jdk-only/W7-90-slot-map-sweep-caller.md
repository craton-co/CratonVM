# W7-90 — the slot-map sweep had no caller; wiring it, and what it still cannot see

Status: `cratonvm_native_api::read_alias::verify_declared_slot_maps` now has two
triggers. Until this lane it had **none**, while seven `SlotMap`s — W7-76's
`java/nio/ByteBuffer`, W7-77's `Month`/`StringJoiner`/`Method`/`Thread`, W7-75's
`Continuation`/`ForkJoinPool` — were published to it. A detector with no caller
is indistinguishable from one reporting all-clear, which is this campaign's
dominant species; it had taken up residence inside the instrument built to
detect that species.

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

### 4.7 Totals

| population | maps | slots | wrong-field rows | clean |
|---|---:|---:|---:|---:|
| all seven, against the real JDK 25 classes | 7 | 38 | **29** | 9 |
| the five declared in **real-JDK** mode | 5 | 32 | **24** | 8 |
| the two synthetic-only, against the real classes | 2 | 6 | 5 | 1 |

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
   launcher and the three exit natives sweep. A JNI-embedded run reports nothing
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
