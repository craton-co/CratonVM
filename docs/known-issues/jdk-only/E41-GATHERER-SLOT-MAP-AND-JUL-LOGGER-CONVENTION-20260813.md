# E41 — one slot map per class: `java/util/stream/Gatherer`, and the second `java/util/logging/Logger` convention

**Date:** 2026-08-13 **Lane:** E41
**Closes:** the "other half" of E35 §3.1 (`Gatherer`) and NOM E35-4 (`Logger`), the
latter with a **different verdict than E35 predicted** — see §2.1.

**This lane did not build or run CratonVM, and did not run `cargo`.** Every
CratonVM "after" below is **PREDICTED**. Evidence is (a) source read in this
working tree, (b) the JDK 25 source checkout at `C:\craton\jdk25src`, and (c) the
`--dump-native-registry` JSON at `scratchpad/p1/reg.json` (`mode: compatible`,
11,748 entries), whose `owns_slot` / `overwrote` fields are the registry's own
verdict. **That dump is missing an entire mode:** `register_synthetic_overrides`
is `#[cfg(feature = "synthetic-jdk")]` (`native-api/tests/guarded_slot_maps.rs:597`
asserts the attribute is immediately above it), so **nothing** it reaches appears
in the dump. Every conclusion below states which of the two sources it rests on.

## 0. Files edited

| file | why it is mine |
|---|---|
| `native-builtins/src/phases_late/streams.rs` | the 3-slot `Gatherer` registrar. **Path note:** the assignment named this file as `streams.rs`; `native-builtins/src/streams.rs` also exists and contains no `Gatherer` code at all (verified: zero matches for `Gatherer`). The 3-slot map the task describes is only in `phases_late/streams.rs:4523`, which is a distinct file from `phases_late.rs`. |
| `native-builtins/src/logging_shims.rs` | assigned |
| `native-builtins/src/logmanager.rs` | assigned (one line: `LOGGER_NUM_FIELDS` → `pub(crate)`) |

`native-builtins/src/streams.rs` was swept (§3) and needed no change.

---

## 1. TASK 1 — `java/util/stream/Gatherer`: two maps, one deleted

### 1.1 Which map is right

`java/util/stream/Gatherer` has **no `synthetic_stub_fields` arm**, so
`class_num_total_fields` answers 0, `try_alloc_concurrent_synthetic`'s closing
`let n = num_fields.max(real)` is the identity, and **the ask IS the object
width**. Nothing clamps a disagreement.

| registrar | slots | map |
|---|---|---|
| `lib.rs::register_pd_stream_gatherers` | 5 | initializer 0, integrator 1, **combiner 2**, **finisher 3**, KIND 4 |
| `phases_late/streams.rs::register_p67_gatherer` | 3 | initializer 0, integrator 1, **finisher 2** |

**The 5-slot map is right, on both authorities available without a run:**

* **Oracle.** `Gatherer` itself is an interface (`jdk25src/java.base/java/util/stream/Gatherer.java:199`),
  so it has no instance fields in either mode. The carrier every one of its
  static factories returns is the record `Gatherers.GathererImpl`, declared
  `(Supplier initializer, Integrator integrator, BinaryOperator combiner,
  BiConsumer finisher)` — `jdk25src/java.base/java/util/stream/Gatherers.java:502-506`.
  **Slots 0..3 of the 5-slot map are that record, in order**; slot 4 is a
  VM-internal kind tag anchored past it. The 3-slot map dropped `combiner`,
  which is a real interface method (`Gatherer.combiner()`, `Gatherer.java:235`),
  and so mis-seated `finisher` one slot low.
* **Consumers.** `register_phase_d_natives` (`lib.rs:24181`) runs AFTER
  `register_phase67_natives` (`lib.rs:24124`) — both unconditional, both inside
  the same `register_synthetic_overrides` body (`lib.rs:21590`) — and
  `register()` is last-registration-wins. So every reader that executes is
  lib.rs's: `pd_stream_gather` opens with `ctx.get_field(gatherer, 4)`,
  `finisher()` reads 3, `combiner()` reads 2.

