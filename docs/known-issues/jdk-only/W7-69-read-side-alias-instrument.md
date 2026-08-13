# W7-69 — the read-side slot-alias instrument, and its first census

> **READ W7-77-guarded-slot-maps.md BEFORE THIS RECORD, NEVER AFTER.**
> Adjudicated 2026-08-12. The instrument is real and landed; the **census below
> is corrected in three places by W7-77 §3, which re-derived all four guarded
> rows from `javap` before touching them**, and §4.3's table still carries the
> uncorrected claims. In this record's own words:
>
> 1. **§6's `java/time/Month` sentence — "a bogus pointer for the collector to
>    mark and move" — is WRONG on BOTH layouts.** W7-77 §4: under the default
>    compact layout `write_compact_field`'s `Reference` arm takes `_ => 0` for a
>    `Value::Int`, so the Int is dropped and the slot reads back null; under
>    legacy, `for_each_ref_slot` matches `Value::Object(Some(_))` and never
>    visits an `Int` cell. It is a wrong **answer** — a nulled `Enum.name`
>    making `getValue()` answer January for every month — not heap corruption.
> 2. **§4.3's `java/lang/Thread` row is UNDERSTATED**: it lists
>    `SYNTHETIC_THREAD_VIRTUAL_SLOT`(5) alone, but the adjacent `THREAD_FIELD_*`
>    run is a separate `const` run the comment-scraper never reached, and
>    **four of five slots disagree** (`name`→`eetop`, `priority`→`tid`,
>    `target`→`interrupted`, `isVirtual`→`holder`; only slot 4 agrees, and that
>    agreement is deliberate). W7-77 §3.1.
> 3. **§4.3's `java/time/Month` row is INCOMPLETE and §4.3's "Nothing is dead in
>    this population" is FALSE.** There are **two** identically named
>    `MONTH_FIELD_VALUE` slot maps in the same crate; `util_time.rs`'s is dead —
>    all five of its triples are overwritten by `register_phase52_time_enums` —
>    and it is dead by call **order**, which nothing gates. W7-77 §2.1 and §7.3.
>
> Also treat every `native-builtins/src/lib.rs` line number in this record as
> **stale** — W7-77 §3.2 measured the drift as a per-file constant and the
> function names are the durable part.
>
> **And nobody has ever run this census.** Both entry points gate on
> `layout_alias::enabled()`, i.e. on `CRATONVM_DBG_LAYOUT_ALIAS`, and that name
> appears **nowhere** under `regression-suite/` or `ci/` — grepped 2026-08-12,
> zero hits in either tree. §7.1 says the first real product of this instrument
> will be a flagged run over a suite; that is still true, and no scheduled job
> can produce it. §4's numbers are a source-level classification and must not be
> quoted as a transcript.

Status: the second instrument W7-59-layout-detector-coverage.md §6 specified and
deliberately did not build now exists, is calibrated against the one defect of
this species known to be real, and is gated by six tests in
`native-api/tests/read_alias_coverage.rs`. **Nothing here was built or run** —
this lane writes code and docs only. Every claim about CratonVM is source-level
and says so; every claim about the real JDK layout is `javap -p` against Eclipse
Adoptium 25.0.3.9 on this Windows host, counted transitively over the superclass
chain with `static` excluded — the same oracle and convention as
W4-4-slot-index-species-sweep.md, W7-49-slot-index-recensus.md and W7-59.

The oracle was checked against W7-59's own table before it was trusted:
`AsynchronousSocketChannel` 1, `ConcurrentHashMap` 12, `java/nio/ByteBuffer` 11
with `mark(0) position(1) limit(2) capacity(3) address(4) segment(5) hb(6)
offset(7) isReadOnly(8) bigEndian(9) nativeByteOrder(10)`. All three reproduce,
including the `hb` = 6 that W7-58-bytebuffer-direct-arm.md read out of
`lib/src.zip` by a different route.

Branch `fix/read-side-slot-alias-instrument-20260812`.

## 1. The gap, restated in one sentence

