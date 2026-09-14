# E4 / R11 — the fix for the class-path module promoted into the boot layer, and the one jar shape it does NOT close

**Date:** 2026-08-13 **Lane:** E4 **Fixes:** the regression diagnosed in
`D1-R11-SERVICELOADER-DOUBLE-SOURCE-20260813.md`, introduced by `f0a472dcf`.

**This lane did not build or run CratonVM.** Every "after" below is **PREDICTED**
and labelled as such. Every "before" is either a stored corpus artefact under
`regression-suite/corpus/out/`, source read in this working tree, or a
`java`/`javap` measurement on HotSpot 25 on this host today.

**Edits applied (owned files only):**

| file | what |
|---|---|
| `native-builtins/src/jboss_jdkspecific.rs` | the gate in `populate_boot_layer_modules` + its doc |
| `classloading/src/class_manager.rs` | stop a lazy `module-info` load from downgrading a `--module-path` module to `automatic` |

**Nominations (files owned by other lanes) in §6.** NOM E-1/E-2/E-3 are
**required for the tree to compile** — the gate calls a trait method that does
not exist yet. They are one atomic change, not three optional ones.

---

## 1. The fix

`populate_boot_layer_modules` walked `ctx.module_names()` — the whole
`ModuleRegistry`, which `ClassManager::new` populates from the **application
class path** as well as boot/ext — and for each entry wrote three surfaces:
`nameToModule` (→ `ModuleLayer.findModule`), `modules` (→ `ModuleLayer.modules()`)
and `ServicesCatalog.getServicesCatalog(systemClassLoader).register(module)`
(→ `ServiceLoader`). One gate at the top of the loop covers all three:

```rust
if ctx.module_is_class_path_only(&name) {
    continue;
}
```

`module_is_class_path_only` is a new `NativeContext` accessor defaulting to
`false`, so no mock in `native-builtins/src/test_utils.rs` changes and a context
that does not model modules keeps today's behaviour. It reads
`ModuleDescriptor::automatic`, which within this VM is not the JPMS
automatic-module flag but **the record of where the descriptor came from** —
`ClassManager::new` stamps `true` for the app class path, `vm_init` re-registers
genuine `--module-path` modules with `false` immediately afterwards.

### 1.1 The second edit, and why the first is unsafe without it

`automatic` had a **third** writer, which the diagnosis did not have:
`classloading/src/class_manager.rs:6136`, on the lazy path that registers a
`module-info` loaded as a class:

```rust
desc.automatic = !is_platform_module_name(&desc.name);
```

`ModuleRegistry::register` is `self.modules.insert(name, desc)` — an
**overwrite** — and `is_platform_module_name` is false for every
application-named module, `--module-path` ones included. So a lazy load of a
module-path module's own `module-info` silently downgraded it to `automatic`.
Before this session that was a readability nuance. With the gate in place it
would **evict a genuine `--module-path` module from the boot layer**, which is
exactly what `regression-suite/src/RJdkModule.java:52,62,352` asserts against.
The edit preserves an existing explicit registration:

```rust
let already_explicit = self
    .module_registry
    .get(&desc.name)
    .is_some_and(|prev| !prev.automatic);
desc.automatic = !already_explicit && !is_platform_module_name(&desc.name);
```

Nothing is lost for the case the original line defends: a `-cp` jar such as
`org.jboss.logging` is never registered explicit by anybody, so it still lands
on `automatic = true`.

---

## 2. Is skipping the right arm, or should the class-path source lose?

**Skipping is the only arm that matches HotSpot in both sub-cases.** A jar on
`-cp` is an unnamed-module citizen; the JDK ignores its `module-info` outright
and its services come only from `../../../apps/META-INF/services`. Therefore:

| jar on `-cp` | HotSpot | skip the module (this fix) | suppress `../../../apps/META-INF/services` instead |
|---|---|---|---|
| ships `provides` **and** a descriptor | 1 provider | **1** ✔ | 1 ✔ |
| ships `provides` only (no descriptor) | **0** providers | **0** ✔ | **1** ✘ |

