# E16 / R11 — NOM E-7's "dormant twin" was not dormant, and not dormant for the stated reason

**Date:** 2026-08-13 **Lane:** E16 **Closes:** `E4-R11-CLASS-PATH-MODULE-BOOT-LAYER-FIX-20260813.md`
NOM E-7 (the unfiltered `ModuleLayer.modules()` / `findModule` pair in
`register_p59_module`). **Corrects** that record's account of *why* the pair is
inert, and its verdict that it is inert everywhere.

**This lane did not build or run CratonVM.** Every CratonVM "after" is
**PREDICTED**. The HotSpot 25 transcript in §3 is a measurement taken on this
host today. The registration facts in §1 come from source read in this working
tree plus the stored `--dump-native-registry` census at
`scratchpad/p1/reg.json` (schema 4, `"mode": "compatible"`, 11 748 rows).

**Edit applied (owned file only):** `native-builtins/src/phases_late/reflect_invoke.rs`
— the class-path-only gate on `ModuleLayer.modules()` (`:2996`) and
`ModuleLayer.findModule(String)` (`:3055`), reusing the accessor E4 added.
No triple added, none removed.

---

## 1. Was it dormant? Answer: **it depends on the mode, and NOM E-7 named the wrong mechanism in both.**

NOM E-7 says: *"It is dead today — `register_jboss_jdkspecific` wins
last-writer-wins."* Neither half survives contact with the call graph.

### 1.1 Line numbers

Verified. In the current working tree the pair is at
`native-builtins/src/phases_late/reflect_invoke.rs:2996-3040` **before** this
lane's edit, exactly as written — `modules()` at `:2996`, `findModule` at
`:3017`, closing `);` at `:3040`. (NOM E-7 gives the path as
`native-builtins/src/reflect_invoke.rs`; the file is under `phases_late/`.
Same file, and the same record cites the correct path elsewhere.)

### 1.2 In `--real-jdk` and `--jdk-only`: dead, but by ABSENCE, not by being overwritten

`reg.json` holds **one** row for each of the three `java/lang/ModuleLayer`
triples, all owned by `jboss_jdkspecific.rs`, all with `"overwrote": null` and
`"owns_slot": true`:

| class | name | registered_by | overwrote | owns_slot |
|---|---|---|---|---|
| `java/lang/ModuleLayer` | `boot` | `jboss_jdkspecific.rs:1964` | `null` | true |
| `java/lang/ModuleLayer` | `modules` | `jboss_jdkspecific.rs:1982` | `null` | true |
| `java/lang/ModuleLayer` | `findModule` | `jboss_jdkspecific.rs:1970` | `null` | true |

That census **does** record losers: 1 217 of its rows carry `owns_slot: false`,
and the winner of each such slot carries a non-null `overwrote`. Two examples
in this very neighbourhood — `java/lang/Module.getResourceAsStream`
(`jboss_jdkspecific.rs:2041` loses, `lib.rs:18754` wins with
`"overwrote": "bridge"`) and `StackWalker$StackFrame.getMethodType`
(`reflect_invoke.rs:2189` loses, `:2435` wins). So a p59 registration that had
been overwritten **would be visible as a second row**. There is none.

The reason is the call graph, not precedence:

```
register_p59_module              (reflect_invoke.rs:2981)
  <- register_phase59_natives    (phases_late.rs:1906)          -- sole caller
  <- register_synthetic_overrides(lib.rs:21558)                  -- #[cfg(feature="synthetic-jdk")]
  <- register_builtins           (lib.rs:21547)
  <- vm_init.rs:1934, inside `if config.use_synthetic_jdk { ... }`
```

`--real-jdk` and `--jdk-only` both take the `else` arm at `vm_init.rs:2001`, so
`register_p59_module` is **never called** in either. The neighbouring
`register_p59_stackwalker` *does* appear in the compatible-mode census because
it has a **second** caller, `register_essential_natives_with_shims`
(`lib.rs:9143`) — that asymmetry is what makes "this file's rows are in the
dump" a false comfort. `register_p59_module` has no such second caller
(`grep -rn "register_p59_module(" --include=*.rs`: two hits, `phases_late.rs:1906`
and a unit test at `phases_late.rs:10008`).

### 1.3 In synthetic-jdk mode: **LIVE, and it is p59 that wins**

`register_builtins` is:

```rust
register_essential_natives(registry);   // lib.rs:21549 -> jboss at lib.rs:10004
register_synthetic_overrides(registry); // lib.rs:21551 -> phase 59 -> register_p59_module
```