`layout_alias` compares two integers at an allocation. The defect it cannot
express is a native reading slot *k* of an object it did **not** allocate, where
slot *k* on the loaded class is a different field — and W7-59 §6 gives three
independent reasons no widening of it reaches that: there is no allocation on
the path, its vocabulary is a count rather than a slot map, and its documented
fallback discriminator (intersect with the `cratonvm::gc::guard` out-of-bounds
reads) misses because **the slot exists**. An in-bounds read of the wrong field
is invisible to both halves.

The worked example, repaired by W7-58 and which nothing in this tree would have
caught: `bb_state` read slot 0 of a real `java.nio.DirectByteBuffer` expecting
the backing array `hb` and got `java.nio.Buffer.mark` = -1. The in-place comment
claiming "`hb` @ 5" was wrong twice over — 5 is `segment`, `hb` is 6.

## 2. What is actually checkable, and when

W7-59 §6 sketched the instrument as *"receiver-keyed, checking slot index
against `declared_fields` at registration time"*. Those two halves pull in
opposite directions and the lane that wrote them did not have to reconcile them.
This lane did, and **rejected registration time as the primary check** for three
reasons that are properties of this tree, not matters of taste. The task said to
treat the specification as a starting point to validate; here is the validation.

**1. At registration time the class is usually not loaded, and "not loaded" is
indistinguishable from "no fields."** `vm/src/vm/vm_init.rs` calls
`class_manager.bootstrap_core_classes()` at `:1116` — 323 named classes — and
registers natives from `:1578` onward. So a registration-time check *can* see
`java/nio/Buffer` and `java/nio/ByteBuffer`, both of which are on that list. It
cannot see `java.nio.DirectByteBuffer`, which is package-private, is **not** on
that list (grepped: the only `java/nio` entries are `ByteBuffer`, `CharBuffer`,
`Buffer`, four `charset` classes and three `file` classes), and is loaded on
demand. Its `declared_fields` would come back empty, and `declared == 0` is
exactly the overload `layout_alias`'s own module header records as *unmeasured,
not cleared*. Registration time inherits that hole and enlarges it from a corner
case to most of the population.

**2. The registered class is not the receiver's class.** A native registered on
`java/nio/ByteBuffer` is entered with a `HeapByteBuffer`, a `DirectByteBuffer` or
a `ByteBufferAsIntBufferL`. Inherited slots are stable — CratonVM lays fields out
superclass-first in declaration order (`classloading/src/class.rs`'s
`first_field_index`, `find_own_field`) — so checking the registered class is
*sound but partial*: it cannot reach any slot past the registered class's own
width, and a native registered on an interface has nothing to check against.

**3. Registration is last-write-wins, so a registration-time check reports dead
code as a defect.** A triple registered and then overwritten never runs. W7-59
spent most of its §5.3 on that distinction and settled it with "did the path
run", not "does the site exist" — the same argument `layout_alias::AllocSite`
makes for reporting Java frames instead of a `#[track_caller]` Rust location.

**The trade-off, stated rather than hidden.** Registration time is free at
steady state and needs no workload. The runtime check costs a branch on a read
path that is genuinely hot — the per-element buffer accessors would call it once
per element moved — and it reports only what a given run executed, so its census
is a **lower bound that depends on the workload**. That is a real cost and this
lane pays it, because a free instrument that answers `Unknown` for the
calibration case is not cheaper, it is useless.

### 2.1 What was built

`native-api/src/read_alias.rs`. One classifier, one emitter, two entry points —
the same architecture as `layout_alias`, for the same reason (two
implementations of one primitive drift, then disagree).

| piece | what it is |
|---|---|
| `SlotAnswer` | `Named(field)` / `Absent { width }` / **`Unknown`** — the third kept distinct from clean, and never reported |
| `field_name_at` | slot → field name, walking `declared_fields` **and** `superclass_of`, depth-bounded |
| `classify_read` | the rule, pure, no heap and no global state |
| `ReadFinding` | `WrongField { actual }` / `SlotAbsent { declared_width }` — deliberately **not** `layout_alias::Direction`, which is a direction on a count |
| `observe_read` | entry point 1: receiver-keyed, at the read, deduped on `(class, slot, expected, site)` |
| `SlotMap` + `declare_slot_map` + `verify_declared_slot_maps` | entry point 2: a native publishes its `const F_x: usize = k` table as `(slot, field name)` pairs and the whole table is swept against the loaded class |

