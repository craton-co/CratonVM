# E35 / R11 — every synthetic allocation in `native-builtins/src/lib.rs`, against its declaration and its twins

**Date:** 2026-08-13 **Lane:** E35
**Closes:** NOM E28-2 in `E28-R11-P59-MODULE-WIDTHS-AND-CATALOG-20260813.md` §5.
**Extends:** that record's §1.4 audit from `phases_late/reflect_invoke.rs` to
`lib.rs`.

**This lane did not build or run CratonVM, and did not run `cargo`.** Every
CratonVM "after" below is **PREDICTED**. Everything else is source read in this
working tree, plus `javap -p` against Adoptium/Microsoft **JDK 25.0.3+9** as the
oracle.

**Edits applied (owned file only): `native-builtins/src/lib.rs`.**

| what | where (post-edit) |
|---|---|
| `Class.getModule()` allocates `java/lang/Module` at 5, not 2 (NOM E28-2) | `:12053` |
| `Gatherer.ofSequential(Supplier,Integrator)` registered here, shadowing the 3-slot factory that fed a slot-4 read | `:42681` |
| the `Class.getProtectionDomain` layout comment, which described a URL/CodeSource/PD shape its own closure contradicts, and which is dead code | `:13066-13094` |

**Nominations in §6.** None is required for the tree to compile.

---

## 1. The method

`try_alloc_concurrent_synthetic` (`util_concurrent_ext.rs:929`) was scanned
repo-wide with a balanced-paren extractor (**1,872 call sites**), and
`class_manager::synthetic_stub_fields` was parsed for its declared widths
(`instance_fields(n)`, `pad_to(.., n)`, and the block arms my first parser
missed — `java/lang/Thread`, `java/util/Locale`,
`java/nio/charset/CodingErrorAction` were all read by hand afterwards).

**The question that decides whether a mismatch is cosmetic or a heap defect is
"does the class have a `synthetic_stub_fields` arm?", and it is answered per row
below.** The success arm ends `let n = num_fields.max(real)`
(`util_concurrent_ext.rs:983`), so with a declaration an under-request is
clamped **up** and costs only a `report_layout_alias(class, asked, real)` per
call plus the failure-arm hazard E28 §1.1 describes (`fabricate_class` writes
`num_total_fields: num_fields` verbatim and memoises it). With **no**
declaration, `real == 0`, `max` is the identity, and **the ask IS the object
width**.

## 2. TASK 2 — the three columns

`lib.rs` allocates **40 distinct classes across 65 `try_alloc_concurrent_synthetic` call sites**. Columns:
what this file asks / what `class_manager::synthetic_stub_fields` declares / what
every other site in the tree asks.

