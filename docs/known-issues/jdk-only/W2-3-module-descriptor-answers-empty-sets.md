# `ModuleDescriptor` answered empty sets for every module, in every mode

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Headline: CLOSED, and now verified.** The original fix is in the tree — all
  seven "Record vs tree" rows check out; spot anchors:
  `build_module_modifier_set` at `native-builtins/src/jboss_jdkspecific.rs:1436`,
  `build_requires_set` at `:1480`, both consumed by `build_module_descriptor` at
  `:1641` and `:1670`. The binary verification this record said it lacked was
  taken 2026-08-12 against the dev binary at `ba65f1a19`: `RJdkModule` runs to
  `PASS RJdkModule (44 checks)` in **both** `--jdk-only` and `--real-jdk`. That
  run needs `--module-path regression-suite/build-modules --add-modules
  cratonvm.jdkonly.svc`, which `run.sh` supplies via `class_args`; without them
  it fails on a harness error, not a VM defect.
* **Residual: CLOSED — the two accessors that turned out to have a source.**
  Commit `4a388071c` *fix(jdk-only): keep the module directive flags
  parse_module_info was dropping* put the directive flags and the `requires`
  version on the parse-side entry structs (`classloading/src/module.rs`:
  `is_synthetic:58`, `is_mandated:69`, `compiled_version:78`, exports `:96`/`:98`,
  opens `:117`/`:119`, populated at `:1252-1259`, `:1279-1280`, `:1297-1298`).
  The bridge consumes them: `Requires.modifiers()` reads TRANSITIVE/STATIC from
  the real enum statics at `jboss_jdkspecific.rs:1513`, and
  `ModuleDescriptor.modifiers()` gains `OPEN` at `:1436`/`:1641`.
* **Residual: STILL OPEN — the whole `## Out-of-file patch (not applied)`, all
  four parts.** Re-grepped 2026-08-12 and every part is genuinely absent:
  1. `classloading/src/module.rs` has **zero** occurrences of `main_class` or
     `ModuleMainClass`; the three external literal sites
     (`classloading/src/access_control.rs`, `vm/src/vm/vm_init.rs`,
     `vm/tests/new19_module_access.rs`) are unmodified.
  2. The six new `NativeContext` accessors do not exist — a tree-wide `*.rs`
     grep for `module_is_automatic|module_version|module_main_class|
     module_requires_full|module_exports_full|module_opens_full` returns nothing.
  3. Consequently the `ModuleRegistry`-backed impls in `vm/src/vm/vm_exec.rs` do
     not exist either.
  4. And `build_module_descriptor` cannot consume them; `build_requires_set`'s
     modifier loop still carries only two bits.
  The visible consequence stands: `isAutomatic()` is a hardcoded `false`, so
  `RJdkModule`'s `check(!d.isAutomatic(), ...)` passes **vacuously**. A green
  vector does not close this, at 44 checks or at any larger number.