`SlotMap` is the answer to W7-59 §6's specific complaint that those constants
have "no machine-readable link to a field name". It is `const` data: free at
runtime, and it is what makes the registration-time idea survive at all — moved
off registration to a point the caller picks, where the classes are loaded.

`SlotOracle` exists so the slot→name walk is testable against a real superclass
**chain**. `MockNativeContext::superclass_of` returns `None` unconditionally
(`native-api/src/test_mock.rs:328`), so a mock-based unit test cannot tell a
chain-walking oracle from one that stops at the receiver's own class — and the
difference is the whole calibration case.

### 2.2 Observation-only, and Compatible mode

Every entry point checks `layout_alias::enabled()` — a `OnceLock<bool>` — first
and returns before touching a class, a name or a lock. With the flag off the
cost is one relaxed load and one predictable branch. Each of the six wired call
sites is `if layout_alias::enabled() { read_alias::observe_read(..); }` with
**no `else`**, which
`every_read_side_observation_is_gated_and_observation_only` mechanises.

**Compatible mode (`--real-jdk`) sees no behaviour change in any mode, with the
flag on or off.** The return value is an `Option<ReadFinding>` that exists only
so a test can tell "clean" from "not looking"; nothing consumes it. Note that
`declare_slot_map` *is* unconditional — it runs in Compatible mode with the flag
off. That is one `&'static` pointer pushed onto a `Vec` once per registrar per
process, and it is deliberate: gating publication on the flag would leave a run
that enables the flag later with nothing to sweep, which is a detector that
reports clean because it cannot see.

**No `CRATONVM_*` flag was added.** `CRATONVM_DBG_LAYOUT_ALIAS` is reused: the
two censuses are two halves of one species, so a reader turning on "the slot
census" should get both, and a new name would need four files
(`types/src/flag_groups.rs`, `types/tests/flag-surface.txt`,
`docs/flag-tokens.md`, `docs/config/flag-inventory.md`) or
`cargo test -p cratonvm-types` goes red. `the_read_side_census_adds_no_flag_of_its_own`
keeps it that way. `docs/config/flag-inventory.md` needs no change: the flag's
home crate is still `native-api`.

**`HEADER_SIZE` is not a factor at any instrumented site.** Re-checked for the
read paths, as W7-49 and W7-59 did for the allocation ones: `native-builtins`,
`native-io` and `native-collections` contain no `heap_alloc_object`,
`HEADER_SIZE` or hand-rolled header arithmetic on a field access — every one of
the 11,948 constant-slot accesses counted in §4 addresses a field by slot index,
which the object model resolves relative to the header for the caller. That
finding still holds.

## 3. Does it fire on the calibration case?

Yes, and the claim is asserted twice from two directions, because either alone
would be a probe that cannot fail.

**The classification half**, in `read_alias.rs`'s own tests. A `ChainOracle`
carrying the real JDK 25 `DirectByteBuffer` chain — `Buffer`(6) →
`ByteBuffer`(5) → `MappedByteBuffer`(2) → `DirectByteBuffer`(2), every name from
`javap -p` — is asked for slot 0 and answers `Named("mark")`, so
`classify_read("hb", …)` is `WrongField { actual: "mark" }`. That is `bb_state`'s
historical body exactly: reading slot 0 of a real `DirectByteBuffer` and calling
it `hb`.

The test asserts the intermediate `SlotAnswer` as well as the finding, and that
is not padding: an oracle that fails to walk `superclass_of` answers
`Absent { width: 0 }` here, because `mark` is declared four classes up. The
finding would then be `SlotAbsent` — a quieter, wrong verdict that sends the next
reader looking for an allocation-width bug that is not there.

**The routing half**, in `native-api/tests/read_alias_coverage.rs`:
`the_calibration_site_is_still_observed_before_its_own_read` proves that
`native-io/src/lib.rs::bb_resolve_heap_array` — the function the repaired
fallback lives in — still contains an `observe_read` naming `BB_FIELD_ARRAY` and
`"hb"`, and that it runs **before** the `get_field(this, BB_FIELD_ARRAY)`.

