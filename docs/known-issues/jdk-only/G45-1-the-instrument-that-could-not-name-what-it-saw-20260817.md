# G45-1 — the instrument that could not name what it saw

**Status:** FIXED-SOURCE, AFTER-LINE PREDICTED. The before-state is MEASURED
on `C:/craton/target-rel3/release/cratonvm.exe` (built from `9ae371468`, the
first binary carrying the G30 coercion instrument). This lane may not build,
so no binary contains the fix yet and every "after" below is **PREDICTED** —
derived from the unchanged `tracing::warn!` format string in
`gc/src/heap.rs::note_field_coercion_loss` with the values the repaired call
sites now supply, and with the class ids resolved by a real
`CRATONVM_DBG_LAYOUT=1` run.

**Provenance:** MEAS for every count, species, descriptor and source line in
§1–§3. PRED for the after-lines in §0 and §2.

**Files:** `gc/src/collector.rs`, `gc/src/gen_heap.rs`, `gc/src/zgc.rs` —
the three this lane owns. `gc/src/heap.rs` needed **no change at all**; see
§5.

---

> **VERIFIED AGAINST A BINARY 2026-09-03. The headline holds: the instrument
> names what it sees.** This record's status was *"no binary contains the fix
> yet and every 'after' below is PREDICTED"*. Measured now on a binary built
> from this tree, running the vector this record measured its before-state on:
>
> ```text
> CRATONVM_DBG_COERCION=1 cratonvm … RJdkSecurity
>
>   coercion warnings carrying a data class_id   96
>   still class_id=-1 index=-1                    0
>   named                                        96   (100%)
> ```
>
> §0's before-state was *"5,431 events across 63 vectors. Every one of them
> printed `class_id=-1`"*. On this vector it is now none of them. The census
> line agrees and reports the hottest site rather than a shrug:
>
> ```text
> descriptor-coercion census: total=96 primitive-into-reference[read=88 store=8]
>   hottest=primitive-into-reference/read class_id=36 index=0 descriptor=[ hits=58
> ```
>
> The distribution across six distinct sites:
>
> ```text
> class_id=36  index=0    58      class_id=596 index=0     3
> class_id=132 index=16   30      class_id=552 index=1     3
> class_id=669 index=4     1      class_id=584 index=4     1
> ```
>
> **A residual on this record's own remediation advice.** The warning text tells
> the reader *"Run with `CRATONVM_DBG_LAYOUT=1` to resolve a `class_id` to a
> name"*. Run exactly that way, alongside `CRATONVM_DBG_COERCION=1`, **no
> id-to-name mapping was emitted for any of the six ids above** — 13,135 lines
> of output and not one resolves 36 or 132. So the instrument now names what it
> saw with an ID, and the documented route from that id to a class name did not
> work here. The gap is small but it is the difference between a diagnosis and a
> lookup the next reader has to invent; it is left open, not fixed.
>
> **A counting caution, recorded because it nearly produced the opposite
> verdict.** A first pass grepped the transcript for `class_id=-1` and found 97
> — apparently a fix that had not landed. Every one of those was the literal
> string inside each warning's own explanatory prose (*"class_id=-1/index=-1
> means the caller has not yet been given provenance"*), not the data. The data
> field is the trailing one, after `value=`. A message that documents a sentinel
> is indistinguishable from a message that reports it, to a grep.
>
> **What this does NOT verify.** One vector, not the 63 this record's
> before-state spans — the 5,431-event figure is not re-measured and the
> proportion named across the whole corpus is unknown. The class ids are
> unresolved (above), so no row here is tied to a class name, and §2's
> per-species after-lines are therefore confirmed only in shape. Nothing in
> §§1-3's MEASURED before-state was re-derived.

