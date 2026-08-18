# E28 / R11 — the three widths, and the catalog side effect whose real blocker is a field NAME

**Date:** 2026-08-13 **Lane:** E28
**Closes:** NOM E20-1 and NOM E20-2 in
`E20-R11-INTERSECTION-BLIND-GUARD-20260813.md` §6.
**Corrects:** `E16-R11-P59-MODULE-LAYER-TWIN-20260813.md` §1.4 bullet 1 and
`E20-R11-INTERSECTION-BLIND-GUARD-20260813.md` §4.2 — see §2.3.

**This lane did not build or run CratonVM, and did not run `cargo`.** Every
CratonVM "after" below is **PREDICTED**. Everything else is source read in this
working tree, plus one `rustc` compile of an isolated model (§2.4).

**Edits applied (owned file only):** `native-builtins/src/phases_late/reflect_invoke.rs`

| what | where (post-edit) |
|---|---|
| `ModuleLayer.boot()` allocates 2, not 1 | `:3024` |
| `ModuleLayer.modules()` delegates to the essential twin on a `nameToModule`-carrying receiver | `:3108-3116` |
| `ModuleLayer.findModule(String)` — same delegation | `:3163-3172` |
| `java/lang/Module` allocations 2 → 5 (three sites) | `:3133`, `:3183`, `:3477` |
| `java/lang/module/ModuleDescriptor` allocation 2 → 16 | `:3256-3258` |
| two stale doc pointers to a `native_module_get_descriptor` that does not exist | `:2818`, `:3352` |

**Nominations in §5.** None is required for the tree to compile.

---

## 1. TASK 1 — the widths, and the two numbers that are not the answer

### 1.1 `java/lang/ModuleLayer`: 1 → 2

Facts, re-derived rather than inherited:

* `javap -p java.lang.ModuleLayer` on JDK 25 declares **six** instance fields
  (`cf`, `parents`, `nameToModule`, `allLayers`, `modules`, `servicesCatalog`).
  So **neither 1 nor 2 is "the real width"**, and E20 §4.1 is right to say so.
* **2** is this VM's *declared synthetic* width:
  `classloading/src/class_manager.rs:15403`,
  `"java/lang/ModuleLayer" => instance_fields(2)`, whose fields are literally
  named `_f0`/`_f1` (`instance_fields`, `class_manager.rs:11929`).
* **2** is also what the essential twin allocates —
  `jboss_jdkspecific.rs:190` `MODULE_LAYER_FIELD_COUNT = 2`, used by
  `build_boot_layer`.
* The under-request was **not** an out-of-bounds hazard at the allocation, and
  writing it up as OOB would be wrong. `try_alloc_concurrent_synthetic`
  (`util_concurrent_ext.rs:983`) ends its success arm with
  `let n = num_fields.max(real)` — an under-request is clamped **up**, so
  `boot()` already handed back a 2-slot object.

What the `1` actually cost, both halves:

1. a `report_layout_alias("java/lang/ModuleLayer", 1, 2)` on **every** call
   (`layout_alias::classify(1, 2)` is `Some`), i.e. the workspace's
   layout-mismatch detector firing on a mismatch owned by this file;
2. the **failure arm**. When `ensure_class_initialized` errs,
   `try_alloc_concurrent_synthetic` takes
   `refused_class(ctx, class_name, num_fields)` →
   `try_ensure_synthetic_class(name, 1)` → `fabricate_class`, and
   `fabricate_class` writes `num_total_fields: num_fields` **verbatim**
   (`class_manager.rs:4249`) with an empty `fields` vec. That fabricates a
   genuinely 1-field `java/lang/ModuleLayer` **class**, memoised for the rest of
   the VM's life by the `get_loaded_class_id` early return at `:3984`. Every
   later `MODULE_LAYER_FIELD_COUNT` write into that class is then out of bounds
   by construction. This is `[Ok≠use]`: the fallback fabricates instead of
   failing, at the caller's guess of a width.

Nothing in the tree reads `ModuleLayer` slot 1 by index (`grep`ed). By name,
`build_boot_layer` writes `parents`/`nameToModule`/`modules` through
`set_field_by_name` — see §3 for why that matters more than the number.

**Verdict implemented: 2**, on the interchangeability argument, not on a
real-width argument. These triples are in the 18-member p59/essential
intersection; which body answers is decided by the VM mode, not by the caller.

### 1.2 `java/lang/Module`: 2 → 5, and a twin that disagrees with the declaration