**And it stays quiet where it should.** Two of the six wired sites are
deliberate non-firing controls, chosen because both are places a naive
instrument would fire:

* `bb_resolve_heap_array`'s slot-5 probe, expecting `segment`. Slot 5 really is
  `Buffer.segment`, and the probe is deliberate — `native-builtins`' typed
  buffer views stash a backing array there because it is the only Object-typed
  field `Buffer` declares.
* `bb_resolve_direct_address`'s fallback, expecting `address` at slot
  `BB_FIELD_MARK` = 4. Slot 4 really is `Buffer.address`. **The constant's name
  says `MARK` and the census still answers clean**, which is the point: the
  instrument is keyed on what the slot MEANS on the loaded class, not on what
  the constant is called.

An instrument that fires on every slot read would be worthless, and both halves
of that are now asserted.

## 4. The first census

Two populations, and they answer different questions. Both are source-level
upper/lower bounds on what the flag would print, not a transcript.

### 4.1 The whole read-side population — printed, not classified

`native-api/tests/read_alias_coverage.rs::census` counts field accesses whose
slot argument is a decimal literal or an `UPPER_SNAKE` constant — i.e. a slot map
asserting itself, as opposed to an index resolved by name at run time:

| crate | constant-slot accesses | observed today |
|---|---:|---:|
| native-builtins | 10,273 | 0 |
| native-io | 734 | 6 |
| native-collections | 941 | 0 |
| native-builtins-crypto / -security / native-awt | 0 | 0 |
| **total** | **11,948** | **6** |

(Includes `#[cfg(test)]` sites; the read-side scanner does not split them the way
`layout_alias_coverage.rs`'s does, and saying so is cheaper than a number that
looks precise and is not.)

**That gap is the honest remainder and it is printed rather than asserted.** A
gate demanding an expected field name at all 11,948 would be muted the day it
landed, and most of those reads are on objects the native allocated itself,
where its slot map is the class's own truth. What matters is that the number is
visible: a silent remainder gets read as coverage, which is the failure this
whole campaign is about.

### 4.2 The population where intent is machine-readable — classified

The subset a static instrument can check today is the slot maps that **name
their own class**. Method: collect every run of `const NAME: usize = k;` in the
six native crates, keep the runs containing a `FIELD`/`SLOT`/`IDX`/`INDEX`
constant, and take the class from the run's own doc comment.

| | count |
|---|---:|
| `const … : usize` runs in the native crates | 538 |
| of those, runs containing a slot-map constant | 337 |
| constants in those runs | 1,017 |
| runs naming a class in their own comment | 93 |
| runs whose named class **resolves in the JDK 25 image** | **36** |

Classifying the constants in those 36, excluding the ones that are widths rather
than indices (`*_COUNT`, `*_FIELDS`, `MAX_*` — those are `layout_alias`'s
species, not this one):

| verdict | constants |
|---|---:|
| slot exists on the real class — comparable | **57** |
| slot past the real class's width (`SlotAbsent`) | 3 |
| real class declares no instance fields — **`Unknown`, unmeasured** | 23 |
| width constant, not a slot index — out of scope | 21 |

Of the 57 comparable, two are excluded as an unresolved attribution (`P67_ARENA_*`,
§7.6). Of the remaining 55, **32 name a different field than the real class has
at that index** and 23 agree.

**The `java/nio/ByteBuffer` map is not among the 36 and that is the method's
sharpest limit.** Its declaration comment reads "ByteBuffer layout: 5-field
synthetic" — a bare simple name, no package — so the class-from-comment scraper
dropped it. The calibration case's own slot map is invisible to the static
census and was found by wiring the instrument, not by scanning. Three more rows,
listed first in the table below.

### 4.3 The 32 (+3), split LIVE / synthetic-only / dead

LIVE = a registrar the real-JDK arm of `vm_init.rs` calls installs the native.
Reachability was walked to the registrar and then hand-confirmed, because
W7-59's own walk was confidently wrong twice before it was right.