`register_jboss_jdkspecific` has exactly one production call site,
`lib.rs:10004`, and it is **inside** `register_essential_natives_with_shims`.
So whenever `register_p59_module` runs at all, it runs **after** jboss and
overwrites it. Last-writer-wins points the other way from NOM E-7's claim.

**Verdict: the twin was not dormant. It is absent in the two modes the corpora
run, and it is the WINNER in the mode it exists in.** "Dead by registration
order" was true in no mode.

Corroborating in-tree, and independently of this lane: the guard at
`phases_late.rs:10006` (`phase59_module_vs_essential_natives`) already describes
p59 as *"synthetic-jdk-only — dead code in the default `cratonvm-cli` build"* —
i.e. mode-scoped, never "loses a race".

### 1.4 What it wins is not only a label

Two consequences that only exist in synthetic-jdk mode, both of which follow
from p59 being the last writer:

* `ServiceLoader.load(layer, service)` calls `ModuleLayer.modules()` for its
  **side effect** — `service_loader.rs:775-783` invokes it and discards the
  result, then reads `layer.servicesCatalog` by name. jboss's `modules()`
  (`jboss_jdkspecific.rs:1084`) derives and caches that catalog from
  `nameToModule`. p59's ignores its receiver entirely and allocates a fresh
  `HashSet` from the registry. So in synthetic mode that read finds nothing.
* p59's `boot()` (`:2990`) allocates a `java/lang/ModuleLayer` with **1** field
  and sets only "is boot". The declared synthetic width is **2**
  (`class_manager.rs:15403`) and jboss's `build_boot_layer` uses
  `MODULE_LAYER_FIELD_COUNT = 2` and populates `parents` / `nameToModule` /
  `servicesCatalog` by name. p59's layer has none of them.

Neither is touched by this lane — see §5.

---

## 2. The edit

Both surfaces now carry the same gate as `populate_boot_layer_modules`, using
the same accessor E4 added (`NativeContext::module_is_class_path_only`,
`registry.rs:1558`, default `false`). **No second filter was written**, and no
new trait method.

```rust
// modules()
let all = ctx.all_module_names();
let names: Vec<String> = all
    .into_iter()
    .filter(|name| !ctx.module_is_class_path_only(name))
    .collect();

// findModule(String)
if names.iter().any(|n| n == &name_str) && !ctx.module_is_class_path_only(&name_str) {
```

`all_module_names()` and `module_names()` are two names for one thing:
`vm_exec.rs:8398` and `vm_exec.rs:8480` have textually identical bodies, both
`module_registry.module_names()`. Keeping `all_module_names()` here avoids an
unrelated diff; the filter is what matters.

Borrow shape verified by compiling an isolated model of the closure signature
and both `&self` accessors with `rustc --edition 2021`
(`scratchpad/e16/borrow.rs` — compiles, prints `modules=1 find_a=true find_b=false`).
The repo itself was not built: this lane may not run `cargo`.

The `phase59_module_vs_essential_natives` guard is unaffected — it diffs
`(class, method, descriptor)` triples, and this edit changes only bodies.

---

## 3. HotSpot 25 ground truth — the contrast IS the test

One `javac`-built modular jar, `com.e16.svc` (exports + `provides`), in two
positions; one probe class on the class path in both arms. Sources and jar:
`scratchpad/e16/`, probe `scratchpad/e16/probe/E16Layer.java`.

```
$ java -version
openjdk version "25.0.3" 2026-04-21 LTS  (Microsoft-13877124, build 25.0.3+9-LTS)

===== ARM A: modular jar on -cp =====
$ java -cp "probe;out/com.e16.svc.jar" E16Layer CLASSPATH
arm=CLASSPATH
  findModule(com.e16.svc).isPresent = false
  boot.modules() contains        = false
  Greeter.class.getModule()      = unnamed module @4aa298b7
  ...getModule().isNamed()       = false
  ...getModule().getName()       = null
  loader                         = jdk.internal.loader.ClassLoaders$AppClassLoader@105be200

===== ARM B: same jar on --module-path =====
$ java -cp "probe" --module-path "out" --add-modules com.e16.svc E16Layer MODULEPATH
arm=MODULEPATH
  findModule(com.e16.svc).isPresent = true
  boot.modules() contains        = true
  Greeter.class.getModule()      = module com.e16.svc
  ...getModule().isNamed()       = true
  ...getModule().getName()       = com.e16.svc
  loader                         = jdk.internal.loader.ClassLoaders$AppClassLoader@330bedb4
```

| measurement | jar on `-cp` | same jar on `--module-path` |
|---|---|---|
| `ModuleLayer.boot().findModule(m)` | **empty** | present |
| `ModuleLayer.boot().modules()` contains it | **false** | true |
| `SomeClassFromThatJar.class.getModule().isNamed()` | **false** | true |