| class | lib.rs asks | declared | other sites ask | verdict |
|---|---|---|---|---|
| `java/lang/Module` | ~~2~~ **5** | **5** `:15407` | 5 ×3 (`reflect_invoke`), `MODULE_FIELD_COUNT`=5 (`jboss:628`), **2 ×3** (`lang_class:18425`, `:18498`, `shared_secrets_bridge:506`) | **FIXED** (NOM E28-2). Was alias-report cost only. Three sites still at 2 → NOM E35-1 |
| `java/util/stream/Gatherer` | **5** ×7 | *(none)* | **3 ×7** (`streams.rs`) | **HEAP DEFECT — FIXED here.** §3.1 |
| `java/net/URL` | 13 | *(none)* | 13 ×4, **6 ×5** | heap-defect *shape*, cross-file; this file's site is dead (§4). NOM E35-2 |
| `java/security/MessageDigest` | 2 | *(none)* | **4 ×2** (`jca/message_digest.rs`) | two widths for one undeclared class, mode-split; no reader indexes >1, so not live. §3.2 |
| `java/util/logging/Logger` | 3 ×4 | **13, NAMED, real-JDK order** `:14191` | 2 ×2, 3 ×2 (`logging_shims`) | width clamped; **SHAPE DEFECT** in two of the four sites — §3.3 |
| `java/util/HashSet` | **2** | **16** `:12330` | 0, 1 ×3, 2 ×5, 3 ×7, 8 ×2 | clamped. **Five** conventions for one class. Stale declaration comment → NOM E35-3 |
| `java/util/ArrayList` | 2 ×4 | **4** `:12325` | 1 ×8, 2 ×70, 3 ×4, 4 ×1 | clamped. Stale declaration comment → NOM E35-3 |
| `java/lang/Thread` | 5 ×4 | **14** `:12881` (block arm) | 5 ×13, 0 ×1, `HS_THREAD_NUM_FIELDS`=5, `THREAD_SYNTHETIC_NUM_FIELDS`=6 | clamped. **17 sites, none of which asks the declared width** — pure alias-report cost |
| `java/util/logging/LogManager` | 2 | **14** `:14165` | 0 ×3, 1 ×1 | clamped; this site is the legacy fallback superseded by `logmanager.rs` |
| `java/security/CodeSource` | 2 | **12** `:13608` | 2 ×2 | clamped; writes BY NAME, so the number never reaches the heap. Real JDK: 6 |
| `java/security/ProtectionDomain` | 4 | **11** `:13530` | 4 ×6 | clamped; this site is dead (§4). Real JDK: 6 |
| `java/util/Locale` | 3 | **32** `:12696` (block arm) | 4 ×4, 32 ×1 | clamped |
| `java/nio/ByteBuffer` | 5 | **6** `:12686` | 3 ×2, 5 ×1, 6 ×3 | clamped; writes 0–4 positionally AND by name |
| `java/lang/ScopedValue` | 2 | **3** `:13186` | 2 ×1 | clamped |
| `java/lang/ClassLoader` | **0** | *(none)* | 0 ×1, 1 ×1 | a zero-slot object — but the site is SHADOWED and dead (§4) |
| `java/lang/ScopedValue$Carrier` | 3 ×2 | 3 | 2 ×2 | agrees with declaration; twins under |
| `java/nio/charset/Charset` | 3 | 3 | 1 ×12, 2 ×2 | agrees; twins under. **Order verified vs JDK 25** (§5) |
| `java/text/DecimalFormat` | 4 | 4 | 3 ×4 | agrees; twins under |
| `java/util/Properties` | 16 | 16 | 0, 2, 4 | agrees; twins under |
| `java/util/concurrent/ThreadPoolExecutor` | 2 ×3 | 2 | 2 ×4 | agrees |
| `java/time/Instant` | 2 | 2 | 2 ×4 | agrees. **Verified vs JDK 25** (§5) |
| `java/lang/Thread$State` | 2 | 2 | 2 ×1 | agrees |
| `java/lang/System$1` | 1 | 1 | — | agrees |
| `java/util/Random` | 2 | 2 | — | agrees (test-only site) |
| `java/util/ArrayList$ListItr` | 5 | 5 | — | agrees |
| `java/util/concurrent/ConcurrentSkipListMap` | 3 | 3 | — | agrees |
| `java/util/concurrent/StructuredTaskScope$Subtask` | 5 | 5 | — | agrees (`SUBTASK_FIELD_*` 0–3 + 1) |
| `jdk/internal/util/ClassFileDumper` | 4 | 4 | — | agrees |
| `java/nio/charset/CodingErrorAction` | 1 | 1, named `name` `:12758` | — | agrees. **Verified vs JDK 25** (§5) |
| `java/util/Base64$Encoder` | 4 ×2 | *(none)* | — | **verified vs JDK 25, width AND order AND types** (§5) |
| `java/util/Base64$Decoder` | 2 | *(none)* | — | **verified vs JDK 25** (§5) |
| `java/util/HexFormat` | 4 ×7 | *(none)* | `P64_HF_SLOTS`=4 | undeclared but unanimous, and **matches the real class** (§5) |
| `java/util/concurrent/atomic/AtomicStampedReference$Pair` | 2 | *(none)* | — | **matches the real class** (`reference`, `stamp`) |
| `java/util/concurrent/atomic/AtomicMarkableReference$Pair` | 2 | *(none)* | — | **matches the real class** (`reference`, `mark`) |
| `java/util/logging/Level` | 2 | *(none)* | 2 ×2 | unanimous, and the first two slots match the real class (`name`, `value`) |
| `java/util/function/Function$Identity` | 0 | *(none)* | 0 ×1 | unanimous |
| `java/util/stream/Gatherer$Downstream` | 2 | *(none)* | — | sole site |
| `java/lang/AssertionStatusDirectives` | 5 | *(none)* | — | sole site |
| `java/io/UnixFileSystem` | 3 | *(none)* | — | sole site; **writes only BY NAME** → §3.4 |
| `java/io/WinNTFileSystem` | 4 | *(none)* | — | sole site; same as above |