| class | slot map | disagreeing slots | verdict |
|---|---|---|---|
| `java/nio/ByteBuffer` *(the +3 — outside the 36)* | `BB_FIELD_*`, `native-io/src/lib.rs:7282` | 0 `hb`→**mark**; 4 `mark`→**address**; 6 `offset`→**hb** | **LIVE** — `register_io_natives`, called in **both** arms of `vm_init`'s fork |
| `jdk/internal/vm/Continuation` | `NEW15_CONT_*`, `phases_late/concurrent.rs:6865` | 0 `scope`→**target**, 1 `target`→**scope** (*swapped*); 2 `state`→**parent**; 3 `pin`→**child**; 4 `preempt`→**tail** | **LIVE** — `register_new15_loom` ← `register_essential_natives_with_shims` (`lib.rs:20348`), called at `vm_init.rs:2239` in the real-JDK arm |
| `java/util/concurrent/ForkJoinPool` | `NEW15_FJP_*`, same file `:6895` | 0 `parallelism`→**termination**; 1 `active`→**saturate** | **LIVE**, same registrar |
| `java/util/StringJoiner` | `SJ_FIELD_*`, `native-collections/src/lib.rs:29479` | 0 `delim`→**prefix**, 1 `prefix`→**delimiter** (*swapped*); 4 `emptyValue`→**size** | **LIVE** — `register_collections_natives`, both arms — but **guarded**, see below |
| `java/lang/reflect/Method` | `METHOD_LEGACY_SLOT_*`, `lang_class.rs:7625` | 11 of 12 (0 `clazz`→**override**, 1 `name`→**accessCheckCache**, …) | **LIVE but correctly guarded**, see below |
| `java/util/concurrent/Exchanger` | `EXCH_FIELD_*`, `phases_early.rs:7608` | 0 `slot`→**arena**; 1 `state`→**ncpu** | **synthetic-only** |
| `java/util/concurrent/Phaser` | `PH_*`, `phases_early.rs:7744` | 0 `parties`→**state**; 1 `arrivals`/`holder`→**parent**; 2 `phase`→**root** | **synthetic-only** |
| `java/time/Month` | `MONTH_FIELD_VALUE`, `phases_early.rs:11352` | 0 `value`→**name** (`Enum.name`, a `String` **reference**) | **synthetic-only** |
| `java/lang/ref/Cleaner` | `CLEANER_LIST`, `phases_late.rs:5575` | 0 `list`→**impl** | **synthetic-only** |
| `java/lang/Thread` | `THREAD_MIRROR_*`, `vertx_eventloop.rs:128` | 0 `name`→**eetop**; 2 `tid`→**name** | **self-allocated receiver** — the allocation-width species, already `layout_alias`'s (5 vs 19, `under`) |
| `java/lang/Thread` | `SYNTHETIC_THREAD_VIRTUAL_SLOT`, `jdk25_concurrency.rs:196` | 5 `isVirtual`→**holder** | **LIVE and correctly guarded** — see §4.4(2) |

The four **synthetic-only** verdicts all rest on the same chain, checked rather
than assumed: `register_phase51_natives` (`lib.rs:23651`),
`register_t25_natives` (`:23637`) and `register_phase69_natives` (`:23730`) all
sit inside `pub fn register_synthetic_overrides` (`:21221`–`:23978`), whose only
caller is `register_builtins` (`:21210`), which `vm_init.rs` calls at `:1580` —
the `if config.use_synthetic_jdk` arm. **Nothing is dead** in this population:
every one of the 31 is reachable from some registrar. That is the opposite
result from W7-59's allocation census, which found five genuinely dead sites,
and it is worth stating because "no dead code found" is a finding only if
somebody looked.

**Two of the LIVE rows are guarded, and the guards are the standing remedy.**

* `java/lang/reflect/Method` — every `METHOD_LEGACY_SLOT_*` write in
  `create_method_object` is inside `if !has_named_layout`, the class-side
  witness. On a real `Method` the named layout resolves and the legacy slot map
  is never applied. The constants' own comment says as much and, unusually for
  this campaign, it is true.
* `java/util/StringJoiner` — the `SJ_FIELD_*` reads sit under a comment reading
  "Legacy 5-field synthetic fallback", below a real-layout arm that resolves
  through `sj_read_elements_real`.