| site | asks |
|---|---|
| `class_manager.rs:15407` (declaration) | **5** |
| `jboss_jdkspecific.rs:214` `MODULE_FIELD_COUNT`, used by `build_module` | **5** |
| `lib.rs:12044` — the essential `Class.getModule()` twin | **2** |
| p59 `modules()`, `findModule()`, `Class.getModule()` (before this lane) | **2** |

**This is the "twin and declaration disagree" case the brief asked to be
reported rather than assumed a typo.** The two essential registrars do not
agree with each other about this class: `jboss` asks 5, `lib.rs` asks 2, and the
declaration backs `jboss`. 5 is the only number that is never an under-request,
so p59 now asks 5 at all three sites; `lib.rs:12044` is NOM E28-2.

Effect on the object: none. `real = 5` in synthetic-jdk mode, so
`num_fields.max(real)` already produced a 5-slot object for every one of these.

### 1.3 `java/lang/module/ModuleDescriptor`: 2 → 16 — and this one was NOT clamped

This is the width that was actually wrong on the heap, and the reason is that
**there is no `java/lang/module/ModuleDescriptor` arm in
`class_manager.rs::synthetic_stub_fields` at all.** `create_synthetic_stub`
therefore gives the stub zero instance fields, `class_num_total_fields` answers
`0`, and `num_fields.max(0) == num_fields`: the request *is* the width.

| site | asks |
|---|---|
| `jboss_jdkspecific.rs:1642` `build_module_descriptor` — what the essential `Module.getDescriptor` at `lib.rs:12229` reaches via `build_synthetic_module_descriptor` (`lib.rs:31527`) | 16 |
| `jboss_jdkspecific.rs:1733` | 16 |
| `reflect_annotations.rs:1672`, `:2048`, `:2110`, `:2178` | 16 |
| p59 `Module.getDescriptor` (before this lane) | **2** |

Seven sites at 16, one at 2. One class, two object widths, decided by which
registrar ran. The consumer that makes it matter:
`reflect_annotations.rs:1419` (`native_module_descriptor_is_open`, the essential
`ModuleDescriptor.isOpen` twin) reads `ctx.get_field(this, 1)` **unguarded** as
its fallback — p59's own `isOpen` guards with
`object_num_fields(this) > 1`, the essential one does not.

Slots 0 (`name`) and 1 (flags) keep their meaning at 16, so p59's own writes and
reads are unchanged.

**What this does NOT fix:** `layout_alias::classify(n, 0)` reports `Undeclared`
for *any* `n`, so the report still fires. Silencing it needs a declaration —
NOM E28-1.

### 1.4 The rest of the file's synthetic allocations, audited

Every `try_alloc_concurrent_synthetic` call in
`native-builtins/src/phases_late/reflect_invoke.rs`, against its declared width
(`class_manager::synthetic_stub_fields`) and its out-of-file twins:

| class | this file asks | declared | twins ask | verdict |
|---|---|---|---|---|
| `java/lang/ModuleLayer` | ~~1~~ **2** | 2 | 2 (`jboss:190`) | **fixed** |
| `java/lang/Module` | ~~2~~ **5** ×3 | 5 | 5 (`jboss:214`) / 2 (`lib.rs:12044`) | **fixed**; twins disagree → NOM E28-2 |
| `java/lang/module/ModuleDescriptor` | ~~2~~ **16** | *(none)* | 16 ×7 | **fixed**; no declaration → NOM E28-1 |
| `java/lang/Integer`/`Long`/`Float`/`Double` | 1 | 1 | — | agrees |
| `java/lang/StackWalker$StackFrame` | 8 | 8 | — | agrees |
| `java/lang/StackTraceElement` | 4 | *(none)* | 4 (`lang_misc`, `lang_stackwalker`) | agrees with twins |
| `java/util/stream/Stream` | 1 | *(none)* | 1 (`streams.rs`, `nio_file.rs`) | agrees with twins |
| `java/lang/constant/ClassDesc` | 1 ×3 | *(none)* | 1 (`classfile_api.rs:462`) | agrees with twins |
| `java/lang/constant/MethodTypeDesc` | 1 ×2 | *(none)* | 1 (`classfile_api.rs:364`) | agrees with twins |
| `java/lang/Package` | 6 | *(none)* | **12** (`lang_class.rs:18099`, `:18531`) | **finding, not fixed** — see below |
| `java/lang/invoke/VarHandle` | 8 (`VH_META_NUM_FIELDS`) ×4, **6** ×3 | *(none)* | 6 (`lang_invoke` `VH_FIELD_COUNT`), 3 (`panama.rs:4089`) | **finding, not fixed** — see below |
| `java/util/HashSet` | 3 ×2 | **16** | *(real ctor)* | **finding, not fixed** — see below |
| `java/util/HashMap` | 3 ×2 | **16** | — | same shape as HashSet |
| `java/util/Optional` | 1 ×2 | **2** | 1 (`jboss` `wrap_optional_present`) | agrees with twin, under declaration |