Same jar, same bytes, same app class loader in both arms — only the position on
the command line differs, and **all three answers flip together**. The
divergence this lane closes is precisely a VM that answers column 2 for a jar
in column 1's position.

### 3.1 PREDICTED CratonVM after-state

| # | measurement, synthetic-jdk mode, modular jar on `-cp` | before | after (**PREDICTED**) |
|---|---|---|---|
| 1 | `ModuleLayer.boot().findModule(m)` | present | empty |
| 2 | `ModuleLayer.boot().modules()` contains it | true | false |
| 3 | same three answers for a `--module-path` module | present/true/named | unchanged — `vm_init` re-registers those with `automatic = false` |
| 4 | `--real-jdk` / `--jdk-only`, anything | unaffected | unaffected — this registrar does not run there (§1.2) |

Row 4 is why no corpus number should move. Row 3 depends on E4 §1.1's
`class_manager.rs` edit being present; without it a lazy `module-info` load
downgrades a `--module-path` module to `automatic` and this gate would evict it
too. That coupling is now shared by two files.

---

## 4. §4.1's `ModulePackages` switch — does this path depend on it? **No, and that makes it WIDER, not narrower.**

E4 §4.1 established that 13 of the 14 double-source corpus jars carry no
`ModulePackages` attribute, so CratonVM registers zero packages for them,
`module_for_package` misses, and `Class.getModule()` answers the **unnamed**
module — which is why the JDK's own `isNamed()` cross-source guard never fired.

Traced in this file, the two paths are different:

* **`Class.getModule()`** (`reflect_invoke.rs:3264`, and its real-JDK twin at
  `lib.rs:12006`) calls `ctx.module_name_of_class(class_id)`. That reads the
  class record's `module_name` (`vm_exec.rs:8278`), which `ClassManager`
  computed **once at define time** from
  `self.module_registry.module_for_package(pkg)` with a `module_name_from_attr`
  fallback (`class_manager.rs:6176-6181`). No `ModulePackages` ⇒ no packages
  indexed ⇒ that lookup misses ⇒ unnamed. **Dependent.**
* **`ModuleLayer.modules()` / `findModule`** (`:2996`, `:3055`) call
  `ctx.all_module_names()` → `ModuleRegistry::module_names()` → the map's
  **key set** (`module.rs:1030`). Packages are never consulted. **Not
  dependent.**

So the accidental mitigation that kept 13 of 14 jars from detonating on the
boot-layer path does **not** apply here. Had `register_p59_module` been the
winner in a corpus mode, all 14 double-source jars — every junit, both lucene,
`hibernate-validator`, `jakarta.mail` — would have been reported present by
`findModule` and named by `modules()`, with `getModule()` still answering
unnamed for 13 of them. That is the impossible state E4 §2's second bullet
names, produced inside one registrar.

The jar measured in §3 is that same shape: `javap -v module-info.class` for
`com.e16.svc` prints a `Module:` attribute and **no `ModulePackages`** — the
shape 38 of the corpus's 51 module-info jars have. HotSpot still answers
`false/false/false` on `-cp`. `ModulePackages` is a CratonVM-side switch on one
of the two surfaces; it is not part of the JDK rule being matched.

---

## 5. Residuals — stated, not fixed

1. **§1.4's two synthetic-mode consequences are NOT closed by this edit.** p59's
   `boot()` returns a 1-field layer with no `nameToModule`/`servicesCatalog`,
   and its `modules()` does not carry jboss's catalog side effect that
   `service_loader.rs:775` depends on. Both are in this lane's own file, and
   both are deliberately left alone: repairing them means deciding whether
   synthetic-jdk mode should use jboss's boot layer at all, which is a
   behavioural change across every synthetic-mode `ServiceLoader` call and
   cannot be landed on a lane that may not run the VM. It should land on its own
   with a synthetic-mode measurement. Filed here so the next reader does not
   have to re-derive it from the call graph.
2. **Synthetic-mode module answers are unmeasured, before and after.** No stored
   run in `scratchpad/p1` is a synthetic-jdk registry dump — `reg.json` is
   `"mode": "compatible"`, and `r.json`/`sweep.json` are the jdk-only violation
   census (schema 1), which carries no per-triple `registered_by`. §1.3 is a
   call-graph fact, not a dump.
3. **The width divergence.** p59 allocates `java/lang/Module` with 2 fields and
   `java/lang/ModuleLayer` with 1; `class_manager.rs:15403-15407` declares 5 and
   2. Same family as the `[w×3]` hazard. Untouched.

---

## 6. NOMINATIONS