`java/nio/ByteBuffer`'s three are likewise reached only after a by-name read
fails, which is why W7-58 could repair `bb_state` without a mass migration.
**Guarded is not the same as clean**, and this is the census's own limit: the
guard is a *runtime* predicate, so whether it holds on every receiver is exactly
what `observe_read` answers and source cannot. The two unguarded rows —
`Continuation` and `ForkJoinPool` — are §6's first entries.

### 4.4 The 23 that agree, including two the scanner got wrong first

Worth listing, because a census with no clean rows is an instrument that fires on
everything: `java/lang/Enum` `name`(0)/`ordinal`(1); `java/lang/ref/Reference`
`next`(2); `java/util/IntSummaryStatistics` all four; `java/util/DoubleSummaryStatistics`
all six (its author derived them from `javap` and said so in the comment);
`java/nio/charset/Charset` `name`(0); `jdk/internal/vm/ContinuationScope`
`name`(0); `java/util/Random` `seed`(0); `java/lang/ProcessBuilder`
`command`(0)/`directory`(1)/`environment`(2).

Two corrections the machine pass needed, recorded because both would have been
confident false positives:

1. **`PB_FIELD_*` at `phases_late.rs:1270` is `java/lang/ProcessBuilder`, not
   `java/lang/Process`.** The comment-scraper took the class from the run's own
   doc comment, which mentions `java.lang.Process` while describing something
   else. Against `Process` the three constants read as `outputWriter`,
   `outputCharset`, `inputReader` — three wrong fields. Against `ProcessBuilder`
   they are `command`, `directory`, `environment` — exactly right. A census keyed
   on a comment is only as good as the comment.
2. **`java/lang/Thread`'s `SYNTHETIC_THREAD_VIRTUAL_SLOT` = 5 is `holder` on the
   real layout, and the site is still clean** — `vm/src/vm/vm_exec.rs`'s
   `thread_start` guards the read with
   `resolve_field_index_in_hierarchy(class_id, "eetop", …).is_none()`, a
   class-side witness, and its own comment records that the previous guard
   (`num_slots() >= 5`) was wrong because "a slot count cannot identify a
   layout". That is the exemplar of the correct remedy and the best-documented
   instance of it in the tree.

Two further misattributions are excluded rather than reported: `J25_CONFIG_*`
(commented `java/time/Duration`, actually a synthetic config carrier) and
`VIEW_BACKING_*` (commented `java/util/HashMap`, actually a dedicated synthetic
view-backing class, as its own comment says two lines later). Both show as
`SlotAbsent` against the commented class and are noise.

## 5. Proving the RED

Six gates in `native-api/tests/read_alias_coverage.rs`, each its own test so a
break names which link broke.

1. `there_is_exactly_one_read_side_detector` — exactly one file emits a
   `wrong-field`/`absent-slot` direction. **Fails** when someone re-inlines a
   second copy.
2. `the_read_side_census_adds_no_flag_of_its_own` — `read_alias.rs` names no
   `CRATONVM_*` variable and does gate on `layout_alias::enabled()`. **Fails**
   when a new flag creeps in, here rather than in `cratonvm-types`.
3. `the_slot_name_oracle_walks_the_superclass_chain` — `field_name_at` still
   contains `super_of(` and `declared_at(`. **Fails** when the walk is dropped,
   which would silently turn every inherited-field finding into `Absent`.
4. `every_read_side_observation_is_gated_and_observation_only` — every
   `observe_read` call in a native crate sits in an `if layout_alias::enabled()`
   block with no `else`. **Fails** the moment the diagnostic starts steering the
   read.
5. `the_calibration_site_is_still_observed_before_its_own_read` — §3's routing
   half. **Fails** when a hot-path cleanup lifts it out or moves it below the
   read.
6. `every_declared_slot_map_is_published` — every `SlotMap` value is handed to
   `declare_slot_map`. **Fails** when a refactor unwires one, which would make
   `verify_declared_slot_maps` sweep nothing and report clean.