So the 3-slot registrar was minting objects for readers that disagreed with it,
including on the identity of slot 2.

### 1.2 The edit

Deleted from `register_p67_gatherer` — **each already re-registered later, and
already winning**, in `lib.rs::register_pd_stream_gatherers`:

| deleted here | winner |
|---|---|
| `Gatherer.of(Integrator)` | `lib.rs:42693` |
| `Gatherer.ofSequential(Supplier,Integrator)` | `lib.rs:42681` (added by E35) |
| `Gatherer.ofSequential(Supplier,Integrator,BiConsumer)` | `lib.rs:42636` |
| `Gatherer.initializer()` / `integrator()` / `finisher()` | `lib.rs:42599..42634` |
| `Gatherers.fold` / `scan` / `windowFixed` / `windowSliding` | `lib.rs:42741..42811` |
| `Stream.gather(Gatherer)` | `lib.rs:42591` |

**Kept:** `Gatherer.defaultInitializer()` and `defaultFinisher()` — the only two
triples `lib.rs` does not register. They allocate nothing and index no slot, and
their long "null IS the sentinel" note is load-bearing, so it survives with the
one sentence that pointed at the deleted `of(..)` re-aimed at `lib.rs:42693` and
its slots 0/3.

Chosen over **widening this file to 5**, which is what a width-only reading would
suggest: that leaves two copies of one slot map to drift apart again, which is
the defect itself. `lib.rs` additionally serves `Gatherer.combiner()`,
`Gatherer$Downstream.push` and `Gatherers.mapConcurrent`, which this file never
had — so deleting loses no coverage.

### 1.3 What `stream.gather(...)` does, before and after

**Before:** `stream.gather(Gatherer.ofSequential(sup, integ))` in synthetic-JDK
mode **aborted the VM** — that overload's 3-slot product reached
`pd_stream_gather`'s `get_field(gatherer, 4)` and `Heap::get_field`
(`gc/src/heap.rs:652`) opens with `assert!(index < num_slots)`: *"field index 4
out of bounds (num_slots=3)"*. **After (PREDICTED):** every `Gatherer` in the VM
comes from one 5-slot producer, `stream.gather(...)` reads a real `KIND` tag and
dispatches, and `combiner()` can no longer return the finisher.

The abort itself was already stopped by E35's `lib.rs:42681`; **this edit removes
the second map, which is what stops a future registration-order change from
re-detonating it.** That is the whole point: the E35 fix was order-dependent, and
this one is not.

### 1.4 Not broken by the deletion

`vm/src/vm/tests.rs:50408 gatherer_of_p67` and `:50455 gatherers_window_fixed_p67`
call `Gatherer.of` / `Gatherers.windowFixed` / `defaultInitializer` /
`defaultFinisher` **through the registry**, so they hit lib.rs's surviving
registrations (which already won). `vm/src/vm/tests.rs:56657` asserts "integrator
in field 1" — true of the 5-slot map. No orphaned functions: every deleted body
was an inline closure, and `let gs = ...` went with them.

---

## 2. TASK 2 — the second `Logger` convention

### 2.1 Ownership from the registry, not from source order — and E35's verdict was half wrong

`class_manager.rs:14191` declares `java/util/logging/Logger` with **named,
real-JDK-ordered** fields: `config` 0, `manager` 1, **`name` 2**, … `parent` 8,
… plus `vm_internal_field(12)` for the VM's level. `logmanager.rs:157-162`
matches it (`LOGGER_FIELD_NAME = 2`, `_PARENT = 8`, `_LEVEL = 12`).
`lib.rs:36361-36362` declares a **second, local** `LOGGER_FIELD_NAME = 0` /
`LOGGER_FIELD_LEVEL = 1`, which name `config: Logger$ConfigurationData` and
`manager: LogManager`.