### NOM E16-1 — `vm-cli/src/main.rs:4110-4111` — the comment states the wrong mechanism

The comment is the origin of NOM E-7's claim. It is wrong in every mode: in
`--real-jdk`/`--jdk-only` there is no `phases_late` registration to win over,
and in synthetic mode `phases_late` is the LAST writer. Anchor verified unique
today.

OLD:

```rust
        // Its native (`register_jboss_jdkspecific`, last-writer-wins over the
        // `phases_late` stub, both `NativeKind::Bridge` so strict mode keeps
        // them) runs `build_boot_layer` → `populate_boot_layer_modules` →
```

NEW:

```rust
        // Its native (`register_jboss_jdkspecific`, `NativeKind::Bridge` so
        // strict mode keeps it) runs `build_boot_layer` →
        // `populate_boot_layer_modules` →
```

and, immediately after that block, add:

```rust
        // NOT last-writer-wins over the `phases_late` stub, as this comment
        // used to say. `register_p59_module` (phases_late/reflect_invoke.rs)
        // registers the same three triples, but its ONLY caller chain is
        // `register_phase59_natives` → `register_synthetic_overrides`, which
        // `vm_init.rs:1934` reaches only under `config.use_synthetic_jdk`. In
        // the modes this launcher runs it is never registered at all — the
        // schema-4 census shows one row per triple, `overwrote: null`. In
        // synthetic mode it runs AFTER `register_essential_natives` and
        // therefore OVERWRITES this one. See
        // docs/known-issues/jdk-only/E16-R11-P59-MODULE-LAYER-TWIN-20260813.md §1.
```

### NOM E16-2 — `native-builtins/src/phases_late.rs:9965-10051` — the guard cannot see this class of bug

`phase59_module_vs_essential_natives` computes `p59.difference(&essential)`:
it flags triples p59 registers that essential does **not**. Every triple in this
record is in the **intersection** — registered by both, with different bodies,
p59 winning where it runs. The guard is structurally blind to it, which is why a
`ModuleLayer.findModule` that had been unfiltered since before `f0a472dcf` sat
green.

Suggested (owner's call on exact form): alongside the existing `difference`,
compute `p59.intersection(&essential)` and assert it against a checked-in
allowlist naming, per triple, which registrar wins in which mode and why the
bodies may differ. That turns "both register it" from invisible into a line
someone has to update. `java/lang/ModuleLayer.{boot,modules,findModule}` and
`java/lang/Module.{getName,getLayer,getPackages}` are today's members.

### NOM E16-3 — `docs/known-issues/jdk-only/E4-R11-CLASS-PATH-MODULE-BOOT-LAYER-FIX-20260813.md` §6 NOM E-7 — correct and close it

Owned by lane E4. Two corrections and a status change:

* the path is `native-builtins/src/phases_late/reflect_invoke.rs`;
* *"dead today — `register_jboss_jdkspecific` wins last-writer-wins"* is wrong
  in both directions (§1.2, §1.3); replace with *"not registered at all in
  `--real-jdk`/`--jdk-only`; registered LAST, and therefore winning, in
  synthetic-jdk mode"*;
* the filter is now applied — NOM E-7 is no longer "no code change, a hazard to
  keep written down". Point it at this record.

The record's second sentence — that `native_module_layer_modules`
(`jboss_jdkspecific.rs:1084`) is fixed only transitively, via `nameToModule` no
longer containing class-path modules — is **confirmed correct** by this lane and
needs no change.

### NOM E16-4 — `native-api/src/registry.rs:1540-1546` — E4 already nominated this; it is still there

The doc above `module_names` still says the registry is *"`java.base` plus
whatever `--module-path` supplied — because only two sites populate it"*. There
is a third (the application class path), and that false sentence is what makes
this whole bug class look impossible from the trait's side. E4 raised it and did
not patch it because of em-dashes in the surrounding text; two lanes have now
had to work around it. It also directly contradicts the doc of
`module_is_class_path_only` fourteen lines below.

---

## 7. The lesson

**"Dead by registration order" is a claim about a race that has to be run to be
believed — and it is the weakest possible reason to leave a divergence in
place.** Here the race was never run: in the modes that matter the losing body
was never registered, and in the mode where it was, it won. Both halves of the
record's reasoning were wrong, and the code was wrong in the direction the
record said was safe.

**The census answers "who owns this slot" precisely, including who lost.** 1 217
of 11 748 rows carry `owns_slot: false` with a matching winner. A triple that
appears **once** with `overwrote: null` was not overwritten — it was never
offered. Absence and defeat look nothing alike in the data, and the difference
is exactly the difference between "another mode is safe too" and "another mode
is where it detonates".