Plus one non-funnel allocation: `try_ensure_synthetic_class(CRATON_SYSTEM_LOGGER_CLASS, 2)`
at `:27172` — a VM-private class, no twin, nothing to compare.

**Score: 25 of the 40 rows agree with everything in sight — this file's ask equals the declaration, or (for an undeclared class) equals every other site's and the real class's. Of the 15 that do not, exactly one is a live heap defect, and it is the only row whose class has no declaration AND two disagreeing slot maps.** That conjunction is the discriminator, not the size of the numeric gap: `Thread` is off by 9 and is inert, `Gatherer` is off by 2 and aborts the VM.

## 3. The rows that are not cosmetic

### 3.1 `java/util/stream/Gatherer` — one class, two slot maps, and a `Heap::get_field` assert — **FIXED**

`java/util/stream/Gatherer` has **no `synthetic_stub_fields` arm**, so `real = 0`
and the ask is the width. In real-JDK mode it is an *interface*, so `real` is 0
there too: this class has no declaration in either mode.

Two registrars model it:

| | slots | map |
|---|---|---|
| `lib.rs::register_phase_d_natives` | **5** | initializer 0, integrator 1, **combiner 2**, **finisher 3**, KIND 4 |
| `phases_late/streams.rs::register_p67_gatherer` | **3** | initializer 0, integrator 1, **finisher 2** |

`finisher` is slot 3 in one map and slot 2 in the other, and slot 2 is
`combiner` in the first — so this is not merely two widths, it is two
*incompatible* maps.

Both registrars sit inside `register_synthetic_overrides`, whose call order is
`register_phase67_natives` (`lib.rs:24122`, reaching `register_p67_gatherer` via
`phases_late.rs:5299`) **then** `register_phase_d_natives` (`lib.rs:24179`).
`register()` is last-registration-wins
(`docs/architecture/natives-over-real-jdk-classes.md` §3), so this file's
accessors, its `Stream.gather`, and six of its seven factories win.

Comparing the two triple sets, `streams.rs`'s 3-slot factories are shadowed by
5-slot ones here for `Gatherer.of(Integrator)`,
`Gatherer.ofSequential(Supplier,Integrator,BiConsumer)`, and all four
`Gatherers.*` — **with exactly one exception**:
`Gatherer.ofSequential(Supplier,Integrator)` was registered only in `streams.rs`
(`:4541`). Its 3-slot product then reached the winning readers:

* `pd_stream_gather` (`lib.rs:42834`) opens with `ctx.get_field(gatherer, 4)`;
* `Gatherer.finisher()` (`lib.rs:42628`) reads slot 3;
* `Gatherer.combiner()` (`lib.rs:42620`) reads slot 2 — which on that object
  holds the **finisher**, i.e. a `BiConsumer` returned where a `BinaryOperator`
  is declared.

`Heap::get_field` (`gc/src/heap.rs:652`) opens with
`assert!(index < num_slots)`. **PREDICTED before the fix:**
`stream.gather(Gatherer.ofSequential(sup, integ))` in synthetic-jdk mode aborts
the VM with `field index 4 out of bounds (num_slots=3)`. Not a wrong answer — a
panic.

**Fix applied (`lib.rs:42679`):** register the missing overload here, in the
5-slot shape its siblings already use. Last-registration-wins shadows the
3-slot factory, so the class now has one producer shape for every reachable
factory. Chosen over widening `streams.rs` to 5 because that would leave two
copies of one slot map to drift again; chosen over widening the ask alone
because widening alone would produce a 5-slot object whose slots 2–4 disagree
with `streams.rs`'s own `finisher()` reader — the E28 §1.4 `VarHandle` lesson,
that a number edit which does not move the writes is not a fix.

**PREDICTED after:** that call path stops aborting and produces the same
`KIND_CUSTOM` gatherer `of(Integrator)` already produces. `pd_gather_custom`
(`lib.rs:42978`) null-checks initializer, integrator and finisher
independently, so the null combiner/finisher this overload leaves is the shape
it already handles.

Oracle note: `javap -p java.util.stream.Gatherer` on JDK 25.0.3 declares
**seven** static factories. This VM now serves four of them
(`of(Integrator)`, `ofSequential` ×2 arities in `lib.rs` plus the 3-arg). The
three unserved (`of(Integrator,BiConsumer)`,
`of(Supplier,Integrator,BinaryOperator,BiConsumer)`, `ofSequential(Integrator)`)
are a separate gap, recorded not fixed.