**From the dump (compatible mode):** zero registrations from
`logging_shims.rs:1780-1990` and zero from `phases_late/streams.rs:4519-4695` —
i.e. `register_logging_natives` and `register_p67_gatherer` **do not exist in
that mode at all**, exactly the blind spot the brief warned about. Every
`java/util/logging/Logger` triple in the dump is owned by `lib.rs:17334-17909`
(the `crate::logmanager::LOGGER_FIELD_*` family), `logmanager.rs:5920-6736`, or
`reflect_annotations.rs:214-255`.

**From source (synthetic-JDK mode), the call order inside
`register_synthetic_overrides` is:**

```
23849  reflect_annotations::register_annotation_overrides   (→ logmanager natives, 1st time)
24028  logging_shims::register_logging_natives              ← the 0/1 family
24029  logging_shims::register_slf4j_natives
24106  phases_late::register_p61_logging      (via phase 61)
24144  phases_late::register_p71_logging_extras (via phase 71)
24372  logmanager::register_logmanager_natives              ← final say
```

E35 §3.3 concluded the family was "registered and shadowed, i.e. dead" because
`logmanager` runs last. **That is right for the conclusion and wrong for the
reason, and the wrong reason hid three live sites.** `register_logmanager_natives`
registers **no** `getGlobal`, `getName`, `getLevel`, `setLevel` or
`config(String)V`. What actually shadows all fifteen of
`register_logging_natives`'s `Logger` triples is `register_slf4j_natives` —
**the next line, in the same file** — which registers a superset. And
`register_slf4j_natives` is therefore the **winner** for `getGlobal`, `getLevel`,
`setLevel`, `getName` and `config(String)V` in synthetic-JDK mode… and it carried
a **third copy of the 0/1 convention, written as bare slot literals**.

So: the *registrations* at `logging_shims.rs:1780` are dead; the *convention* was
not. It had a live reader in every mode and three live writers in synthetic-JDK
mode.

### 2.2 The live sites, and what they did

| site (mine) | triple it serves | shadowed by | live? |
|---|---|---|---|
| `jul_logger_filter_name` :621 | helper for `Logger.setFilter`/`getFilter`, owned by `lib.rs:17592`/`:17608` — **`owns_slot: true` in the dump** | — | **LIVE, ALL MODES** |
| `jul_log_msg` :3750 | `Logger.config(String)V` in `register_slf4j_natives` | nothing | **LIVE, synthetic-JDK** |
| `register_slf4j_natives` `getGlobal` | — | nothing | **LIVE, synthetic-JDK** |
| `register_slf4j_natives` `setLevel` | — | nothing | **LIVE, synthetic-JDK** |
| `register_slf4j_natives` `getLogger` / `isLoggable` / `addHandler` / `removeHandler` | logmanager :5927/:6639/:6475/:6481, `reflect_annotations` :213/:240 | dead |
| `register_logging_natives` (all 15 `Logger` triples) | `register_slf4j_natives` @24029 | dead |
| `register_slf4j_natives` `LogManager.getLogger` | logmanager :5927 (`owns_slot: true`) | dead |

Three of those were genuine defects, not cosmetics:

* **`setLevel` wrote `Value::Int(level)` into slot 1** — an `Int` into
  `manager: Ljava/util/logging/LogManager;`. This is character-for-character the
  defect `logmanager.rs:161`'s comment records as *already fixed there* ("this
  used to sit on `manager`"), reappearing in a second file. Meanwhile
  `lib.rs:17605` (the real-JDK-mode owner of the same triple) writes slot 12 — so
  the two modes disagreed about where a logger's level lives.
* **`jul_log_msg` read slot 2 as the handler list** and then called
  `size()` / `get(I)` on it. On the 13-slot Logger that `Logger.getLogger(name)`
  actually returns, **slot 2 is the name `String`** — so `logger.config("…")` in
  synthetic-JDK mode invoked `String.size()`, whose failure `?`-propagates out of
  the native.