* **Re-verified independently 2026-08-12 (second pass), and part 1 was
  deliberately NOT landed.** All four parts are still absent — a tree-wide
  `*.rs` grep for `module_is_automatic|module_version|module_main_class|
  module_requires_full|module_exports_full|module_opens_full` returns only
  `reader/`'s unrelated `ModuleMainClass` *attribute* tests, and
  `jboss_jdkspecific.rs` still carries `ctx.set_field_by_name(desc,
  "automatic", Value::Int(0))` with `version`/`rawVersionString`/`mainClass`
  left null on purpose.

  **Why part 1 was not landed by a lane that owned `classloading/src/module.rs`.**
  Adding `main_class` to `pub struct ModuleDescriptor` breaks three struct
  literals outside that file — `classloading/src/access_control.rs`,
  `vm/src/vm/vm_init.rs`, `vm/tests/new19_module_access.rs` — each needing one
  added `main_class: None,` line. The struct has no `Default` impl and the
  literals name every field, so there is no non-breaking spelling. Landing part
  1 alone therefore buys **no observable change at all** (parts 2–4 are the only
  consumers, and all three live in files this lane does not own) while making
  the tree's ability to compile depend on three edits landing elsewhere in the
  same commit. The four parts are one change; splitting them at the struct
  boundary is the worst place to split them. The exact text for all four is
  unchanged below and was re-checked against the tree, including that
  `parse_module_info` still has both `desc` and `packages` bindings the part-1
  snippet inserts between.

  **`version()` and `Requires.compiledVersion()` need no parse-side work at
  all** — `ModuleDescriptor::version` (`module.rs`, from `version_index`) and
  `ModuleRequiresEntry::compiled_version` (from `requires_version_index`) are
  both parsed and carried today. Only `main_class` has no data source anywhere.
  So of the five accessors this record names as sourceless, four are blocked
  purely on the **bridge** (parts 2–4), and one is blocked on the bridge *and*
  the struct field.

  **No fixture assertion was added, and that is not an oversight.** Every
  accessor here is answered by a hardcoded constant or a null field, and the
  hardcoded answers happen to be *correct* for `cratonvm.jdkonly.svc` — it is
  not automatic, it carries no version, it declares no main class. A check that
  distinguishes the fix from the hardcode needs a module that disagrees with it:
  an **automatic** module (a plain jar with no `module-info` on the module path)
  for `isAutomatic()`, and a `jar --module-version` / `--main-class` build for
  the other three. Both are new harness work, and both would go RED until parts
  2–4 land. Writing them now would hand the next lane a red suite instead of a
  closed record.

Lane W2-3 of the jdk-wave2 pool; the defect directly behind lane L9's
`--module-path` resolution fix.

Lane W2-3 of the jdk-wave2 pool; the defect directly behind lane L9's
`--module-path` resolution fix.

## The failure

`regression-suite/src/RJdkModule.java`, **both** `--real-jdk` and `--jdk-only`
(HotSpot 25 passes: 44 checks, exit 0):

```
AssertionError: exports: []
    at RJdkModule.descriptor(RJdkModule.java:69)
```

HotSpot's answer for the same method:

```
CK RJdkModule exports=[com.cratonvm.jdkonly.svc, com.cratonvm.jdkonly.svc.open]
   opens=[com.cratonvm.jdkonly.svc.open]
   packages=[com.cratonvm.jdkonly.svc, com.cratonvm.jdkonly.svc.internal,
             com.cratonvm.jdkonly.svc.open]
   provides=[com.cratonvm.jdkonly.svc.Greeter->2]
```

Reproduce:

```
cd regression-suite && <cratonvm> --java-home "<jdk>" [--jdk-only] \
    --module-path build-modules --add-modules cratonvm.jdkonly.svc -cp build RJdkModule
```

The `--module-path`/`--add-modules` flags are load-bearing in that command. A
verifier who drops them measures a different (and already-recorded) failure.

## The oracle

The module under test is real and compiled. Its true descriptor, straight from
the class file (note: `javap -cp <dir> module-info` resolves the *system*
`module-info`, so name the file):

```
$ javap -v regression-suite/build-modules/cratonvm.jdkonly.svc/module-info.class
Module:
  "cratonvm.jdkonly.svc"
  1 requires   "java.base" ACC_MANDATED
  2 exports    com/cratonvm/jdkonly/svc
               com/cratonvm/jdkonly/svc/open
  1 opens      com/cratonvm/jdkonly/svc/open
  0 uses
  1 provides   com/cratonvm/jdkonly/svc/Greeter with
                 com/cratonvm/jdkonly/svc/internal/EnGreeter
                 com/cratonvm/jdkonly/svc/internal/FactoryGreeter
```

There is **no `ModulePackages` attribute** (javac does not emit one for an
exploded compilation), so the package set comes from
`classloading::module::exploded_packages`' tree walk — which is why
`com.cratonvm.jdkonly.svc.internal` (no export, no open, only classes) and the
resource-only directories still appear in `packages()`.

## Root cause