### 3.2 `java/security/MessageDigest` — 2 vs 4, undeclared, and a mode split

No `synthetic_stub_fields` arm, so in synthetic-jdk mode the ask is the width.

* `lib.rs:36958` asks **2**: `MD_FIELD_ALGO = 0`, `MD_FIELD_DATA = 1` (a `byte[]`
  accumulator on the object).
* `jca/message_digest.rs:170`/`:693` ask **4**: `FIELD_ALGO = 0`, the algorithm
  also written by name, and the accumulator in a process-wide side table.

`jca::register` runs from `register_essential_natives_with_shims`
(`lib.rs:19259`); `register_security_natives` runs from
`register_synthetic_overrides` (`lib.rs:24031`). So **synthetic-jdk mode gets
this file's 2-slot model and real-JDK/`--jdk-only` gets jca's**, and in real-JDK
mode the real class is loaded so `real` clamps both anyway.

**Not a live defect: nothing in `jca/message_digest.rs` indexes above slot 0**,
so a 2-slot receiver is never over-indexed. It is the §3.1 shape one reader
away from being live.

One consequence worth naming, which is a *coverage* split rather than a width
one: `lib.rs` registers only `getInstance(String)`, while `jca` also registers
the `(String,String)` and `(String,Provider)` overloads. In synthetic-jdk mode
those two overloads therefore mint a **jca-shaped** MessageDigest (accumulator
in the side table) which this file's winning `update`/`digest` bodies then read
at `MD_FIELD_DATA`. Recorded, not fixed — it needs a synthetic-mode
measurement, not a number edit.

### 3.3 `java/util/logging/Logger` — the same file holds both slot maps, and one of them is a wrong-type write

This is the §3-style find the brief's `Base64` example points at: **caught by
shape, not by count.**

`class_manager.rs:14191` declares `Logger` with **named, real-JDK-ordered
fields**: `config` 0, `manager` 1, `name` **2**, … `parent` **8**, … plus
`vm_internal_field(12)` commented *"VM-internal, anchored past the real layout:
LOGGER_FIELD_LEVEL"*. `logmanager.rs:157-162` matches it:
`LOGGER_FIELD_NAME = 2`, `LOGGER_FIELD_PARENT = 8`,
`LOGGER_FIELD_LEVEL = LOGGER_REAL_FIELDS = 12`, with its own comment recording
that the level *"used to sit on `manager`"*.

`lib.rs` uses **both** conventions:

* `:17405`, `:17426`, `:17597`, `:17603`, `:17625`, `:17632`, `:17852`, `:17899`
  use `crate::logmanager::LOGGER_FIELD_NAME` / `_LEVEL` / `_PARENT` — correct.
* `:36359-36360` declare a **second, local** `LOGGER_FIELD_NAME = 0` and
  `LOGGER_FIELD_LEVEL = 1`, used by `alloc_logger` / `native_logger_get` /
  `native_logger_get_global` / `native_logger_get_name` /
  `native_logger_get_level` / the `isLoggable` body at `:36455`.

Against the declaration, that second convention writes the **name String into
`config: Ljava/util/logging/Logger$ConfigurationData;`** and the **`Level`
object into `manager: Ljava/util/logging/LogManager;`** — the exact thing
`logmanager.rs`'s comment says was fixed there. Both are references, so the
collector is safe and no assert fires (the ask of 3 is clamped to 13); the
damage is that `Logger.getName()` and `Logger.getLevel()` read a different
object than `logmanager.rs`'s producers write.

**Why it is not live today:** those four functions are registered by
`logging_shims.rs::register_logging_natives` (a `use super::*` submodule of
`lib.rs`, which is why the names resolve), called at `lib.rs:24026`; and
`logmanager::register_logmanager_natives`, which registers `getLogger`,
`getName`, `getLevel` and `setLevel` on the same class with the slot-2/slot-12
map, is called at `lib.rs:24372` — **later, therefore winning** — and again from
`register_annotation_overrides` (`reflect_annotations.rs:203`) in real-JDK mode.
So the drifted family is **registered and shadowed**, i.e. dead.