Plus `census`, which prints §4.1 and asserts only that it is non-zero.
Deliberately not a ratchet, for the reason `layout_alias_coverage.rs` gives: a
count over a population that changes with every native added gets re-baselined on
sight, and a gate people re-baseline teaches them to re-baseline the file.

### 5.1 All six simulated red, and two of them earned it

This lane cannot run `cargo`. Each gate's predicate is a text scan, so all six
were re-implemented outside the tree and run against the tree and against six
mutated copies. **All six are green on the unmutated tree and red on their own
mutation** — a second emitter pasted into `vm_exec.rs`; a private
`CRATONVM_DBG_READ_ALIAS` in `read_alias.rs`; `cursor = oracle.super_of(cid)`
replaced with `cursor = None`; an `else` added to the calibration gate; the
slot-0 observation deleted; `declare_slot_map(&BB_SLOT_MAP)` removed.

The simulation paid for itself twice, and both are the reason the campaign's rule
is *a gate never seen to fail is the most common wasted effort*:

* **Gate 6 was RED on the untouched tree.** Its first predicate was "a `const`
  or `static` whose declaration mentions `SlotMap`", which matched
  `read_alias.rs`'s own
  `static MAPS: OnceLock<Mutex<Vec<&'static SlotMap>>>` — the registry the sweep
  reads. A gate that fires on an unmutated tree gets deleted, not investigated.
  The shipped predicate requires the declared type to **be** a `SlotMap`.
* **Gate 5 stayed GREEN against the exact mutation it exists for.** Deleting the
  slot-0 observation left `bb_resolve_heap_array`'s *other* observation — the
  deliberate `BB_SEGMENT_SLOT` control — in place, and both `find` and `rfind`
  on the bare call were still satisfied. The shipped predicate requires an
  `observe_read` whose text names `BB_FIELD_ARRAY` **and** `"hb"` together. This
  is the vacuous-green shape in miniature: the gate was measuring that *an*
  observation existed, not that *the* observation did.

## 6. Defects for follow-up lanes, not fixed here

Nothing below was repaired. This lane's product is the instrument and the
census; a lane that builds an instrument and then gets lost repairing its output
delivers neither. The one thing changed in a native crate is six
`observe_read` calls and a `const` slot map — additive, gated, and with no
`else`.

**Unguarded, LIVE, and new to the record:**

1. **`jdk/internal/vm/Continuation`, five slots, `NEW15_CONT_*`.** The two
   `state` reads at `phases_late/concurrent.rs:8130` and the writes at `:8117`
   are bare `ctx.get_field(this, NEW15_CONT_STATE)` with no name-first arm. On a
   real `Continuation`, slot 2 is `parent` — a `Continuation` **reference** — so
   the read falls to the `_ => NEW15_CONT_STATE_NEW` arm and the guard against
   re-running a completed continuation silently never fires. Slots 0 and 1 are
   `scope`/`target` **swapped**, which is the `AsynchronousSocketChannel` shape
   W7-49 §5 records: the most dangerous variety, because both are references and
   both resolve.
2. **`java/util/concurrent/ForkJoinPool`, two slots, `NEW15_FJP_*`.** Same
   registrar, same lane. `parallelism`(0) is `termination` and `active`(1) is
   `saturate` on the real class — a `CountedCompleter` and a `Predicate`, both
   references, both written with `Value::Int`. This is the GC-visible half of §5
   of `docs/architecture/natives-over-real-jdk-classes.md`: an `Int` in a slot
   the collector scans as an oop.
3. **`java/nio/ByteBuffer` slot 6, expecting `offset`.**
   `bb_resolve_heap_offset`'s fallback reads slot 6, which on JDK 25 is `hb` —
   the backing **array**. The `Value::Int` match is the only thing between it and
   a fabricated offset, and the by-name read above it resolves on every real
   receiver, so this is a latent row rather than a live bug. It is the third
   wrong slot in the map W7-58 corrected two of.
4. **`java/nio/ByteBuffer` slot 4, `buf_read_mark` and `buf_set_mark`.** Reading
   slot 4 as `mark` reads `address`; writing it stamps an `Int` onto a `long`
   pointer. `buf_set_mark` already saves and restores `address` around the write
   — the compensation is there, the census row names what it is compensating for,
   and a lane repairing the slot map should delete both together.