`native-builtins/src/lib.rs::build_synthetic_module_descriptor` — the single
implementation behind every `Module.getDescriptor()`,
`Class.getModule().getDescriptor()` and
`ModuleLayer.findModule(..).get().getDescriptor()` path — did this:

```rust
for field in ["modifiers", "requires", "exports", "opens", "provides", "packages"] {
    let empty = module_descriptor_empty_set(ctx)?;
    ctx.set_field_by_name(desc, field, Value::Object(Some(empty)));
}
```

Empty sets, unconditionally, for every module, in every mode. Only `name`,
`open` and `uses` were ever answered truthfully.

This was not a missing-data problem *for those five fields*. The data already
existed and was already reachable: `classloading::module::parse_module_info`
parses each module's `module-info.class` into a full `ModuleDescriptor`, and
both `ClassManager`'s boot `module-info` scan and `vm_init`'s
`resolve_module_path` wiring register it into `ClassManager::module_registry`.
`NativeContext` simply exposed no accessor for anything except
`module_packages` / `module_uses` / `module_is_open`, so the native surface
fabricated instead of asking.

## Record vs tree (re-checked 2026-08-11)

This campaign has documented fourteen records claiming a hand-off patch was
never applied when it is in the tree today. W2-3 is a **fifteenth**: every
bullet of "The fix" and all three "defects fixed alongside" are present.

| claim | tree |
| --- | --- |
| four new registry accessors + `module_is_registered` on `NativeContext` | present, `native-api/src/registry.rs` (`module_exports`, `module_opens`, `module_requires`, `module_provides`, `module_is_registered`), with `ModuleRegistry`-backed impls in `vm/src/vm/vm_exec.rs` |
| `build_synthetic_module_descriptor` is a one-line delegation | present, `native-builtins/src/lib.rs` → `jboss_jdkspecific::build_module_descriptor` |
| `$Exports`/`$Opens`/`$Provides`/`$Requires` built with real field shapes | present, `build_export_like` / `build_provides` / `build_requires_set` |
| collections built with the real `HashSet`/`ArrayList` ctor + `add` | present, `build_string_hash_set` |
| `findModule` returns `Optional.empty()` for an unregistered name, gated on the registry being populated | present, gated on `module_is_registered("java.base")` |
| `findModule` returns the canonical cached mirror | present, `get_cached_module_mirror` / `cache_module_mirror` |
| `Module.getResourceAsStream` registered with encapsulation gate | present, `native_module_get_resource_as_stream` |

One factual error in the old record: the accessor is **`rawVersion()`**, not
`rawVersionString()` — `rawVersionString` is the private *field*. Confirmed by
`javap -p java.lang.module.ModuleDescriptor` on JDK 25.

## The full accessor surface

`javap -p java.lang.module.ModuleDescriptor` and its five nested types on
JDK 25. "Fabricated" means the value is invented by CratonVM and could be
wrong; "sourced empty/absent" means the emptiness is itself the measured
answer.

### `ModuleDescriptor`

| accessor | verdict | source |
| --- | --- | --- |
| `name()` | real | the module name being built |
| `isOpen()` | real | `module_is_open` |
| `requires()` | real | `module_requires` |
| `exports()` | real | `module_exports` |
| `opens()` | real | `module_opens` |
| `uses()` | real | `module_uses` |
| `provides()` | real | `module_provides` |
| `packages()` | real | `module_packages` |
| `modifiers()` | **partial (was fabricated empty)** | `OPEN` wired 08-11 from `module_is_open`; `AUTOMATIC`/`SYNTHETIC`/`MANDATED` unsourced |
| `isAutomatic()` | **fabricated `false`** | registry knows (`ModuleDescriptor::automatic`); no `NativeContext` accessor asks |
| `version()` / `rawVersion()` | sourced absent | field left null ⇒ real `Optional.ofNullable` ⇒ `Optional.empty()`; parsed into `ModuleDescriptor::version`, never bridged |
| `mainClass()` | **unsourced** | `ModuleMainClass` attribute is never consulted by `descriptor_from_module_attribute`; field left null ⇒ `Optional.empty()` |
| `accessFlags()` | derived | real bytecode computes it from `modifiers()`; inherits that row's verdict |
| `toNameAndVersion()` | derived | real bytecode over `name` + `version` |
| `compareTo` / `equals` / `hashCode` / `toString` | derived | real bytecode over the fields above |