Also stale in this file, and load-bearing for the next reader:
`lib.rs:17591` says *"Our `allocate_logger`-created synthetic loggers
(slot0=name, slot1=level, slot2=parent)"* while the code three lines below it
uses `LOGGER_FIELD_NAME = 2` / `_LEVEL = 12` / `_PARENT = 8`. Deliberately not
edited in this lane — see NOM E35-4, because deleting the constants and the
dead family is a bigger change than a comment and wants its own diff.

### 3.4 `java/io/UnixFileSystem` / `WinNTFileSystem` — a width that cannot matter, and a shape that does

Both are undeclared, sole-site, and the widths (3 / 4) are self-consistent. But
`lib.rs:36234-36243` populates them with `set_field_by_name("slash" / "colon" /
"userDir")` only. `get_field_by_name`/`set_field_by_name` resolve against the
class's declared fields, and an undeclared class fabricates anonymous `_fN`
slots — so in synthetic-jdk mode **all three writes are silent no-ops**, and the
object handed to `java/io/File.FS` carries nothing. This is E28 §2.3's lesson
(`[premise=guard]`: the field NAMES, not the slot count, are the blocker)
appearing a second time in a different subsystem. Recorded, not fixed: it needs
a declaration, which is `class_manager.rs` (NOM E35-5).

## 4. Three allocation sites in this file are dead

Found while sweeping, because a dead site's width is not worth arguing about:

* **`Class.getProtectionDomain` (`:13093`)** — the SAME triple is registered
  three times in one function: `:13017`, this closure, and `:13234`, the outer
  two both with `lang_class::native_class_get_protection_domain0`. Last-wins, so
  the closure's three allocations (`ProtectionDomain` 4, `URL` 13,
  `CodeSource` 2) never run. Its layout comment was also **wrong**: it claimed
  `URL` slot 0 = path / 1 = protocol / 2 = host, while the body 100 lines below
  writes the real JDK 13-field order (0 = protocol, 1 = host, 2 = port,
  3 = file, 6 = path) and documents it correctly. Comment replaced; the
  registration left in place, because the two bodies are not identical (this one
  has `jar:file:` handling the live one lacks) and deleting it is a behaviour
  question.
* **`ClassLoader.getSystemClassLoader` (`:39275`, asks **0**)** — shadowed by
  `classloader::register_classloader_natives` (`lib.rs:24226` >
  `register_java_lang_extras_natives` at `:24047`). This file already documents
  the shadowing at `:39119`; the zero-slot ask is therefore inert.
* **the `native_logger_*` family (`:36388-36440`, plus `alloc_logger`)** — §3.3.

`[dup nati]`. Two of the three would have read as width defects on a census that
did not ask which registration wins.

## 5. TASK 3 — the layouts checked against the real JDK, field ORDER and TYPES

Oracle: `javap -p` on **JDK 25.0.3+9**. Rows marked CLEAR were compared
field-by-field, not width-only.

| synthetic | this file's map | real JDK 25 instance fields, in order | verdict |
|---|---|---|---|
| `Base64$Encoder` | 0 `newline` (byte[]), 1 `linemax` (Int), 2 `isURL` (Int), 3 `doPadding` (Int) | `byte[] newline`, `int linemax`, `boolean isURL`, `boolean doPadding` | **CLEAR** — width, order and types all match |
| `Base64$Decoder` | 0 `isURL` (Int), 1 `isMIME` (Int) | `boolean isURL`, `boolean isMIME` | **CLEAR** — E14's one-Int-slot defect stays fixed |
| `java/net/URL` | 0 protocol, 1 host, 2 port(Int), 3 file, 4 query, 5 authority, 6 path, 7 userInfo, 8 ref, 9 hostAddress, 10 handler, 11 hashCode(Int), 12 tempState | identical, 13 fields | **CLEAR** — the comment at `:13182` is exact |
| `java/util/HexFormat` | 0 delimiter, 1 prefix, 2 suffix, 3 ucase(Int) | `String delimiter`, `String prefix`, `String suffix`, `boolean ucase` | **CLEAR** |
| `java/nio/charset/Charset` | 0 name, 1 aliases (ref[]), 2 aliasSet | `String name`, `String[] aliases`, `Set aliasSet` | **CLEAR** — and it dual-writes by name |
| `java/nio/charset/CodingErrorAction` | 0 name | `String name` (the only instance field) | **CLEAR** |
| `java/time/Instant` | 0 seconds (Long), 1 nanos (Int) | `long seconds`, `int nanos` | **CLEAR** — the reader at `:39881` accepts either tag defensively |
| `AtomicStampedReference$Pair` | 0 reference, 1 stamp | `T reference`, `int stamp` | **CLEAR** |
| `AtomicMarkableReference$Pair` | 0 reference, 1 mark | `T reference`, `boolean mark` | **CLEAR** |
| `java/util/logging/Level` | 0 name, 1 value (Int) | `String name`, `int value`, `String resourceBundleName`, … | **CLEAR for the two slots used** (a prefix of the real order) |
| `java/util/logging/Logger` | 0 name, 1 level *(the local constants)* | `config`, `manager`, `name`, … | **DIVERGENT** — §3.3 |
| `java/security/CodeSource` | written BY NAME (`location`, `certs`) | `URL location`, `CodeSigner[] signers`, `Certificate[] certs`, … | **CLEAR by construction** — `:13207` already records that raw slot 1 is `signers`, not `certs` |
| `java/security/ProtectionDomain` | slot 0 codesource, rest by name | `codesource`, `classloader`, `principals`, `permissions`, `hasAllPerm`, `staticPermissions` | **CLEAR** — `:13217`'s SBR-13 note has the slot-1 = `classloader` fact right |
| `java/util/HashSet` | 0 array, 1 size *(legacy fallback at `:31398`)* | `HashMap map` — **one** field | divergent by design; the real-layout arm 15 lines above writes the real `map` slot, and `build_package_set`'s doc already records the trade-off. Declared 16 so no OOB |