Three rows are deliberately **reported and not changed**, because in each case
the number is a native's own slot map and changing it alone would silence a
detector rather than fix a layout:

* **`java/lang/Package` 6 vs 12.** No declaration, so `real = 0` and the ask is
  the width: p59 mints 6-slot Packages, `lang_class.rs` mints 12-slot ones.
  Nothing in the tree indexes a `Package` above slot 2 (`lang_class` writes 0
  and 2; both by index and, for 0, also by name), so this is not live today. It
  is the same *shape* as §1.3 and is the next one to detonate if a `Package`
  native ever grows a slot.
* **`java/lang/invoke/VarHandle` 8 vs 6, inside one file.** The
  `ConstantBootstraps` factories at `:4385`, `:4409`, `:4429` allocate
  `VH_FIELD_COUNT`-worth (6) and stamp slots 0–4, while this file's other four
  factories allocate `VH_META_SLOT_COUNT` (8) and additionally stamp
  `VH_META_VAR_TYPE` (6) and `VH_META_COORD0` (7). The three narrow ones **hold
  the two `Class` mirrors** (`args[3]` declaring class, `args[4]` field type) and
  drop them, so `varType()`/`coordinateTypes()` on their handles take the
  documented "no metadata → REFUSE" path. Widening 6→8 alone changes nothing:
  the accessors guard on slot *content* (`VH_VAR_TYPE` holding an object), not
  on width. The real fix is to stamp the mirrors, which makes two accessors
  start answering where they used to refuse — a behaviour change that wants a
  measurement, not a number edit.
* **`java/util/HashSet` 3 vs declared 16** (and `HashMap` likewise; the comment
  above `class_manager.rs:12330` still says *"3 fields (buckets, size,
  capacity)"* while the code says `instance_fields(16)` — a stale comment worth
  a reader's attention). The 3 is this file's `(array, size, capacity)`
  convention, written at slots 0/1/2 by `build_string_set` (`:2796`). Raising
  the ask to 16 would silence `report_layout_alias(3, 16)` without changing a
  single write. See §4.5 for the divergence that *is* live here.

## 2. TASK 2 — the `servicesCatalog` side effect

### 2.1 What the side effect is, exactly

`jboss_jdkspecific::native_module_layer_modules` (`:1084`) has an arm that p59's
body has no equivalent of. Entered when the receiver's class resolves a field
named `nameToModule` **and** that field is non-null, it:

1. builds the module set from `nameToModule.values()` via a real
   `new HashSet(Collection)`;
2. calls `ServicesCatalog.create()`, iterates the same values, and calls
   `ServicesCatalog.register(Module)` for each;
3. **writes the catalog back into the receiver's `servicesCatalog` field**;
4. caches the set into the receiver's `modules` field.

Step 3 is the side effect. Step 2's `register(Module)` reads
`descriptor.provides()`, so a module declaring no services is a no-op rather
than a special case.

### 2.2 What depends on it

`native-builtins/src/service_loader.rs:775-790`. `ServiceLoader.load(layer,
service)` invokes `ModuleLayer.modules()` **for the side effect alone** — it
discards the returned `Set` (`let _ = ctx.invoke(...)`) and then reads
`get_field_by_name(layer, "servicesCatalog")` to call `findServices`. With no
catalog, that read yields `Object(None)` and the layer contributes **zero**
providers. The visible shape is the one `jboss_jdkspecific.rs:347-357` records:
`RJdkModule.java` failing with `AssertionError: module service providers: []`.

### 2.3 The correction: in synthetic-jdk mode, **neither** body performs it

E16 §1.4 and E20 §4.2 both read as *"jboss derives and caches the catalog; p59
ignores its receiver; so in synthetic mode that read finds nothing"* — which
invites the conclusion that p59 winning is what loses the catalog. It is not.