**Guarded, LIVE, and worth a look anyway** (the guard is a runtime predicate;
whether it holds on every receiver is what `observe_read` answers and source
cannot): `java/util/StringJoiner`'s swapped `prefix`/`delimiter` and its
`emptyValue`→`size`; `java/lang/reflect/Method`'s 11-of-12 legacy map.

**Synthetic-only, and one of them is the sharpest shape in the census:**
`java/time/Month`'s `MONTH_FIELD_VALUE` writes an `Int` ordinal into slot 0,
which on any real enum is `java.lang.Enum.name` — a `String` reference. If that
registrar ever escapes `register_synthetic_overrides`, it is a bogus pointer for
the collector to mark and move, which is exactly the `MethodHandles$Lookup`
consequence §5 of the architecture note describes. Also here:
`Exchanger` (2), `Phaser` (3), `Cleaner` (1).

**Not a read-side defect, filed against the other census:**
`vertx_eventloop.rs`'s 5-slot `java/lang/Thread` mirror is self-allocated, so it
is `layout_alias`'s `under` direction (5 vs 19) and should already appear in a
run of `CRATONVM_DBG_LAYOUT_ALIAS=1`.

## 7. What this lane could not resolve

1. **Runtime confirmation of anything.** Nothing was built or run. The six wired
   observation points are source-level claims about where the calls sit; §4's
   census is a source-level classification, not a transcript. The first real
   product of this instrument will be a `CRATONVM_DBG_LAYOUT_ALIAS=1` run over a
   suite, which nobody has done.
2. **`verify_declared_slot_maps` has no caller.** The sweep exists, is tested,
   and is published to by `register_io_natives` — but nothing calls it yet.
   Choosing its trigger (a debug-only VM hook after a workload, most likely)
   needs a build, and wiring it blind would be a call nobody has seen run.
   **CLOSED 2026-08-12 by W7-90-slot-map-sweep-caller.md**, and the guess in
   parentheses was right: the trigger is the launcher's teardown immediately
   after `main(String[])` returns, plus the three self-terminating natives,
   because a `System.exit` never reaches the first. Three gates (links 7-9 of
   `read_alias_coverage.rs`) fail if a `SlotMap` is declared and never swept;
   all three are RED on the tree as it stood before that lane. Note that
   **§4.1's 11,948 / 6 figures did not improve** — the sweep covers a different
   and much smaller population (38 slots in 7 maps).
3. **11,942 of 11,948 constant-slot reads state no expected field.** They are
   printed, not covered. Closing that is a per-crate migration to `SlotMap`
   declarations, and it should be done class-by-class from §6's list rather than
   as a sweep.
4. **The census only classifies slot maps that name their own class** — 36 of
   93 comment-named runs, out of 337 slot-map runs. The other 244 runs name no
   class at all and are unclassified, not clean. The two misattributions in §4.4
   show the method's error bars in both directions.
5. **`Unknown` is still one answer to two questions.** A class with zero declared
   instance fields is indistinguishable from one that is not loaded, inherited
   straight from `layout_alias`. 23 constants in §4.2 land there, most of them on
   interfaces (`java/net/http/HttpResponse`, `java/lang/ref/Cleaner$Cleanable`,
   `java/lang/foreign/MemorySegment`, `java/nio/file/Path`), where a non-zero
   slot map is the intended fabrication rather than an alias. Closing it needs a
   `class_is_loaded` predicate `NativeContext` does not have.
6. **`P67_ARENA_*` is unresolved, not clean.** Its comment names
   `jdk/internal/foreign/MemorySessionImpl` but `p67_new_arena` allocates
   `java/lang/foreign/Arena`, an interface with no instance fields; the slots are
   then written on that object and on a separate session object whose class this
   lane did not settle from source. It is excluded from the 31 and is the clearest
   case for why the runtime, receiver-keyed entry point is the primary one.
7. **Last-write-wins is settled only at the registrar level**, as in W7-59 §9.4.
   A LIVE verdict here means a registrar the real-JDK arm calls installs the
   native; whether a later registrar overwrites that specific triple is
   per-triple and needs a build.