## 6. NOMINATIONS

### NOM E35-1 — `native-builtins/src/lang_class.rs`, `native-builtins/src/shared_secrets_bridge.rs` — the last three `java/lang/Module` sites still ask 2

E28 §1.2 asked whether the `lib.rs` ↔ `jboss` disagreement was the only one for
`Module`. **It was not.** With `:12053` fixed, three sites remain at 2 against a
declaration of 5:

* `lang_class.rs:18425` (`canonical_unnamed_module`)
* `lang_class.rs:18498` (the per-loader unnamed module)
* `shared_secrets_bridge.rs:506` (`jla_define_unnamed_module`)

All three are clamped by `num_fields.max(real)` and all three fire
`report_layout_alias("java/lang/Module", 2, 5)` per uncached call. The *reason*
they need a nomination rather than a shrug is the comment at
`shared_secrets_bridge.rs:505`:

```rust
    // Mirror `Class.getModule()` shape: 2-field synthetic Module, field 0 =
    // name (None = unnamed).
```

That comment is now false — `Class.getModule()` asks 5. `lang_class.rs:18427`
carries the same claim (*"the synthetic 2-field contract's slot 0"*). Change the
three literals to 5 and reword the two comments to name the declaration rather
than a peer site.

### NOM E35-2 — `native-builtins/src/phases_late.rs`, `servlet.rs`, `jboss_module_loader.rs` — `java/net/URL` is minted at two widths with no declaration

`java/net/URL` has **no `synthetic_stub_fields` arm**, so the ask is the width in
synthetic-jdk mode. Nine sites: **13** at `lang_class.rs:21732`,
`net_phase_e.rs:3695`, `jboss_module_loader.rs:2841`,
`phases_late/jar_manifest.rs:2897`; **6** at `phases_late.rs:3588`, `:3619`,
`:3671`, `servlet.rs:1307`, `jboss_module_loader.rs:2807`.

This is E28 §1.3's `ModuleDescriptor` shape exactly, and the tree already knows
it — `lang_class.rs:21722` says *"our 6-field `URL_FIELD_FULL` index collides
with `authority`, corrupting `URL.toString()`/`toURI()` for Spring Boot's
launcher"*. A 13-slot reader on a 6-slot URL is a `Heap::get_field` assert, not a
wrong answer. Two halves, either can land alone:

**(a)** add a `java/net/URL` arm to `class_manager::synthetic_stub_fields`
declaring the 13 real fields **by name** in the JDK 25 order verified in §5
(`protocol, host, port:I, file, query, authority, path, userInfo, ref,
hostAddress, handler, hashCode:I, tempState`). That alone makes every
under-request clamp up and silences the `Undeclared` report.
**(b)** move the five `URL_FIELD_FULL`-convention sites onto the 13-field map.
(a) without (b) is safe; (b) without (a) is not, because the 6-field sites'
slot 5 write would then land on `authority`.