`ModuleLayer.modules()`'s catalog arm is gated on
`ctx.get_field_by_name(*layer, "nameToModule")` being `Object(Some(_))`.
`get_field_by_name` (`vm/src/vm/vm_exec.rs:10980`) resolves the name in the
receiver's class hierarchy and returns `Value::Object(None)` when it does not
resolve. In synthetic-jdk mode — the only mode `register_p59_module` runs in
(`vm_init.rs:1932`) — `java/lang/ModuleLayer` is the fabricated stub whose two
fields are named `_f0` and `_f1`. There is no `nameToModule`.

So **jboss's body, run on the same receiver, takes the same false branch.** The
same fact makes `build_boot_layer`'s three `set_field_by_name` writes
(`parents`, `nameToModule`, `modules`) silent no-ops, and
`populate_boot_layer_modules`'s `nameToModule.put` / `modules.add` blocks
unreachable for the same reason.

**The runtime blocker on the services catalog in synthetic-jdk mode is the field
NAMES, not which registrar won the slot.** `[premise=guard]`: the divergence is
real as a code divergence and, on today's synthetic `ModuleLayer`, inert as a
runtime divergence.

One route survives that in synthetic mode and is worth naming because it is not
gated on the layer's fields at all: `populate_boot_layer_modules` also calls
`register_module_in_loader_catalog` (`jboss_jdkspecific.rs:507`), which
registers each module in the **system class loader's** catalog — the door
`ServiceLoader.load(Class)` uses. p59's `boot()` reaches neither
`populate_boot_layer_modules` nor that call (§4.1).

### 2.4 The edit — call the twin, do not copy it

Both `modules()` and `findModule(String)` now begin with the twin's **own entry
condition** and, when it holds, hand the whole call to the twin:

```rust
if let Some(Value::Object(Some(layer))) = args.first() {
    if matches!(
        ctx.get_field_by_name(*layer, "nameToModule"),
        Value::Object(Some(_))
    ) {
        return crate::jboss_jdkspecific::native_module_layer_modules(ctx, args);
    }
}
```

Gating on the twin's own predicate is what makes the two bodies unable to
disagree about *when* the arm applies — the consolidation, rather than a second
copy of the catalog loop.

**Delegating unconditionally would be a regression, and this is the reason the
"just call the other one" answer needs a gate.** The twin's fallback, for a
receiver with neither `nameToModule` nor `modules`, builds an **empty**
`java/util/HashSet` through its real constructor. p59's registry walk is the
only thing that answers a synthetic 2-slot boot layer with the actual module
list, so an unconditional delegation would take `ModuleLayer.boot().modules()`
in synthetic mode from N modules to 0.

Borrow/binding shape verified by compiling an isolated model with
`rustc --edition 2021` (`scratchpad/e28/delegate.rs` — compiles, prints
`delegated=true fallback=true`, i.e. both arms select correctly). The repo
itself was not built: this lane may not run `cargo`.

**PREDICTED effect: none observable today, in either mode.** The gate is false
for every `ModuleLayer` that exists in synthetic-jdk mode (§2.3), and the
registrar does not run in `--real-jdk`/`--jdk-only`. What changes is that the
two bodies now agree by construction on the one input class where they *can*
differ, and that the agreement goes live the moment NOM E28-1 lands.

## 3. TASK 3 — the 18 intersection triples, TWIN by TWIN

`SHARED FN` rows are safe by construction — one `fn` item, both registrations.
The three are `Module.canRead` (`native_module_can_read`),
`Module.addExports` (`native_module_add_exports`) and `Module.addOpens`
(`native_module_add_opens`); no diff possible, nothing to report.

The fifteen `TWIN` rows, read on both sides:

| triple | essential body | agree? |
|---|---|---|
| `ModuleLayer.boot()` | `jboss:712` | **NO — §4.1, the largest divergence in the set** |
| `ModuleLayer.modules()` | `jboss:1084` | **NO — §2; now delegates on the twin's own predicate** |
| `ModuleLayer.findModule(String)` | `jboss:721` | **NO — §4.2; now delegates on the same predicate** |
| `Module.getName()` | `jboss:997` | **NO, and both are right in their own mode — §4.3** |
| `Module.getLayer()` | `jboss:1057` | **NO — §4.3; the twin also lazily seeds the boot layer** |
| `Module.getPackages()` | `jboss:1014` | **NO — §4.5, two different Set shapes** |
| `Module.getDescriptor()` | `lib.rs:12229` | **NO — §4.4** |
| `Module.isExported(String)` | `lib.rs:12145` | effectively yes — §4.6 |
| `Module.isExported(String,Module)` | `lib.rs:12160` | effectively yes — §4.6 |
| `Module.isOpen(String)` | `lib.rs:12179` | effectively yes — §4.6 |
| `Module.isOpen(String,Module)` | `lib.rs:12194` | effectively yes — §4.6 |
| `ModuleDescriptor.name()` | `reflect_annotations:2220` | **NO — §4.7** |
| `ModuleDescriptor.isAutomatic()` | `reflect_annotations:1394` | bodies identical; **null receiver differs** — §4.7 |
| `ModuleDescriptor.isOpen()` | `reflect_annotations:1408` | bodies one guard apart; **null receiver differs** — §4.7 |
| `Class.getModule()` | `lib.rs:11983` | **NO — §4.8** |

**Zero of fifteen agree outright.** Four (§4.6) agree on every input that can
occur; two more agree on the body and differ only on a null receiver. That is
the number the guard next door cannot see, and it is why `SHARED FN` vs `TWIN`
is the useful column.

One difference runs through the whole `ModuleDescriptor` group and is worth
stating once: p59 extracts its receiver with `obj_arg(args, 0)?`
(`lib.rs:25230`), which **throws NPE** on a null, while all three essential
`ModuleDescriptor` bodies open with a `match … _ => return Ok(false/null)` that
**fabricates an answer** for a null receiver. HotSpot throws NPE before an
instance method body runs, so p59 is the one that matches the oracle here and
the essential bodies are the `[Ok≠use]` half. The four `Module.isExported` /
`isOpen` triples do **not** have this split — both sides use `obj_arg` there.

## 4. The diffs

### 4.1 `ModuleLayer.boot()` — p59 has no memo, so `boot() == boot()` is false

The essential twin is `build_boot_layer`, and its doc comment states the defect
it was written to fix:

> `ModuleLayer.boot()` is a singleton — `ModuleLayer.boot() == ModuleLayer.boot()`
> and `someModule.getLayer() == ModuleLayer.boot()` are both spec'd identities…
> This used to allocate a FRESH layer on every call, so both comparisons were
> always false (measured: `regression-suite/src/RJdkModule.java:60` fails with
> "module must be in the boot layer" in real-jdk AND jdk-only, while HotSpot 25
> passes). Memoise per VM instead.

p59's `boot()` is **three lines and allocates a fresh layer on every call.** It
is the pre-fix body, still shipping, in the one mode where it wins. It also
skips, in order: the per-VM memo keyed on `vm_identity()`, the `add_global_root`
that keeps the layer alive across a moving GC, `parents`/`nameToModule`/
`modules` initialisation, `populate_boot_layer_modules`, and
`register_module_in_loader_catalog` — the last of which is **not** gated on any
`ModuleLayer` field name and would therefore do real work in synthetic mode.

**Not fixed here, deliberately.** Delegating to `build_boot_layer` calls
`new_initialized_object(ctx, "java/util/ArrayList", "()V", …)` three times, and
in synthetic-jdk mode those are constructor invocations on fabricated stubs
whose method tables come from `synthetic_stub_ctor_methods`. If any fails,
`build_boot_layer` returns `Err` and `ModuleLayer.boot()` **throws** where it
today always succeeds — a `[flag≠mode drops it]` shape, and exactly the
"decide whether synthetic-jdk mode should use jboss's boot layer at all" call
that E16 §5.1 said needs a synthetic-mode measurement. NOM E28-3.

### 4.2 `ModuleLayer.findModule(String)`

Before this lane, four differences. p59: returns an empty `Optional` for a null
or non-`String` argument; does not validate the module name; consults only the
boot `ModuleRegistry`; never looks at the receiver. jboss: throws
`NullPointerException` for null and `IllegalArgumentException` for a non-String
(matching HotSpot), runs `validate_module_name`, treats a receiver carrying
`nameToModule` as authoritative **in both directions** (a miss there is a real
absence), and only then falls back to the registry — behind a
`registry_populated` guard keyed on `java.base` being registered, so that an
unpopulated registry keeps the legacy permissive fabrication.

This lane closes the receiver half. The argument-validation half is left,
recorded here: on a synthetic boot layer, p59 still answers `Optional.empty()`
for `findModule(null)` where HotSpot throws.