* **`jul_logger_filter_name` fell back to slot 0** for the name, i.e. read the
  `ConfigurationData` and handed it to `read_string`. The only all-modes reader.

None of them could ever assert out of bounds: `Logger` **has** a declaration, so
`num_fields.max(real)` clamped every 2- or 3-slot ask up to 13 and all the writes
landed in bounds. **The width was never the hazard. The field identity was** —
[1 of 10 callsites] / [mock=slot table] again.

### 2.3 The edits — one convention survives in this file

`logging_shims.rs` now contains **no local or literal JUL-Logger slot number**;
every site names `crate::logmanager::LOGGER_FIELD_NAME` / `_LEVEL` /
`LOGGER_NUM_FIELDS` (verified by grep: the only remaining bare indices in the file
are `Level`'s own slots 0/1 in `jul_level_int`, which match the real class).

1. `jul_logger_filter_name` → delegates to `logmanager::jul_logger_name_object`,
   the tree's one three-layout-tolerant name reader (used ~10× in `logmanager.rs`).
   `Option` semantics preserved exactly: `None` only when no layout holds a name
   object, `Some("")` for a genuinely empty name.
2. `jul_log_msg` → name via `logmanager::read_jul_logger_name`; handlers via
   `jul_logger_handlers_get`, the GC-safe side table that the `addHandler` which
   actually owns the slot (`reflect_annotations.rs:213`) writes into.
3. `register_slf4j_natives`: `getLogger` / `getGlobal` / `LogManager.getLogger`
   now ask `LOGGER_NUM_FIELDS` (13) and write `LOGGER_FIELD_NAME`;
   `setLevel` / `isLoggable` use `LOGGER_FIELD_LEVEL`;
   `addHandler` / `removeHandler` use the side table (dropping the
   `object_num_fields(this) > 2` guard, which was measuring a legacy 2-field shape
   no producer in this file mints any more).
4. `register_logging_natives`: its `setLevel`, `isLoggable` and `addHandler`
   closures moved to the same three names.
5. `logmanager.rs`: `LOGGER_NUM_FIELDS` is now `pub(crate)` with a doc comment
   naming it as *the* width JUL Logger producers must ask for.

**One deliberate non-edit, and one deliberate half-edit:**

* `setLevel` now stores the **`Level` reference**, not `Int(decoded)`. Both halves
  had to change together: slot 12 is `vm_internal_field(12)`, whose descriptor is
  `Ljava/lang/Object;` (`class_manager.rs:11968`), so *moving the `Int` to slot 12*
  would merely relocate the wrong-tag write. Storing the reference is what
  `lib.rs:17605` and `logmanager.rs:2103` already do, and `jul_level_int` decodes
  both shapes, so the name-keyed `record_jul_logger_level` publish is unchanged.
* `register_slf4j_natives`'s `getLevel` still ignores its receiver and mints a
  fresh INFO `Level` per call. It reads **no** Logger slot, so it is not a slot
  bug; making it read `LOGGER_FIELD_LEVEL` would start returning `null` for a
  logger with no explicit level — the real JDK contract, but a behaviour change
  that needs a run. Left with a comment saying so. It is now the only remaining
  divergence between this registrar and the essential one.

**PREDICTED after:** in synthetic-JDK mode `Logger.getLogger(x).setLevel(SEVERE)`
records the level where `getLevel`/`isLoggable`/`logmanager` look for it instead
of on `manager`; `Logger.getGlobal().getName()` answers `"global"` through the
canonical reader instead of depending on which layout `getName` happened to
believe in; `logger.config("…")` stops calling `size()` on a `String`; and no
JUL-Logger object in this file carries a name in `config` or a level in `manager`.
Real-JDK/compatible mode is unaffected by 3–5 (those registrars do not run — dump
proof) and improved by 1 (`setFilter`/`getFilter`'s key).

### 2.4 Why the dead registrations were NOT deleted

The six bodies behind them (`alloc_logger`, `native_logger_get`,
`native_logger_get_global`, `native_logger_get_level`, `native_logger_log_if`,
plus the local constants) live in **`lib.rs`**, which this lane may not edit.
Deleting the registrations alone orphans them into `dead_code` warnings in
another lane's file. NOM E41-1 does both in one diff.

---

## 3. TASK 3 — the sweep of my three files

Shape (a) = an allocation whose width or field ORDER disagrees with
`class_manager.rs` or with another site for the same class. Shape (b) = a locally
declared slot constant duplicating a shared one.

### `native-builtins/src/streams.rs` — CLEAR, no change

* Two allocations only. `java/util/ServiceLoader$Itr` at **2** — undeclared, but
  **unanimous across all five sites in the tree** (`:321`, `phases_late/streams.rs:134`
  and `:152`, `phases_late.rs:3993`, `servlet.rs:1924`) on width *and* map
  (0 = backing store, 1 = cursor), and this file both produces and consumes it
  with that map (`native_sl_itr_has_next`/`_next`). `java/util/Collections$EmptyIterator`
  at **0** — the declaration's single field is `PUBLIC|STATIC|FINAL`, i.e. **zero
  instance fields**, so 0 is exact.
* `Stream` slot 0 = elements (read-only here) matches every producer's
  `try_alloc_concurrent_synthetic(.., "java/util/stream/Stream", 1)`.
* `Flow.Subscription` slot 0 = cancellation flag; the file already documents that
  it deliberately does **not** write demand there because that is lib.rs's layout.
* Shape (b): **no `const` declarations at all** in the file.

### `native-builtins/src/logging_shims.rs` — fixed, §2

Also checked and CLEAR: `java/util/logging/Level` at 2 (undeclared, unanimous
2×2, and slots 0/1 = `name`,`value` are a prefix of the real class);
`org/slf4j/Logger` 2, `org/slf4j/Marker` 1, `org/slf4j/impl/Static*Binder` 1,
`org/slf4j/ILoggerFactory` 0, `org/apache/logging/log4j/Logger` 2 — all
sole-convention within this file with no competing site; `java/util/ArrayList` 2
and `java/util/HashMap` 3 — both declared wider (4 / 16) and clamped up, and both
match the ~90-site tree-wide convention.

### `native-builtins/src/logmanager.rs` — one finding, reported not changed

* `java/util/logging/LogManager` at `LM_NUM_FIELDS` = 14 vs declared 14
  (`class_manager.rs:14165`, 12 named + 2 `vm_internal`) — **exact**.
* `java/util/logging/Logger` at `LOGGER_NUM_FIELDS` = 13 vs declared 13 — **exact**.
* `java/util/logging/LogManager$StringEnumeration` at 2 — undeclared, but sole
  producer (`:1855`) and sole readers (`native_enumeration_has_more`/`_next`) in
  this same file, both on 0 = array / 1 = cursor. **CLEAR.**
* `java/util/Collections$EmptySet` 0 (declared `vec![]`), `java/lang/Object` 0/1,
  `java/util/ArrayList` 2 — clear/clamped.
* **FINDING — `org/jboss/logmanager/Logger`, two maps and a bytecode-`new`
  hazard.** `get_or_create_jboss_logger` (`:2084`, `:2100`) allocates it at
  `LOGGER_NUM_FIELDS` (13) and writes `LOGGER_FIELD_NAME` **2**, `_PARENT` 8,
  `_LEVEL` 12. But `class_manager.rs:13850` declares that class with **two**
  fields — `name` **0**, `parentHandle` 1 — and `jboss_logmanager.rs:84` declares
  its own `const LOGGER_FIELD_NAME: usize = 0` under a comment that says
  *"matching the layout in `logmanager::LOGGER_FIELD_NAME`"*, **which is 2**. The
  comment is false. Consequence: `jboss_logmanager.rs`'s `logger_name` (`:186`)
  reads slot 0, finds nothing, and labels every boot-log line `<root>`. The
  allocation is safe (13 > 2, `max` clamps up), but **bytecode `new
  org/jboss/logmanager/Logger` sizes from the declaration** — 2 slots — and a raw
  `get_field(obj, 12)` on such an instance is a `Heap::get_field` assert. Not
  fixed here: the correct map depends on whether the declaration should track the
  real class (`org.jboss.logmanager.Logger extends java.util.logging.Logger`,
  which would make `logmanager.rs`'s 13-slot JUL map the right one and both the
  declaration and `jboss_logmanager.rs` wrong), and `class_manager.rs` and
  `jboss_logmanager.rs` are not mine. **NOM E41-2.**
* **FINDING — `org/jboss/logmanager/LoggerNode` (16) and `LogContext` (1) are
  undeclared and populated BY NAME.** `attach_minimal_jboss_logger_node` (`:2072`)
  writes `effectiveLevel` / `effectiveMinLevel` / `useParentHandlers` /
  `useParentFilter` via `set_field_by_name`, then `set_field_by_name(logger,
  "loggerNode", …)`. With no declaration, `ensure_synthetic_class` mints anonymous
  `_fN` slots, so **all five writes are silent no-ops in synthetic-JDK mode** and
  the node is all-null. The null-*safety* half of the function's stated purpose
  still holds; the INFO-default half does not. This is E35 §3.4's
  `UnixFileSystem`/`WinNTFileSystem` shape in a third subsystem. **NOM E41-3.**
* Shape (b): the file's `LM_FIELD_*` / `LOGGER_FIELD_*` constants **are** the
  shared ones — this is the file the rest of the tree imports from. No duplicates.

### Noticed while sweeping, outside my files (one line each)

* `java/util/ServiceLoader$Itr` slot 0 holds an **`Object[]`** at four sites and an
  **`ArrayList`** at `phases_late.rs:3993`. Same slot map, different store *type*;
  the `hasNext`/`next` readers in `streams.rs` assume the array. Recorded, not
  investigated — not one of the two shapes I was asked to sweep for.

---

## 4. NOMINATIONS

### NOM E41-1 — `native-builtins/src/lib.rs` — delete the second `LOGGER_FIELD_*` convention and its six bodies

Supersedes NOM E35-4 with the ownership question settled. After this lane,
`logging_shims.rs` no longer references either constant, so the whole family can
go in one diff without orphaning anything *in `logging_shims.rs`* — but the
registrations at `logging_shims.rs:1786-1908` still name four of the bodies, so
**the registration removals and the body removals must land together.**

Delete from `lib.rs`:

```rust
const LOGGER_FIELD_NAME: usize = 0;
const LOGGER_FIELD_LEVEL: usize = 1;
```

(`:36361-36362`; **keep** `LEVEL_FIELD_NAME` / `LEVEL_FIELD_VALUE` on the next two
lines — those are `java/util/logging/Level`'s own slots, they are correct, and
`native_level_init` is registered by `register_essential_natives_with_shims`
(`lib.rs:7485`/`:7491`) so it is live in **every** mode.)

…and the bodies `native_logger_get` (`:36390`), `native_logger_get_global`
(`:36402`), `native_logger_get_level` (`:36430`), `native_logger_log_if`
(`:36446`) — each reached only from `logging_shims.rs:1789`/`:1795`/`:1878`/
`:1834-1852`, each of those triples re-registered by `register_slf4j_natives`
one call later (`lib.rs:24029`) and most again by
`logmanager::register_logmanager_natives` (`lib.rs:24372`). `alloc_logger`, if it
has no other caller after that, goes too.

Also stale and load-bearing, `lib.rs:17591`:

OLD:

```rust
    // Our `allocate_logger`-created synthetic loggers (slot0=name, slot1=level, slot2=parent)
```

NEW:

```rust
    // Our `allocate_logger`-created synthetic loggers use the DECLARED layout
    // (`class_manager.rs:14191`, real-JDK order): name = LOGGER_FIELD_NAME (2),
    // parent = LOGGER_FIELD_PARENT (8), level = LOGGER_FIELD_LEVEL (12, the
    // VM-internal slot anchored past the 12 real fields). The "slot0=name,
    // slot1=level, slot2=parent" this comment claimed until 2026-08-13 was the
    // retired 3-field shim layout, and the three lines below it already use the
    // `logmanager` constants.
```

(Anchor verified unique in the working tree; verify again before applying, this
file is under concurrent edit.)

### NOM E41-2 — `classloading/src/class_manager.rs` + `native-builtins/src/jboss_logmanager.rs` — `org/jboss/logmanager/Logger` has two slot maps

§3. Decide once, then make all three sites agree:

* the real `org.jboss.logmanager.Logger` **extends `java.util.logging.Logger`**,
  which is the argument for keeping `logmanager.rs`'s 13-slot JUL map and
  **widening the declaration at `class_manager.rs:13850` to the same 13-field
  real-JDK-ordered shape** as `java/util/logging/Logger` (`:14191`) — that also
  removes the bytecode-`new`-sizes-2 / native-reads-12 hazard;
* under that choice, `jboss_logmanager.rs:84`'s `const LOGGER_FIELD_NAME: usize = 0`
  becomes `crate::logmanager::LOGGER_FIELD_NAME`, and its comment ("matching the
  layout in `logmanager::LOGGER_FIELD_NAME`") stops being false. **Note it is used
  for two different classes** — `logger_name` (`:186`) reads a `Logger`, but
  `level_name` (`:171`) reads a `Level`, where slot 0 genuinely is the name. Split
  it into two constants; do not renumber both.

Not offered as literal replacement text: the field list should be taken from the
real class, and the split above changes call sites this lane did not read in full.

### NOM E41-3 — `classloading/src/class_manager.rs` — declare `org/jboss/logmanager/LoggerNode` and `LogContext`

§3. `logmanager.rs:2072-2077` populates them entirely through
`set_field_by_name("effectiveLevel" / "effectiveMinLevel" / "useParentHandlers" /
"useParentFilter" / "loggerNode")`, and with no `synthetic_stub_fields` arm those
five writes resolve nothing in synthetic-JDK mode. Same remedy as NOM E35-5 and
the same caveat: declare **by name** off the real class, not pattern-matched off
the write list.

---

## 5. The lesson

**E35's rule — "no declaration AND two slot maps" is the alarm — held, and its
corollary is what this lane adds: a *shadowed* map is not a *dead* one.**

`Gatherer` is the clean case: no declaration, two maps, a `Heap::get_field`
assert. `Logger` is the case that looks like the opposite and is not. It *has* a
declaration, so `num_fields.max(real)` clamped every under-request and no assert
could ever fire — and the second map still produced a live `Int` in a reference
slot, a `String.size()` call, and a `ConfigurationData` handed to `read_string`.
The clamp protects the *heap*; nothing protects the *field's identity*.

And the thing that made the Logger half tractable was **asking the registry which
registration owns each triple instead of reading the file top to bottom**. E35
named `logmanager` (`lib.rs:24372`) as the shadower and concluded "dead". The
actual shadower was `register_slf4j_natives` on the *next line* (`lib.rs:24029`),
and because it is a superset rather than an equal set, five triples — `getGlobal`,
`getName`, `getLevel`, `setLevel`, `config` — landed on a registrar that carried
its own third copy of the wrong map. **"A later registrar exists" is not the
question; "which registrar owns THIS triple" is** — and the `--dump-native-registry`
JSON answers it directly for one mode, while the other mode has to be reconstructed
by hand from the `#[cfg]` boundary, which is exactly where the answer differed.