### NOM E35-3 — `classloading/src/class_manager.rs` — two declaration comments state widths the code does not have

Both anchors verified unique in the working tree today. These are the comments a
reader consults *instead of* counting, which is what makes them worth a diff.

OLD (`:12324`):

```rust
        // Collections: ArrayList/Vector/Stack/CopyOnWriteArrayList = 2 fields (data, size)
```

NEW:

```rust
        // Collections: ArrayList/Vector/Stack/CopyOnWriteArrayList — the code
        // below says 4, not the "2 fields (data, size)" this comment claimed
        // until 2026-08-13. Real `java.util.ArrayList` declares `elementData`
        // and `size` plus `modCount` inherited from `AbstractList`; the fourth
        // slot is padding. The (data, size) pair IS the convention ~90 native
        // sites write at slots 0/1 — they are under-requests clamped up by
        // `num_fields.max(real)`, not a second layout.
```

OLD (`:12329`):

```rust
        // HashMap/HashSet/ConcurrentHashMap = 3 fields (buckets, size, capacity)
```

NEW:

```rust
        // HashMap/HashSet/ConcurrentHashMap — 16, not the "3 fields (buckets,
        // size, capacity)" this comment claimed until 2026-08-13. Five
        // different slot conventions write into these objects (0, 1, 2, 3 and 8
        // slot asks across ~18 sites); 16 is what makes every one of them an
        // under-request that `num_fields.max(real)` clamps up instead of an
        // out-of-bounds write. Real `java.util.HashSet` has ONE field, `map` —
        // see `build_package_set`'s doc comment for which callers need that
        // real shape and which need the array convention.
```

### NOM E35-4 — `native-builtins/src/lib.rs` (this lane's own file, deliberately not landed) — retire the second `LOGGER_FIELD_*` convention

§3.3. `:36359-36360`'s `LOGGER_FIELD_NAME = 0` / `LOGGER_FIELD_LEVEL = 1`
contradict `logmanager.rs`'s 2 / 12 and the named declaration at
`class_manager.rs:14191`, and the family that uses them
(`alloc_logger`, `native_logger_get`, `native_logger_get_global`,
`native_logger_get_name`, `native_logger_get_level`, the `isLoggable` body) is
registered by `logging_shims.rs:1780` and then shadowed by
`logmanager::register_logmanager_natives`. The right change is to delete the
constants and the six bodies and let `logging_shims` register nothing for those
triples — but "delete a registered native because a later registration wins" is
exactly the assumption `[dup nati]` says to verify against a run, and this lane
may not run. The stale comment at `:17591` (*"slot0=name, slot1=level,
slot2=parent"*) goes with it. Filed against this file so the next lane in it
inherits the reasoning rather than re-deriving it.

### NOM E35-5 — `classloading/src/class_manager.rs` — declare `java/io/UnixFileSystem` and `java/io/WinNTFileSystem` by name

§3.4. `lib.rs:36234` populates `java/io/File.FS` entirely through
`set_field_by_name("slash"/"colon"/"userDir")`, and with no declaration those
three writes resolve nothing in synthetic-jdk mode. Not offered as literal
replacement text: the real `UnixFileSystem`/`WinNTFileSystem` field sets differ
between the two, and the fix should be checked against `javap -p` per platform
rather than pattern-matched off the write list.

## 7. The lesson

**"Asked ≠ declared" is a bad alarm; "no declaration AND two slot maps" is the
alarm.** Fifteen of this file's forty classes carry a disagreement.
Fourteen of them are clamped up by `num_fields.max(real)` and cost a
`report_layout_alias` per call — including `java/lang/Thread`, where **all
seventeen sites in the tree** ask 5 against a declared 14 and none of them is
wrong on the heap. The one that aborts the VM is off by two, and the thing that
makes it different is that its class has no `synthetic_stub_fields` arm to clamp
against **and** two registrars had written two different maps for it.

**And the width sweep kept finding things that were not widths.** Ranking the
rows by "does this reach the heap" surfaced three dead registrations, two stale
declaration comments, a layout comment contradicted by its own closure, a
by-name populate against an undeclared class, and a second copy of a slot map in
the same file that already imports the correct one. `[1 of 10 callsites]` and
`[mock=slot table]` are the same rule seen from two sides: **the number is
checkable and therefore gets checked; the field's identity is not, and therefore
drifts.**