### 4.3 `Module.getName()` and `Module.getLayer()` — a mode split, not a bug

p59 reads `get_field(this, 0)` / `get_field(this, 1)`; jboss reads
`get_field_by_name(this, "name")` / `"layer"`. In synthetic-jdk mode the
`java/lang/Module` stub's fields are `_f0…_f4`, so **jboss's `getName` would
return null there** and p59's is the correct body; in real-JDK mode the real
class has the named fields at different indices and p59 is not registered. Both
are right where they run, and that is the honest reading — but it also means
these two bodies are **not** interchangeable and cannot be made so by a width
change. `getLayer` additionally diverges in substance: jboss lazily seeds an
unset `layer` with the boot layer and writes it back; p59 returns whatever slot
1 holds, which `modules()` deliberately leaves unset ("to avoid infinite
recursion"), so p59's `getLayer()` answers **null** for every Module that
`modules()` produced.

### 4.4 `Module.getDescriptor()`

p59 fabricates a descriptor unconditionally, carrying only the name and a zero
flags word. The essential twin: returns an existing `descriptor` field if
present; returns **null** for an unnamed module, matching real
`Module.getDescriptor()`; otherwise builds through
`build_synthetic_module_descriptor` → `jboss::build_module_descriptor`, which
answers `requires`/`exports`/`opens`/`uses`/`provides`/`packages` from the boot
`ModuleRegistry`, and caches it back onto the Module. p59 answers a non-null
descriptor for the **unnamed** module, where HotSpot answers null. Recorded, not
changed: the null answer is what `lib.rs:12229`'s comment says NPE'd
Elasticsearch's `ProviderLocator.checkUses` in the other direction, and picking
between them wants the measurement this lane cannot take.

### 4.5 `Module.getPackages()` — two different `Set` shapes

p59 → `build_string_set` (`:2796`), a hand-rolled 3-slot synthetic `HashSet`
with `(array, size, capacity)` at slots 0/1/2. jboss → `build_package_set` →
`crate::build_real_layout_string_hashset`, a real `HashMap`-backed `HashSet`.
`build_package_set`'s own doc comment states why:

> Real `java.util.HashSet` has exactly one instance field — `transient
> HashMap<E, Object> map;` — so a (array, size, capacity) 3-slot convention here
> writes an `Object[]` into what real bytecode's `size()`/`iterator()`/
> `stream()` (all delegating to `this.map`) expect to be an actual `HashMap`.

In synthetic-jdk mode the 3-slot convention is what the collection natives read,
so p59's shape works there. The divergence is nonetheless the documented
wrong-shape one, in a helper (`build_string_set`) that this file uses for other
surfaces too. Also: the two answer different *contents* — jboss falls back to
`BOOT_JDK_PACKAGES` (permissive, "callers check `.contains`") for a Module it
did not build, p59 returns whatever `ctx.module_packages` holds, i.e. possibly
empty.

### 4.6 `Module.isExported` ×2 and `isOpen` ×2 — the four that effectively agree

Both sides resolve the receiver's module name, slash-normalise the package, and
call the same four `NativeContext` predicates
(`is_package_exported_unqualified`, `is_package_exported_to`,
`is_package_open_unqualified`, `is_package_open_to`). One difference, and it is
the by-name/by-index split again: `lib.rs`'s `module_name_of_mirror` tries
`get_field_by_name(m, "name")` **first** and falls back to slot 0, while p59's
`read_module_name` is the slot-0 reader. In synthetic mode the named field does
not resolve, so `module_name_of_mirror` falls through to the same slot 0 and the
two agree. Recorded as "effectively yes" rather than "yes" for that reason.

### 4.7 `ModuleDescriptor.name()` / `isAutomatic()` / `isOpen()`

* `isAutomatic()`: p59's closure and `native_module_descriptor_is_automatic`
  have the same body (`get_field_by_name(this, "automatic")`, else 0). They
  differ only in the null-receiver arm described above. **The closest agreement
  in the set.**
* `isOpen()`: same body down to the slot-1 flags fallback, except p59 guards it
  with `object_num_fields(this) > 1` and the essential twin does not. With §1.3
  landed, every descriptor in the tree is ≥ 2 slots wide, so the guard is now
  always true and the two answer the same. Before §1.3, p59 built 2-slot
  descriptors and the guard was equally always true — the guard was protecting
  against a width nothing produced, while the unguarded twin was the one exposed
  to a hypothetical narrow one.
* `name()`: **diverge.** p59 reads slot 0 and returns whatever is there,
  including null. The essential twin reads by name and, on a miss,
  **fabricates** the string `"synthetic"` and writes it back "so subsequent
  reads see a stable identity". A descriptor built by p59 and read through the
  essential body therefore answers `"synthetic"` for its name; read through
  p59's body it answers the real name. Recorded, not changed — `[Ok≠use]` in
  the essential body, not in mine.

### 4.8 `Class.getModule()` — two independent canonical caches, and one missing arm

Both cache per module name (`get_cached_module_mirror`/`cache_module_mirror`),
both read the reflected class through `class_id_from_mirror` with a
`class_id_of_object` fallback, both pin across the `create_string`, and both
dual-write slot 0 and the real `name` field. The essential twin has one arm p59
does not: for `module_name == None` it routes through
`lang_class::unnamed_module_for_loader`, giving a **per-loader** unnamed module
with a non-null `loader` field, because HotSpot's unnamed module is per-loader
and caller-sensitive APIs (`ResourceBundle.getBundle(String)` above all) derive
a search loader from it. p59 caches a single unnamed mirror for all loaders.
Recorded, not changed: `unnamed_module_for_loader` and
`native_class_get_class_loader` are `lang_class.rs`, not this lane's file, and
the change is a behavioural one in the mode with no real class library.

## 5. NOMINATIONS

### NOM E28-1 — `classloading/src/class_manager.rs` — declare `ModuleLayer`'s and `ModuleDescriptor`'s fields by NAME

Owned by the `class_manager.rs` lane. This is the nomination that makes §2's
delegation live and closes §1.3's residual `Undeclared` report. It is also the
`[mock=slot table]` rule at VM scale: **declare the field, or every by-name
access is measuring the absence of a declaration.**

Two independent halves; either can land alone.

**(a) `java/lang/module/ModuleDescriptor` — currently has no arm at all.** Add
one next to the `java/lang/Module` arm at `:15407`. Anchor verified unique in
the working tree today.

OLD:

```rust
        "java/lang/Module" => instance_fields(5),
```

NEW:

```rust
        "java/lang/Module" => instance_fields(5),

        // `java/lang/module/ModuleDescriptor` had NO arm here, so the
        // fabricated stub declared zero instance fields, `real` was 0, and
        // `num_fields.max(real)` left every caller's request as the literal
        // object width. Eight allocation sites, seven asking 16 and one (p59's
        // `Module.getDescriptor`) asking 2, therefore produced two different
        // object widths for one class — see
        // docs/known-issues/jdk-only/E28-R11-P59-MODULE-WIDTHS-AND-CATALOG-20260813.md §1.3.
        //
        // Slot 0 = `name` and slot 1 = `open` are NOT arbitrary: they are the
        // indices `phases_late/reflect_invoke.rs`'s `ModuleDescriptor.name()`
        // and `isOpen()` already read, and `open` is the name
        // `reflect_annotations.rs`'s `native_module_descriptor_is_open` and
        // `native_module_builder_build` write by name. Declaring them makes the
        // by-index and by-name readers address the SAME slot instead of two
        // disjoint views of one object. `pad_to` keeps the remaining 14 slots
        // the sixteen-slot callers rely on.
        "java/lang/module/ModuleDescriptor" => pad_to(
            vec![
                named_field("name", "Ljava/lang/String;"),
                named_field("open", "I"),
            ],
            16,
        ),
```

**(b) `java/lang/ModuleLayer` — the two `_fN` slots are the reason the services
catalog is unreachable in synthetic-jdk mode.** `build_boot_layer`,
`populate_boot_layer_modules`, `native_module_layer_modules` and
`service_loader.rs:775` all address this object through
`parents` / `nameToModule` / `modules` / `servicesCatalog`, and **none of those
names is declared**, so every write is a no-op and every read is
`Object(None)` (§2.3).

This half is **not offered as literal replacement text**, because it is not
safe as one: `native_module_layer_boot` in this file and `register_p59_module`'s
`boot()` both write the boot flag to raw **slot 0**, and any named-field
declaration that puts `parents` at slot 0 silently retargets that write. The
change has to move in step with both `boot()` bodies — put the boot flag in a
declared named field of its own at slot 0, then `parents`, `nameToModule`,
`modules`, `servicesCatalog` — and it changes `ServiceLoader` behaviour in
synthetic-jdk mode, so it wants the measurement §2.3 says nobody has taken. It
is written down here so the next reader does not have to re-derive that the
field names, not the registration order, are what is holding the catalog shut.

### NOM E28-2 — `native-builtins/src/lib.rs:12044` — the essential `Class.getModule()` asks 2 where the declaration and the other essential registrar say 5

Owned by the `lib.rs` lane. Anchor verified unique in the working tree today.
Not a correctness bug — `num_fields.max(real)` already yields 5 — but it is one
of the two disagreeing twins §1.2 reports, and it fires
`report_layout_alias("java/lang/Module", 2, 5)` on every uncached
`Class.getModule()`.

OLD:

```rust
            let m_obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Module", 2)?;
```

NEW:

```rust
            // 5, not 2: `class_manager.rs:15407` declares the synthetic
            // `java/lang/Module` as `instance_fields(5)` and
            // `jboss_jdkspecific.rs:214` allocates `MODULE_FIELD_COUNT = 5`.
            // The object was 5 slots wide either way (`num_fields.max(real)`);
            // asking 2 only fired `report_layout_alias(.., 2, 5)` on every
            // uncached call. The synthetic-mode twin of this triple
            // (`phases_late/reflect_invoke.rs`) now asks 5 as well — see
            // docs/known-issues/jdk-only/E28-R11-P59-MODULE-WIDTHS-AND-CATALOG-20260813.md §1.2.
            let m_obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Module", 5)?;
```

### NOM E28-3 — `native-builtins/src/phases_late/reflect_invoke.rs` — p59's `boot()` is the pre-memo body (this lane's own file, deliberately not landed)

§4.1. `ModuleLayer.boot() == ModuleLayer.boot()` is false in synthetic-jdk mode,
which is the exact defect `build_boot_layer`'s doc comment says was measured
failing at `regression-suite/src/RJdkModule.java:60`. The fix is to delegate to
`native_module_layer_boot`, and it must not land blind: `build_boot_layer` runs
three real constructors (`java/util/ArrayList`, `java/util/HashMap`,
`java/util/HashSet`) and returns `Err` if any fails, turning a
`ModuleLayer.boot()` that today always succeeds into one that can throw in the
mode with no class library. Needs a synthetic-mode run. Filed against this file
so the next lane in it inherits the reasoning rather than the three lines.

### NOM E28-4 — `native-builtins/src/phases_late.rs` `P59_AND_ESSENTIAL` — three rows can be re-classified

Owned by the E20 lane. The reason column can now carry the answer instead of
the question for:

* `ModuleLayer.modules()` — no longer "KNOWN DIVERGENT" in the sense E20 meant.
  The catalog arm is now reached by delegation, and §2.3 establishes that on
  today's synthetic `ModuleLayer` **neither** body performed the side effect.
* `ModuleLayer.findModule(String)` — receiver-authority half closed by
  delegation; argument-validation half still divergent (§4.2).
* `ModuleLayer.boot()` — the divergence is the **memo and the boot-layer
  population**, not the width. The width is closed; NOM E28-3 is the rest.

`ModuleDescriptor.isAutomatic()` is the one row in the whole set that can be
marked *bodies verified identical* — with the caveat that its null-receiver arm
is not (§3, closing paragraph), which is itself worth a reason-column word,
because it is the essential side that diverges from HotSpot there, not p59's.

## 6. The lesson

**A width is three claims, and they are allowed to disagree with each other.**
For one class there is what the JDK declares (6 for `ModuleLayer`), what this VM
declares for its stand-in (2), and what each native asks for (1, and 2, and — for
`Module` — 2 from one essential registrar and 5 from the other). Only the middle
one is checkable by the mismatch detector, and only when it exists: with no
`synthetic_stub_fields` arm the detector reports `Undeclared` for every input, so
`ModuleDescriptor` had eight callers, two object widths and a permanently-firing,
permanently-uninformative report.

**And the thing that made the divergence inert is the thing that has to change
to fix it.** `servicesCatalog` was written up twice as "p59 wins and drops the
side effect". It does drop it — but the side effect could not have fired anyway,
because the synthetic `ModuleLayer` declares `_f0`/`_f1` and every participant
in that code path addresses it by name. Consolidating the two bodies is right
and cheap; it is the *declaration* that decides whether the consolidated body
ever does anything. `[reach≠defect]` — the fix that was nominated would have
been landed, measured as a no-op, and mis-read as evidence the defect was
imaginary.