### `ModuleDescriptor.Requires`

| accessor | verdict | source |
| --- | --- | --- |
| `name()` | real | `module_requires` |
| `modifiers()` | **partial (was fabricated empty)** | `TRANSITIVE`/`STATIC` wired 08-11 from bits `module_requires` already delivered; `MANDATED`/`SYNTHETIC` parsed 08-11 but not bridged |
| `compiledVersion()` / `rawCompiledVersion()` | sourced absent | `requires_version_index` parsed 08-11 into `compiled_version`, not bridged; field null ⇒ `Optional.empty()` |
| `accessFlags()` | derived | from `modifiers()` |

### `ModuleDescriptor.Exports` / `ModuleDescriptor.Opens`

| accessor | verdict | source |
| --- | --- | --- |
| `source()` | real | `module_exports` / `module_opens` |
| `targets()` | real | same |
| `isQualified()` | real | real bytecode, `!targets.isEmpty()` |
| `modifiers()` | **sourced empty** | flags parsed 08-11; measured zero — see below |
| `accessFlags()` | derived | from `modifiers()` |

### `ModuleDescriptor.Provides`

| accessor | verdict | source |
| --- | --- | --- |
| `service()` | real | `module_provides` |
| `providers()` | real | same |

### `ModuleDescriptor.Version`

Never instantiated by CratonVM, because no descriptor carries a version yet.
`parse()` / `compareTo()` / `toString()` are real JDK bytecode and work if one
is handed to them.

## Which modifier bits actually occur

Before deciding that an empty `modifiers()` is acceptable, it was measured
rather than assumed. Scanning JDK 25's own module declarations —
`javap -v --module <m> module-info`, grepping the `Module:` section for
`ACC_MANDATED`/`ACC_SYNTHETIC`, over `java.base`, `java.desktop`,
`java.logging`, `java.sql`, `jdk.jfr`:

```
=== java.base ===
=== java.desktop ===
      1     #8,8000    // "java.base" ACC_MANDATED
=== java.logging ===
      1     #8,8000    // "java.base" ACC_MANDATED
=== jdk.jfr ===
      1     #10,8000   // "java.base" ACC_MANDATED
=== java.sql ===
      1     #8,8000    // "java.base" ACC_MANDATED
```

`ACC_MANDATED` on `requires java.base`, in every module, and **nowhere else** —
zero hits on any `exports` or `opens` directive, and `java.base` itself has no
`requires` at all. So:

* an empty `Exports.modifiers()` / `Opens.modifiers()` is the **right** answer
  for every `javac`- or `jlink`-emitted directive, and
* `Requires.modifiers()` is the one that visibly disagreed with HotSpot:
  `[MANDATED]` there, `[]` here, for `requires java.base` in every module.

That is why the exports/opens flags are parsed but the *requires* bits are the
priority for the bridge patch below.

## What was fixed 2026-08-11, and where the data came from

**`classloading/src/module.rs`** — `descriptor_from_module_attribute` read the
class file's directive flag words and the `requires` version index and threw
them away. The reader had always parsed them
(`ModuleRequires.{requires_flags,requires_version_index}`,
`ModuleExports.exports_flags`, `ModuleOpens.opens_flags`); only this hop
discarded them, which is why four accessors had no data source *anywhere* in
the VM. They are now carried on the entry structs
(`ModuleRequiresEntry::{is_synthetic,is_mandated,compiled_version}`,
`Module{Exports,Opens}Entry::{is_synthetic,is_mandated}`). Data only — nothing
reads them yet.

**`native-builtins/src/jboss_jdkspecific.rs`** — two accessors the old table
listed as sourceless in fact had a source already reaching the native:

1. `Requires.modifiers()` — `module_requires` hands over
   `(name, is_transitive, is_static)`, and `build_requires_set` bound the bools
   to `_transitive` / `_is_static` and dropped them. Now minted from the **real
   enum's static constants**. That detail is load-bearing: `Enum.equals` is
   identity, so `mods.contains(Requires.Modifier.TRANSITIVE)` only answers true
   if the set holds the genuine singleton. A synthesised stand-in would compare
   unequal and read as "not transitive" — a wrong answer dressed as a right one.
   The old record's reason for not doing this ("a native cannot mint enum
   constants without reading the enum's statics") is true but not a blocker:
   `static_field_index_by_name` + `get_static_field` is a well-worn idiom, used
   at ~20 sites across `native-builtins`.

2. `ModuleDescriptor.modifiers()` — was empty for every module while `isOpen()`
   *on the same object* was answered truthfully from `module_is_open`.
   `newOpenModule` is specified to build a descriptor whose modifiers contain
   `Modifier.OPEN`, so the two accessors are two spellings of one fact and the
   object was contradicting itself. `OPEN` now comes from the data already
   exposed.

Both hoist `ensure_class_initialized` out of the allocation-bearing loop —
`<clinit>` runs Java and can move the heap — and skip it entirely when no entry
carries a modifier, so the common `requires <plain>` path never initialises the
enum.

**Mode impact:** none in `Compatible`. Every path here is reached only for a
module the registry actually parsed; an unregistered module and any embedder
with an unpopulated registry keep the previous answer byte for byte, which is
the same gate the original fix used.

## What is deliberately NOT fabricated

Per accessor, with the javadoc sentence that decides it:

* **`version()` / `rawVersion()` / `mainClass()` / `Requires.compiledVersion()` /
  `rawCompiledVersion()`** — the fields are left **null**, not set. Real
  `ModuleDescriptor.version()` is `Optional.ofNullable(version)`, and its
  javadoc is *"Returns the module version"* with `@return` *"An `Optional`
  containing the module version; an empty `Optional` if the module does not
  have a version"*. A null field therefore renders as `Optional.empty()`, which
  is the honest "nothing was recorded" answer and the *correct* answer for a
  module-info that carries no version. It is wrong only for one that does — and
  writing a fabricated version would convert a truthful empty into a false
  claim. Answering absence beats raising here because absence is a
  spec-sanctioned outcome of this exact accessor.

* **`isAutomatic()` / `Modifier.AUTOMATIC`** — currently a hardcoded `false`,
  which *is* a fabrication and is flagged as such in the source. Javadoc:
  *"Returns `true` if this is an automatic module."* There is no "unknown"
  encoding available — the return type is `boolean` — so the honest fix is to
  answer it from the registry, not to pick a default. Until the accessor lands,
  `RJdkModule.java:61`'s `check(!d.isAutomatic(), ...)` passes **vacuously**:
  it would pass against a hardcoded `false` whatever the module really is. This
  is the same vacuous-pass shape as the old `findModule` check at `:48`.

* **`Requires.modifiers()` MANDATED/SYNTHETIC**, **`Exports`/`Opens`
  `modifiers()`** — parsed now, not bridged. Leaving a modifier *out*
  understates the set rather than inventing membership, so a caller testing
  `contains(X)` gets a false negative, never a false positive. For exports and
  opens the understatement is provably zero-width against every JDK 25 module
  (see the scan above).

`RJdkModule` asserts none of these directly. `isAutomatic()` is the one it
touches, vacuously.

## Out-of-file patch (not applied)

**Apply all four parts together or none of them.** Re-verified absent
2026-08-12; the prescription below is NOT in §2.4's dead list. Part 1 is a
`classloading/` change with three one-line consequences outside it, and it is
inert without parts 2–4 — see the second-pass status block.