The second row is not hypothetical — it is the shape of
`regression-suite/modules/cratonvm.jdkonly.svc`, and §5 shows it is the sharpest
contrast the vector has. Two further reasons the other arm is not available:

* Suppressing the class-path source means making
  `ClassLoader.getResources("META-INF/services/…")` **hide a resource that is
  genuinely on the class path**. HotSpot returns it. Every non-`ServiceLoader`
  consumer of that resource (anything calling `getResources` directly, and the
  vector's own `descriptorProviderNames`) would then read a class path that does
  not exist.
* It would leave `ModuleLayer.boot().findModule(m)` present for a `-cp` jar
  while that jar's own classes answer `getModule().isNamed() == false` — the
  impossible state that caused this in the first place. The symptom would move;
  the divergence would not.

---

## 3. Does it break what `ModuleLayer.boot()` was added FOR? No.

`vm-cli/src/main.rs:4109-4133` states the reason in measured numbers. The call
exists because `--jdk-only` refuses the `SyntheticStub` `ServiceLoader` natives,
so nothing populated the system loader's `ServicesCatalog` and every
**module-declared** provider vanished:

| service | before `f0a472dcf` | with it |
|---|---|---|
| `java.nio.file.spi.FileSystemProvider` | 0 | 2 |
| `java.util.spi.ToolProvider` | 0 | 9 |
| `javax.tools.JavaCompiler` | 0 | 1 |

and `ToolProvider.getSystemJavaCompiler()` was null, sending H2's
`SourceCompiler` down a path HotSpot never takes.

**Every one of those providers is declared by a PLATFORM module** — `java.base`,
`jdk.compiler`, `jdk.zipfs` and friends. Those descriptors are registered from
the **bootstrap** class path (`class_manager.rs:2807-2811` passes `false` for
bootstrap and extension), so `automatic == false` and the gate does not see
them. **PREDICTED: those three counts stay at 2 / 9 / 1.** The feature the
commit was added for is untouched; only the app-class-path entries it also
swept up are removed.

`RJdkModule` is the control. It runs with
`--module-path $MODBUILD --add-modules $JDKONLY_MODULE` (`run.sh:366-367`), so
`vm_init` registers its module `automatic = false` and the gate keeps it —
provided §1.1's edit is present. `RJdkFailure:309,311` asserts
`findModule("java.base").isPresent()` (platform, kept) and a nonexistent module
absent (unaffected).

---

## 4. Other jars with both forms — 14 of 103, across all four corpora

**How this was checked.** Every `cp.args` under `regression-suite/corpus/out`
was parsed into a distinct set of jar paths (103, 0 missing from disk); each was
opened with `System.IO.Compression` and tested for a `module-info.class` **and**
a non-empty `../../../apps/META-INF/services/`; each hit's `module-info` was extracted and run
through `javap -v`. Script:
`scratchpad/e4/scan-double-source.ps1`, output `scratchpad/e4/double-source.txt`.

**51 of 103 jars carry a `module-info.class`. 14 carry both forms:**

| jar | corpus | doubled service(s) |
|---|---|---|
| `junit-jupiter-engine-5.14.4` | bc-java, commons-math | `TestEngine` → `JupiterTestEngine` |
| `junit-vintage-engine-5.14.4` | bc-java, commons-math | `TestEngine` → `VintageTestEngine` |
| `junit-jupiter-engine-6.1.0` | spring-framework | `TestEngine` |
| `junit-platform-suite-engine-6.1.0` | spring-framework | `TestEngine` |
| `junit-platform-launcher-{1.12.1,1.14.4,6.1.0}` | all three | `TestExecutionListener` → `UniqueIdTrackingListener` |
| `junit-platform-engine-{1.12.1,1.14.4,6.1.0}` | all three | `DiscoverySelectorIdentifierParser` |
| `lucene-core-9.7.0` | **h2** | 6 services (`Codec`, `PostingsFormat`, `DocValuesFormat`, `KnnVectorsFormat`, `SortFieldProvider`, `TokenizerFactory`) |
| `lucene-analysis-common-9.7.0` | **h2** | 3 services |
| `hibernate-validator-9.1.0.Final` | spring-framework | `ValidationProvider` |
| `jakarta.mail-2.0.1` | bc-java | `jakarta.mail.Provider` |

In each case the `module-info`'s `provides … with X` names the **same class** the
descriptor names — verified with `javap -v` for `junit-jupiter-engine-5.14.4`
(`org/junit/jupiter/engine/JupiterTestEngine`), `junit-platform-launcher-1.14.4`
(`…listeners/UniqueIdTrackingListener`) and `lucene-core-9.7.0`
(`Lucene95Codec` and five more).

**This is an EXPOSURE census, not a failure count.** Only one stored run ever
executed a binary containing `f0a472dcf`: applying D1 §2's discriminator
(`CRATONVM_DBG_TOARRAY` vs `CRATONVM_DBG_LAYOUT` in the autobox WARN) across
five runs reproduces its partition exactly —

```
bc-java-jdk-only-20260812-213333   TOARRAY=14  LAYOUT=4     <- the split run
bc-java-jdk-only-20260812-220129   TOARRAY=1   LAYOUT=0
spring-framework-jdk-only-…-215547 TOARRAY=1   LAYOUT=0
commons-math-jdk-only-…-204935     TOARRAY=6   LAYOUT=0
h2-jdk-only-…-195247               TOARRAY=0   LAYOUT=0
```

— and `grep -rl "multiple engines"` over all of `out/` returns exactly the four
logs D1 names, all in that one run. Every other stored run predates the binary.
So spring-framework (two doubled `TestEngine`s), commons-math and h2 (nine
doubled Lucene SPIs) were **exposed and never measured**.

### 4.1 The switch that decides whether a double-source jar detonates: `ModulePackages`

`try_register_module_info` (`class_manager.rs:2915-2924`) takes a module's
packages from the `ModulePackages` attribute and **nothing derives them if it is
absent** (`ModuleRegistry::register` only indexes what it is handed). Without
packages, `module_for_package` misses, `module_name_of_class` answers `None`,
and `Class.getModule()` (`native-builtins/src/lib.rs:12006-12011`) returns the
**unnamed** module.

Measured with `javap -v` over all 51 module-info jars
(`scratchpad/e4/blast.ps1`): **13 carry `ModulePackages`, 38 do not.** Of the 14
double-source jars, **13 have none** — including every junit and lucene jar —
and exactly one, `jakarta.mail-2.0.1`, has it.

That is why the throw was junit's. For the 13, CratonVM labelled the class-path
copy **unnamed**, so the JDK's only cross-source guard
(`clazz.getModule().isNamed()`, JDK 25 `LazyClassPathLookupIterator.hasNextService`
bci 25-35) did **not** fire and the provider came through twice. This upgrades
D1 §4.2 step 4 from a structural argument to a measurement.

---

## 5. Predicted after-state

| # | measurement | before | after (**PREDICTED**) |
|---|---|---|---|
| 1 | the four bc-java classes in `C17` §6.2, `--jdk-only` | `JUnitException: Cannot create Launcher for multiple engines with the same ID 'junit-jupiter'`, 0 tests | run, and `ServiceLoader` yields exactly the 2 engines the 2 `../../../apps/META-INF/services` descriptors name |
| 2 | `RServiceLoaderDoubleSource` §6 falsifier command | RED at "ModuleLayer.boot() contains a module whose only source is the CLASS path" | GREEN, `separation inBootLayer=false named=false svc=on` |
| 3 | `RJdkModule` | green | green — its module comes from `--module-path`, `automatic = false` (requires §1.1) |
| 4 | `FileSystemProvider` / `ToolProvider` / `JavaCompiler` counts under `--jdk-only` | 2 / 9 / 1 | 2 / 9 / 1 — unchanged, all platform modules |
| 5 | h2 corpus, `Codec`+8 other Lucene SPIs | doubled (never measured; exposed) | single |
| 6 | **`jakarta.mail.Provider` under `--jdk-only`** | **1** | **0** — see §5.1 |

### 5.1 The residual this fix does NOT close, stated before it is discovered

`jakarta.mail-2.0.1` is the one double-source jar with `ModulePackages`.
CratonVM therefore labels its classes **named**, and the JDK's `isNamed()` skip
*does* fire for its class-path copy. Today its single provider arrives from the
module source. Remove the module source and the class-path source is still
skipped by the JDK — **1 → 0, where HotSpot answers 1.**

It is real, it is caused by this fix, and it is **unexercised in the corpora as
configured**: `jakarta.mail` appears in five `cp.args` files and in **no log
line of any run** (`grep -ril 'jakarta.mail\|javax.mail\|smime' out/*/` matches
only `cp.args`). bc-java's selected `AllTests` classes are asn1/pqc/util.

**The complete fix is one more line in a file this lane owns, and it is
deliberately NOT applied** — see §6 NOM E-6. It would make the app-class-path
scan register its modules with an **empty package list**, so a `-cp` class
reports the unnamed module exactly as on HotSpot, and `../../../apps/META-INF/services` then
supplies the provider (1, matching). It is held back because its blast radius is
different in kind and would be inseparable from this one in a corpus re-run: it
flips `Class.getModule().isNamed()` from true to false for the **13** jars that
carry `ModulePackages` — among them `slf4j-api`, `byte-buddy` and
`byte-buddy-agent`, i.e. exactly the classes Mockito's `assureCanReadMockito`
does module checks on. Every flip is toward the oracle, and 38 of 51 jars
already sit on that side of the line. It should land next, on its own, with its
own corpus measurement.

---

## 6. NOMINATIONS

### NOM E-1 — `native-api/src/registry.rs` — the accessor (REQUIRED TO COMPILE)

Anchor re-verified against the working tree today: `registry.rs:1547-1549`,
matched exactly once.

OLD:

```rust
    fn module_names(&self) -> Vec<String> {
        vec![]
    }
```

NEW:

```rust
    fn module_names(&self) -> Vec<String> {
        vec![]
    }

    /// True when `module_name` reached the registry ONLY by scanning the
    /// application class path — i.e. a modular jar on `-cp`, which a real JVM
    /// treats as an unnamed-module citizen whose `module-info` is ignored.
    ///
    /// Callers that model the JDK's module *system* must skip these; callers
    /// that only want labelling may not care. Defaults to `false` so a context
    /// that does not model modules keeps its existing behaviour.
    fn module_is_class_path_only(&self, module_name: &str) -> bool {
        let _ = module_name;
        false
    }
```

**Also worth correcting in the same file, not patched here because the
surrounding doc contains em-dashes that must survive verbatim:** the doc above
`module_names` (`:1540-1546`) says the registry holds "`java.base` plus whatever
`--module-path` supplied — because only two sites populate it". There is a
third, it is the application class path, and that false sentence is what makes
this bug look impossible from the trait's side.

### NOM E-2 — `vm/src/vm/vm_exec.rs` — the implementation (REQUIRED TO COMPILE)

Anchor re-verified: `vm_exec.rs:8398-8405`, matched exactly once. (Note there is
a second, textually identical body at `:8471` named `all_module_names` — the
anchor below includes the `fn module_names` line, so it is unambiguous.)

OLD:

```rust
    fn module_names(&self) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .module_names()
    }
```

NEW:

```rust
    fn module_names(&self) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .module_names()
    }

    fn module_is_class_path_only(&self, module_name: &str) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .is_class_path_only(module_name)
    }
```

### NOM E-3 — `classloading/src/module.rs` — the registry query (REQUIRED TO COMPILE)

Anchor re-verified: `module.rs:1029-1032`, matched exactly once.

OLD:

```rust
    /// Return all registered module names.
    pub fn module_names(&self) -> Vec<String> {
        self.modules.keys().cloned().collect()
    }
```

NEW:

```rust
    /// Return all registered module names.
    pub fn module_names(&self) -> Vec<String> {
        self.modules.keys().cloned().collect()
    }

    /// True when this module was registered by the APPLICATION class-path scan
    /// and nothing re-registered it as explicit.
    ///
    /// `ClassManager::new` stamps `automatic = true` for every
    /// `module-info.class` it finds on the app class path; `vm_init` then
    /// re-registers each genuine `--module-path` module with
    /// `automatic = false`. So within this VM `automatic` is not a JPMS
    /// automatic-module flag in the JDK's sense — it is the record of WHERE the
    /// descriptor came from, and that is the question the module system has to
    /// ask before treating a descriptor as real.
    pub fn is_class_path_only(&self, module_name: &str) -> bool {
        self.modules
            .get(module_name)
            .is_some_and(|desc| desc.automatic)
    }
```

### NOM E-4 — `regression-suite/run.sh` — wire the vector, then register it

`RServiceLoaderDoubleSource` exists, is green on HotSpot (1523 checks, 5/5
processes, 3/3 md5-identical), has 8 mutation controls, and is **not scheduled**.
Its discriminating half needs one class-path entry the harness must supply, and
it FAILS rather than skips when that is absent. **Order matters: 1 and 2 before
3.** Line numbers below are re-verified against the current file — the launch
sites have moved since D1 wrote this (they are now `:525` and `:556`, not
487/518).

1. A per-class class-path hook. Both arms launch with a single `-cp "$BUILD"`
   and no vector has ever needed a second entry:

   ```sh
   # Extra CLASS-PATH entries a vector needs, appended to $BUILD. Handed to
   # BOTH VMs. Emits the separator too, so an empty answer is a no-op.
   #
   # RServiceLoaderDoubleSource needs a MODULAR jar (here: the exploded module
   # regression-suite/modules already builds) on the CLASS path, present when
   # the VM starts -- CratonVM scans the app class path for module-info.class
   # inside ClassManager::new, before main, so a jar the vector creates itself
   # would be scanned by nobody. It must NOT also appear in class_args(): a
   # --module-path module IS resolved into the boot layer and IS defined to the
   # application loader on a real JVM, and the vector refuses to run in that
   # configuration precisely so it cannot measure its own command line.
   class_cp_extra() {
     case "$1" in
       RServiceLoaderDoubleSource)
         [ -n "$HAVE_MODULE" ] && printf '%s' "$CPSEP$MODBUILD/$JDKONLY_MODULE" ;;
       *) : ;;
     esac
   }
   ```

   with `-cp "$BUILD$(class_cp_extra "$c")"` at **`:525`** (CratonVM) and
   **`:556`** (HotSpot). `$CPSEP` is new and load-bearing: this file has never
   needed a class-path separator and it is `;` on Windows, `:` elsewhere.

2. A `class_args()` arm (the function is at `:391`; the `RJdkModule` arm at
   `:366` is the model):

   ```sh
       RServiceLoaderDoubleSource)
         [ -n "$HAVE_MODULE" ] && printf '%s' \
           "-Dcratonvm.rt.cpmodule=$JDKONLY_MODULE -Dcratonvm.rt.cpclass=com.cratonvm.jdkonly.svc.Greeter -Dcratonvm.rt.cpservice=com.cratonvm.jdkonly.svc.Greeter"
         ;;
   ```

3. **Only then** add `RServiceLoaderDoubleSource` to `JDKONLY_CLASSES` (`:137`).
   Until 1 and 2 land it belongs in `UNREGISTERED_CLASSES` (`:175`) with this
   record as its reason, so the coverage census does not report it as forgotten.

Verified on HotSpot with exactly this shape (module compiled to a directory,
placed on `-cp`, no module path): PASS, 754 checks. **PREDICTED on CratonVM
after NOM E-1..E-3 land: PASS.** The suite module declares its providers only in
`module-info` and ships no `../../../apps/META-INF/services`, so it exercises the row of §2's
table that the other arm gets wrong.

### NOM E-5 — `regression-suite/corpus/run-corpus.sh` — record the binary's identity

Unchanged from D1 NOM 7 and re-confirmed by §4 of this record: the TSV header
carries `# cv=<path>`, and a path is stable across a rebuild. This lane had to
recover "which binary produced this row" from the incidental wording of a WARN
message, for the second time. Emit a content hash **per row**, not once per run.

### NOM E-6 — `classloading/src/class_manager.rs` — DEFERRED, this lane's own file

The completion of the same JDK rule, closing §5.1. **Not applied**, for the
reason in §5.1: it must be measured on its own. Exact text, anchor verified
unique today (`class_manager.rs:2812-2814`):

OLD:

```rust
                for bytes in class_path.scan_module_infos() {
                    Self::try_register_module_info(&mut module_registry, &bytes, automatic);
                }
```

NEW: `try_register_module_info` gains a `class_path_only: bool` parameter (the
same bit as `automatic`) and, when it is set, passes `Vec::new()` to
`module_registry.register` instead of the parsed `ModulePackages` list — a
`-cp` class then reports the unnamed module, as it does on HotSpot. Everything
`vm_init` re-registers keeps its packages, because `resolve_module_path`
supplies them explicitly (`vm_init.rs:1342-1347`), falling back to scanning the
exploded tree / jar entry list when `ModulePackages` is absent.

Expected effect, measured on the corpora: **13 jars** of 51 flip
`Class.getModule().isNamed()` from `true` to `false`; the other 38 already
answer `false`. `jakarta.mail.Provider` goes 0 → 1 (HotSpot: 1).

### NOM E-7 — no code change, a hazard to keep written down

`native-builtins/src/phases_late/reflect_invoke.rs:2996-3040` registers a second
`ModuleLayer.modules()` / `findModule` pair that enumerates `all_module_names()`
with **no** class-path filter. It is dead today — `register_jboss_jdkspecific`
wins last-writer-wins (`vm-cli/src/main.rs:4110-4111`,
`reflect_invoke.rs:2814-2816`) — but it is a `Bridge` kind, so `--jdk-only`
keeps it, and a registration-order change would silently restore the exact bug
this record closes. Same for `native_module_layer_modules`
(`jboss_jdkspecific.rs:1067-1107`), which builds the layer's own catalog from
`nameToModule.values()` and is fixed only *transitively*, by that map no longer
containing class-path modules.

---

## 7. What is still owed

1. A CratonVM run. Rows 1-4 of §5 are predictions. If row 1 comes back still
   red, the state to check first is whether the tree carries NOM E-1..E-3 —
   a partial landing does not compile, so a *green build* with a red row 1 means
   the mechanism is not what D1 §4 says.
2. The h2 corpus. §4 says nine Lucene SPIs were doubled on binary B and nobody
   measured it. A re-run is the only thing that turns that into a number.
3. NOM E-6, measured separately.
4. Whether `jakarta.mail`'s 1 → 0 ever reaches a test. It does not in the
   corpora as configured; that is a fact about the configuration, not about the
   VM.

---

## 8. The lesson

**A gate is only as good as the invariant it reads, and an invariant with three
writers has none.** `automatic` meant "came from `-cp`" at two of its three
write sites. The third — a lazy `module-info` load — set it from the module's
*name*, which is indistinguishable between a `-cp` jar and a `--module-path`
module. Reading a flag as a fact required making it one first, and the diagnosis
that named the flag had checked two of the three sites and said so.

**And: a fix aimed at a duplicate can produce a zero.** The gate removes one of
two sources. For 13 of the 14 exposed jars the other source is live and the
count goes 2 → 1. For the fourteenth, CratonVM's *own* mislabelling had already
disabled the other source, so the same edit takes it 1 → 0. The measurement that
found this — `javap -v | grep ModulePackages` over 51 jars — took a minute, and
nothing in the diagnosis pointed at it.