> **The remediation-advice residual is FIXED 2026-09-04.** The discharge note
> above recorded that this record's own warning text — *"Run with
> `CRATONVM_DBG_LAYOUT=1` to resolve a `class_id` to a name"* — produced nothing:
> 13,135 lines of output and not one `[layout]` row, so none of the six class ids
> in that transcript could be resolved.
>
> **The cause is a cheap gate standing in front of an informative one.**
> `ClassRegistry::register_compact_layout_if_enabled` opened with
>
> ```rust
> if !cratonvm_types::compact_ref_fields_enabled() {
>     return;                      // <- the diagnostic sat past this
> }
> ```
>
> so with compact reference fields off — the default — `CRATONVM_DBG_LAYOUT=1`
> printed nothing at all, while **three** separate doc comments told readers to
> use it for exactly this mapping: `gc/src/autobox.rs`, `gc/src/heap.rs` (this
> record's own warning), and `types/src/compact_value.rs`. The flag was gated on
> an unrelated FEATURE, not on what it reports.
>
> The class-id-to-name mapping exists in every configuration, so it is now
> emitted in both: unchanged when compact layouts are on, and as
> `[layout] <name> cid=<N> (no compact layout: …)` when they are off.
>
> **`autobox.rs`'s own comment, two flags earlier, says why this mattered**:
> that message once advertised `CRATONVM_DBG_TOARRAY=1`, which printed nothing
> at its site, and *"a diagnostic that names the wrong instrument costs more
> than no diagnostic, because it is trusted"* — measured by two readers who each
> followed the advice and got an empty transcript. This was the same failure one
> flag over, and it was found the same way.
>
> **What is NOT fixed.** The four-species coercion counter and its
> `class_id`/`index` provenance are unchanged — this makes the ids RESOLVABLE,
> it does not resolve them into the coercion warning itself, so a reader still
> has to cross-reference two lines of output. The 5,431-event, 63-vector
> before-state is still not re-measured.

## 0. The headline

G30 gave this tree a four-species counter for silent field coercions. It
works: 5,431 events across 63 vectors. Every one of them printed
`class_id=-1 index=-1`, so two separate lanes recovered the class by hand
from `javap` plus backtraces, and both nominated the same fix.

MEASURED, `RJdkSecurity` under `CRATONVM_DBG_COERCION=1`, the largest single
cluster in the corpus — the tail of the event line, verbatim:

```
species="pointer-into-primitive" access="unattributed" descriptor=I value=Object(Some(ObjectRef { ptr: 0x180801018a0 })) class_id=-1 index=-1 occurrence=0
```

PREDICTED, same event after this change:

```
species="pointer-into-primitive" access="store" descriptor=I value=Object(Some(ObjectRef { ptr: 0x180801018a0 })) class_id=475 index=2 occurrence=0
```

`475` is `java/security/Provider` (MEASURED, `CRATONVM_DBG_LAYOUT=1`:
`[layout] java/security/Provider cid=475 LEGACY, no compact layout`). Flat
slot `2` of that class is the inherited `java.util.Hashtable.threshold`, an
`int` — the field `Hashtable.addEntry` reads to decide whether to rehash.
The writer is `native-builtins/src/jca/provider_chain.rs:317`,
`ctx.set_field(p, 2, Value::Object(Some(info)))`: a live `String` reference
aimed at slot 2 of a *synthetic* `Provider` whose slots 0/1/2 were `name` /
`version` / `info`, landing on the *real-JDK* layout where slots 0/1/2 are
`Hashtable.table` / `count` / `threshold`. The site's own comment already
says "slot 1 is `Hashtable.count`" — the instrument now says the rest of it
out loud, in the log, without a `javap`.

Three facts the event line could not previously carry, all now present:

| field | before | after |
|---|---|---|
| `class_id` | `-1` | the receiver's real class id |
| `index` | `-1` | the flat slot index |
| `access` | `"unattributed"` | `"read"` or `"store"` |

`access` is the one nobody asked for and it may be worth the most: it
separates the 670-event benign read population in §3 from the 217-event
store defect, and before this change all 5,431 events were in one bucket.

---

## 1. Where the `-1` came from — three files, and only one of them mattered

`coerce_field_value_by_descriptor(value, desc)` is *defined* as
`coerce_field_value_for_slot(value, desc, FieldCoercionSite::UNATTRIBUTED)`.
Any caller of the short form reports `-1/-1` by construction. The estimate
this lane was handed — "one line each at `gen_heap.rs:4174/4187` and
`collector.rs:433/439`" — was checked against the signatures and is close
but not complete. What is actually there:

| site | overrides `*_as`? | reached how |
|---|---|---|
| `collector.rs:419/425/432/438` (trait DEFAULTS) | — | **every VM run** |
| `gen_heap.rs:4161/4167/4174/4180` (inherent) | n/a | `-XX:+UseGenerationalGC` runs |
| `g1.rs` | **no** | inherits `collector.rs` |
| `zgc.rs` | **no** | inherits `collector.rs` |
| `GenerationalHeap`'s `GarbageCollector` impl (`gen_heap.rs:17200`) | **no** | inherits `collector.rs` |

So it was eight call sites, not four, and the four in `collector.rs` are the
ones that matter: **no shipped collector overrides any of the four `*_as`
methods.** `VmHeap`'s enum `dispatch!` (`vm_heap.rs:222`) routes the whole VM
into the trait defaults, and the MEASURED backtrace agrees — every event in
every log this lane collected passes through
`cratonvm_gc::collector::GarbageCollector::set_field_as` at `collector.rs:433`
or is inlined straight from `VmHeap::get_field_as`.

The subtlety worth writing down, because it decides whether the `gen_heap.rs`
half is dead code: Rust resolves `h.get_field_as(..)` on a
`&GenerationalHeap` to the **inherent** method, which wins over the trait
method. `dispatch!` expands to exactly that. So the `gen_heap.rs` inherent
bodies are live for `-XX:+UseGenerationalGC` and the `collector.rs` defaults
are live for everything else. Neither is the other's dead twin, and letting
them disagree about what a slot receives is the shape of divergence that
produced W7-84 (see `autobox.rs`'s module note). Both are repaired, and
`the_inherent_accessors_and_the_trait_defaults_agree` pins that they answer
identically.

## 2. `zgc.rs` does not route through the coercion helper at all

Stated plainly because it is the fact most likely to be rediscovered the hard
way. VERIFIED by grep, zero hits across `zgc.rs`, `zgc/` and
`zgc_concurrent.rs` for `coerce_field_value_by_descriptor`,
`coerce_field_value_for_slot` and `FieldCoercionSite`. `ZgcRealHeap`'s
`GarbageCollector` impl (`zgc.rs:8502`) declares `get_field`, `set_field`,
`get_field_volatile` and `set_field_volatile` — the raw half — and **none**
of the four descriptor-aware `*_as` methods. It inherits all four.

That matters because **ZGC is the default collector** (`vm/src/config.rs:769`,
since 2026-08-10). The coercion provenance of a default `cratonvm` run comes
entirely from `collector.rs`. Two consequences, both of which have already
cost somebody time in this tree:

- A lane looking for a ZGC-side coercion site will find nothing and may
  conclude the instrument does not cover ZGC. It does — through
  `collector.rs`, and the `RJdkSecurity` numbers in §3 are a default (ZGC)
  run.
- A measurement blamed on `gen_heap.rs` is not measuring a default run. An
  earlier performance lane diagnosed a defect in `gen_heap.rs` that was not
  the collector executing.

A 22-line comment at `zgc.rs:8502` now says this at the impl, including the
condition under which it stops being true: if this heap ever overrides one of
the four for a fused fast path, it must pass a real `FieldCoercionSite` or
the default collector silently reverts to `class_id=-1`.

## 3. The clusters, and what they now name

MEASURED: 19 vectors under `--jdk-only` with `CRATONVM_DBG_COERCION=1`,
1,339 events. Attributed by the nearest `native_builtins` frame in each
event's backtrace.

| n | nearest native frame | species | desc | site |
|---:|---|---|---|---|
| 670 | `reference::native_rq_poll` | primitive-into-reference | `L` | `reference.rs:758` |
| 245 | `properties_sidetable::props_defaults` | primitive-into-reference | `L` | `properties_sidetable.rs:1680` |
| **217** | `jca::provider_chain::make_provider` | **pointer-into-primitive** | `I` | `provider_chain.rs:317` |
| 54 | `native_object_clone` | primitive-into-reference | `L` | `lib.rs:26586` |
| 19 | `unsafe_natives_ext::native_unsafe_get_object` | primitive-into-reference | `L` | `unsafe_natives_ext.rs:2677` |
| 15 | `net_phase_e::url_raw_full_string` | primitive-into-reference | `L` | `net_phase_e.rs:8017` |
| 8 | `lang_class::mirror_class_id` | primitive-into-reference | `L` | `lang_class.rs:1737` |
| 7 | `lang_invoke::varhandle_compare_and_set` | primitive-into-reference | `L` | `lang_invoke.rs:4536` |

**`provider_chain::make_provider` — 217, and all 217 are the same event.**
Uniform `species="pointer-into-primitive" descriptor=I`. It is the §0
headline; after this change it is self-identifying without a backtrace, and
without a backtrace it can be read from a rate-limited default-configuration
log rather than only under `CRATONVM_DBG_COERCION=1`. 207 of the 217 are in
`RJdkSecurity`, 5 in `RSslLiveSession`, 3 in `RJdkX509Intercept`, 2 in
`RCrypto` — so a lane repairing it can iterate on `RJdkSecurity` alone.

**`native_rq_poll` — 670, and the `access` field is why this one matters.**
These are READS of a slot that was never descriptor-initialised, answering
`null`, which is what the field means. `heap.rs`'s `FieldAccessKind` doc
already predicted this population from an `RJdkNet` run (336 of 352 reads of
`java.lang.ref.ReferenceQueue.head`); this sweep confirms it at scale and at
half the corpus. They will now be filed under `access="read"` and stop
outnumbering the 217 real stores 3:1 in one undifferentiated bucket.

**`lang_class::mirror_class_id` — 8 here, 398 corpus-wide** per the two
earlier lanes. This is the *read* counterpart of the W7-84 write, which
nobody has examined; the sweep here is 19 vectors, so the 8 is a floor, not a
correction. Not investigated by this lane.

**`native_object_clone` — 54 here, 270 corpus-wide.** Clone re-runs the
coercion per field, so it amplifies whatever the original store got wrong
rather than being an independent defect. Worth reading after the sites it
copies from are repaired, not before.

Also MEASURED, and worth recording because it bounds where to look: of the
nine vectors this lane re-ran, `RCollections`, `RMapGcStress`, `RMapResizeGc`,
`RJitGc` and `RForNameGcStress` produce **zero** coercion events (11 W7-84
autobox lines each and nothing else). The population is concentrated in
JDK-boot-heavy and security/net vectors, not in the GC stress family.

## 4. Why the hot path cannot have changed

These four bodies are the VM's entire descriptor-aware field path and 97 of
99 vectors pass over them under `--jdk-only`, so the acceptable behavioural
delta is zero. Three independent reasons it is zero:

1. **The returned `Value` is bit-identical, by construction.**
   `coerce_field_value_for_slot` reads its `site` parameter in exactly one
   place: as an argument to `note_field_coercion_loss`. SOURCE-VERIFIED
   against every arm of the function. No arm branches on it, and the short
   form `coerce_field_value_by_descriptor` this change replaced is *literally*
   the same function with `site = UNATTRIBUTED`. The value half of the call
   is the same code.
2. **The reporting is unchanged.** `note_field_coercion_loss` is still
   `#[cold]`, still rate-limited `n < 4 || n.is_power_of_two()` per species,
   still on target `cratonvm::gc::guard`. It is reached from the same arms,
   for the same inputs, at the same frequency. Only its arguments improved.
3. **The added work on the non-lossy path is one already-resident load.**
   `class_id_of` on all three collectors is `header.class_id` — `zgc.rs:8576`,
   `gen_heap.rs:3069`, `g1.rs:9801` — and `get_header` is `#[inline]`
   (`gen_heap.rs:2862`: one pointer deref plus two cached-`OnceLock` bool
   checks that are the *same* checks the adjacent `get_field` performs). The
   neighbouring `get_field`/`set_field` in the very same call already
   dereferences that header to reach `compact_field_slot(header, index)`, so
   the line is in L1. `FieldCoercionSite` is a three-word `Copy` struct
   passed to an `#[inline]` function. No branch, no atomic, no allocation, no
   lock.

The counter `fetch_add` and the `tracing::warn!` remain entirely inside the
`#[cold]` function, as before.

Re-ran under the fixed binary's predecessor to establish the before-state is
green, all `--jdk-only`, all **PASS** (rc=0 and a `PASS <Class>` line):
`RJdkHello`, `RCollections`, `RMapGcStress`, `RMapResizeGc`,
`RPriorityQueueGc` (`--nojit --Xmx 64m`), `RTreeRangeGc` (`--Xmx 64m`),
`RJitGc`, `RForNameGcStress`, `RCrypto`. The GC-sensitive flags are from
`regression-suite/harness-guard.sh::class_cv_args`; without them
`RPriorityQueueGc` and `RTreeRangeGc` are inert rather than failing, which is
the worse outcome because it is silent.

## 5. `heap.rs` needed nothing — the estimate was wrong in the useful direction

The brief allowed for a `heap.rs` signature change as a nomination. **None is
needed, and this is worth stating so the next lane does not re-derive it.**
`pub fn coerce_field_value_for_slot(value, desc_byte, site)` already exists,
is already `pub`, is already `#[inline]`, and `FieldCoercionSite::read` /
`::store` are already `pub` constructors taking exactly
`(Option<ClassId>, usize)` — which is exactly what a collector has in hand at
each of the eight call sites. The G30 lane built the whole receiving end and
declined to wire it only because those files belonged to other lanes. The fix
is the argument, not the signature.

`heap.rs`'s own four accessors (`heap.rs:864/874/894/904`) already pass a
real site and are the model these eight were made to match, down to the
`Some(self.class_id_of(obj_ref))` spelling.

## 6. Tests

`gc/src/collector.rs` gains its first `#[cfg(test)]` module,
`coercion_provenance_tests`, built on a `RecordingCollector` in the shape of
`g1.rs`'s `StubCollector` — every method the tests do not call is
`unreachable!()`.

- `the_four_descriptor_aware_defaults_ask_who_the_object_is` — **the pin.**
  Each of the four defaults must call `class_id_of` **exactly once** per
  access. Reverting any one of them to `coerce_field_value_by_descriptor`
  leaves the counter at 0 and the test red. "Exactly once" rather than "at
  least once": twice would mean the header is re-read per access on the
  allocation hot path.
- `provenance_did_not_change_what_the_slot_receives` — a 14-case table
  through all four defaults, asserting the stored/returned `Value` is
  identical to `coerce_field_value_by_descriptor`'s.
- `the_reads_are_reads_and_the_writes_are_stores` — the direction and the
  `UNATTRIBUTED` contrast.

`gc/src/gen_heap.rs`'s existing `mod tests` gains the inherent-method half:
`descriptor_aware_accessors_land_exactly_what_the_bare_helper_lands` and
`the_inherent_accessors_and_the_trait_defaults_agree` (fully-qualified
`GarbageCollector::set_field_as(&heap, ..)`, because plain method syntax
would pick the inherent method and compare it with itself).

**Counter isolation, deliberately.** None of the new tests fires a lossy
coercion. The loss counters are process-global and `heap.rs`'s G30 tests
assert **exact** deltas on them under a module-private `g30_lock()` that
neither `collector.rs` nor `gen_heap.rs` can take. A lossy input from these
modules would make those exact deltas flaky *from another module*, for no
gain — the lossy arms are already covered where the lock lives. What these
tests can observe locally is (a) whether the default asked `class_id_of` at
all, which is a necessary condition for real provenance and which the
replaced call never satisfies, and (b) the value identity, which is the half
that carries the risk. Every case in both tables is a normalising or identity
arm that never reaches `note_field_coercion_loss`.

Not run: this lane may not invoke `cargo build`/`check`/`test`. `rustfmt
--edition 2021 --check` is clean over all three files' changed regions (the
tree has pre-existing formatting drift elsewhere in `gen_heap.rs`,
`collector.rs:79` and `zgc/`; none of it is in a hunk this lane touched).

---

## NOMINATIONS

**N1 — `provider_chain.rs:317` writes a `String` into `Hashtable.threshold`.**
NOT this lane's file. 217 MEASURED events, all identical, all
`pointer-into-primitive` at descriptor `I`. `make_provider` allocates a
`java/security/Provider` and then writes its own synthetic slots 0/1/2
(`name`/`version`/`info`) into a class whose real-JDK flat layout is
`Hashtable.table`/`count`/`threshold`. Slot 2 takes a live `String` pointer
where an `int` is declared; `coerce_field_value_for_slot` turns it into
`Value::Int(ptr as i32)`, so `Hashtable.threshold` becomes the low 32 bits of
a heap address and `addEntry`'s rehash decision reads it. HEAD commit
`7d6b596a8` claims a fix in this area on the servlet path; whether it covers
this site is untested here — the binary available to this lane predates it.
Re-measure on `RJdkSecurity` (207 of the 217) before doing anything else.

**N2 — the `Float`/`ReturnAddress` asymmetry at a reference slot.** Carried
forward unchanged from G30 NOMINATION 6, restated because this lane's sweep
gives it a denominator: `heap.rs`'s `b'L' | b'['` arm nulls `Int`, `Long` and
`Double` but *stores* `Float` and `ReturnAddress`, reporting them as
`PrimitiveIntoReferenceUncoerced`. Zero events of that species in the
1,339-event sweep. The population is either genuinely empty or lives outside
these 19 vectors; either way it is now measurable, which it was not before.

**N3 — resolve `class_id` to a name in the event line itself.**
`heap.rs`'s message tells the reader to re-run under `CRATONVM_DBG_LAYOUT=1`
to turn a class id into a name. The W7-84 sibling in `autobox.rs` says the
same. Both now have a real class id to resolve and neither can resolve it,
because `gc` cannot reach the class registry. `crate::gc::resolve_class_info`
exists and is already used from `gen_heap.rs`'s FIELD-WATCH path
(`gen_heap.rs:4013`), so the wiring is available; it is a `heap.rs` change
and is not taken here. Worth doing only inside the `#[cold]` reporter.

**N4 — `properties_sidetable.rs:1680` reads 245 primitives out of a
reference slot.** Second-largest cluster and, unlike `native_rq_poll`, not
obviously the benign never-initialised-slot shape: `props_defaults` reading
`Properties.defaults` should find a `Properties` or `null`, not an `Int`.
Not investigated. Once the after-line lands it identifies its own class and
slot.

## What this lane could not settle

- **No "after" line was executed.** The rule against `cargo build` means no
  binary contains this change. §0's after-line is composed from the unchanged
  format string plus MEASURED values (`cid=475` from a real layout dump, `2`
  from the source at `provider_chain.rs:317`), which is as close as this lane
  can get, but it is PREDICTED and should be marked MEASURED by whoever runs
  it first. The single command:
  `CRATONVM_DBG_COERCION=1 cratonvm --jdk-only -cp . RJdkSecurity 2>&1 | grep 'pointer-into-primitive'`
  — 207 lines, and every one of them should now read `class_id=475 index=2`.
- **The corpus-wide totals were not reproduced.** The 5,431-across-63-vectors
  figure and the 398 / 270 counts for `mirror_class_id` and
  `native_object_clone` come from earlier lanes' full 100-vector sweeps. This
  lane swept 19 vectors (1,339 events) and reproduced `make_provider`'s 217
  exactly, which is the number that mattered; the other two are floors here,
  not disagreements.
- **Whether `class_id_of` should be lazy.** It is called unconditionally, and
  §4 argues the cost is one L1-resident load. That argument is from source
  inspection, not from a measurement — this lane could not build, so it could
  not profile. If a future lane measures a regression on the field-access
  path, the fix is a `heap.rs` variant of `coerce_field_value_for_slot` that
  takes a `FnOnce() -> FieldCoercionSite` instead of a value, which would move
  the header read behind the `#[cold]` branch. That is a `heap.rs` signature
  change and is deliberately **not** nominated, because nominating an
  unmeasured optimisation is how a hot path acquires a closure it did not
  need.