Everything remaining needs `native-api/src/registry.rs` and
`vm/src/vm/vm_exec.rs`, which this lane does not own. The parse-side data all
exists as of 2026-08-11 except `main_class`, whose struct field is included
below because adding it to `classloading::module::ModuleDescriptor` breaks the
three literal construction sites outside this lane
(`classloading/src/access_control.rs`, `vm/src/vm/vm_init.rs`,
`vm/tests/new19_module_access.rs`), each needing one added line.

### 1. `classloading/src/module.rs` — `mainClass()`'s data source

Add to `pub struct ModuleDescriptor`:

```rust
    /// The `ModuleMainClass` attribute's class, in INTERNAL (slash) form.
    ///
    /// A sibling attribute of `Module`, not part of it, so it is resolved in
    /// `parse_module_info` (which sees the whole attribute list) rather than in
    /// `descriptor_from_module_attribute` (which only sees the `Module`
    /// attribute). Backs `ModuleDescriptor.mainClass()`.
    pub main_class: Option<String>,
```

Set `main_class: None` in `descriptor_from_module_attribute`'s returned
literal, then in `parse_module_info`, between the `desc` and `packages`
bindings:

```rust
    let mut desc = desc;
    desc.main_class = class_file.attributes.iter().find_map(|a| {
        match a.as_decoded() {
            Some(cratonvm_reader::attribute::Attribute::ModuleMainClass { main_class_index }) => {
                class_file.constant_pool.get_class_name(*main_class_index).map(|s| s.to_string())
            }
            _ => None,
        }
    });
```

And one added line in each of the three external literals:

```rust
        main_class: None,
```

### 2. `native-api/src/registry.rs` — new `NativeContext` accessors

Beside the existing `module_*` accessors. Every default is the conservative
"this context does not model modules" answer, matching the convention the
neighbouring accessors already set.

```rust
    /// True if `module_name`'s descriptor was registered for a jar on the
    /// CLASS path rather than a real module path — the JDK's *automatic
    /// module* shape. Backs `ModuleDescriptor.isAutomatic()` and
    /// `ModuleDescriptor.Modifier.AUTOMATIC`, both of which are currently a
    /// hardcoded `false` in the Java mirror.
    fn module_is_automatic(&self, module_name: &str) -> bool {
        let _ = module_name;
        false
    }

    /// The raw module version string from `module-info.class`
    /// (`Module.version_index`), if the producer recorded one. Backs
    /// `ModuleDescriptor.rawVersion()` and, through `Version.parse`,
    /// `version()`.
    fn module_version(&self, module_name: &str) -> Option<String> {
        let _ = module_name;
        None
    }

    /// The `ModuleMainClass` attribute's class in INTERNAL (slash) form, if the
    /// module declares one. Backs `ModuleDescriptor.mainClass()`.
    fn module_main_class(&self, module_name: &str) -> Option<String> {
        let _ = module_name;
        None
    }

    /// `requires` directives with the FULL modifier set and compiled version:
    /// `(name, transitive, static, synthetic, mandated, compiled_version)`.
    ///
    /// Supersedes [`module_requires`], whose `(String, bool, bool)` tuple
    /// cannot carry `ACC_SYNTHETIC`/`ACC_MANDATED` — which is why
    /// `Requires.modifiers()` reports `[]` for `requires java.base` where
    /// HotSpot reports `[MANDATED]`. Added alongside rather than widening the
    /// existing method so no current caller has to change.
    fn module_requires_full(
        &self,
        module_name: &str,
    ) -> Vec<(String, bool, bool, bool, bool, Option<String>)> {
        let _ = module_name;
        vec![]
    }

    /// `exports` directives with their modifier bits:
    /// `(package, targets, synthetic, mandated)`. Package names are INTERNAL
    /// (slash) form, as in [`module_exports`].
    fn module_exports_full(&self, module_name: &str) -> Vec<(String, Vec<String>, bool, bool)> {
        let _ = module_name;
        vec![]
    }

    /// `opens` directives, same shape as [`module_exports_full`].
    fn module_opens_full(&self, module_name: &str) -> Vec<(String, Vec<String>, bool, bool)> {
        let _ = module_name;
        vec![]
    }
```

### 3. `vm/src/vm/vm_exec.rs` — the `ModuleRegistry`-backed impls

Beside the existing `module_*` impls, same `class_manager().read().module_registry`
shape:

```rust
    fn module_is_automatic(&self, module_name: &str) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .is_some_and(|d| d.automatic)
    }

    fn module_version(&self, module_name: &str) -> Option<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .and_then(|d| d.version.clone())
    }

    fn module_main_class(&self, module_name: &str) -> Option<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .and_then(|d| d.main_class.clone())
    }

    fn module_requires_full(
        &self,
        module_name: &str,
    ) -> Vec<(String, bool, bool, bool, bool, Option<String>)> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .map(|d| {
                d.requires
                    .iter()
                    .map(|r| {
                        (
                            r.module_name.clone(),
                            r.is_transitive,
                            r.is_static,
                            r.is_synthetic,
                            r.is_mandated,
                            r.compiled_version.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn module_exports_full(&self, module_name: &str) -> Vec<(String, Vec<String>, bool, bool)> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .map(|d| {
                d.exports
                    .iter()
                    .map(|e| {
                        (
                            e.package_name.clone(),
                            e.to_modules.clone(),
                            e.is_synthetic,
                            e.is_mandated,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn module_opens_full(&self, module_name: &str) -> Vec<(String, Vec<String>, bool, bool)> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .map(|d| {
                d.opens
                    .iter()
                    .map(|o| {
                        (
                            o.package_name.clone(),
                            o.to_modules.clone(),
                            o.is_synthetic,
                            o.is_mandated,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
```

### 4. Consuming it in `jboss_jdkspecific.rs` (this lane's file, blocked on 2+3)

Once the accessors exist, in `build_module_descriptor`:

* switch `ctx.module_requires` → `ctx.module_requires_full`, widen
  `build_requires_set`'s tuple, and add `MANDATED`/`SYNTHETIC` to the same
  `[(wanted, constant)]` array the `TRANSITIVE`/`STATIC` loop already walks;
* set `automatic` from `ctx.module_is_automatic(module_name)` instead of `0`,
  and add `Modifier.AUTOMATIC` to `build_module_modifier_set` on the same bit;
* when `ctx.module_version(module_name)` is `Some`, set `rawVersionString` to
  that string and `version` to the result of invoking the real static
  `ModuleDescriptor$Version.parse(String)` — do NOT hand-build a `Version`, its
  four fields include two parsed token lists;
* when `ctx.module_main_class(module_name)` is `Some`, set `mainClass` to the
  **dotted** form (the field holds a binary class name, and every other name in
  this builder already crosses `dotted` on the way out);
* same for `Requires.compiledVersion` / `rawCompiledVersion` per entry.

Each is guarded by `Some` — a module with no version must keep the null field,
because that is what makes `Optional.empty()` the *truthful* answer rather than
a fabricated one.

## Falsifying observation

Run the verify command above with `--module-path build-modules --add-modules
cratonvm.jdkonly.svc`. If `exports` is still `[]`, the registry is not being
consulted — check first that `resolve_module_path` actually registered the
module (`tracing::info!("module path: resolved N module(s)...")` in
`vm/src/vm/vm_init.rs` fires only when the resolution is non-empty). If
`exports` is non-empty but `packages` is short, `exploded_packages` is the
suspect, not this change.

For the 08-11 modifier work specifically, `RJdkModule` does **not** cover it —
it asserts no modifier set. The cheap probe is a class-path vector printing
`ModuleLayer.boot().findModule("java.base").get().getDescriptor().requires()`
and any module's `.modifiers()`, compared against HotSpot: `requires` modifiers
should stay `[]` until the bridge patch lands (MANDATED is the only bit
java.base's dependents carry), and `modifiers()` should be `[]` for
`cratonvm.jdkonly.svc` (not an open module) and `[OPEN]` for an `open module`
fixture. A vector that only checks `.isEmpty()` on these proves nothing — an
empty set is exactly what the defect produced.
