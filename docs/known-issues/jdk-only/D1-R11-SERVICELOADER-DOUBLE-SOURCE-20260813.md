# D1 / R11 — the duplicate `junit-jupiter` engine is a modular jar on `-cp` promoted into the boot layer. It is not intermittent, and `getResources` is innocent.

**Date:** 2026-08-13 **Lane:** D1 **Subject:** `WAVE-D-QUEUE.md` §R11,
`C17-CORPUS-READJUDICATION-20260812.md` §3.

**This lane never built, ran or touched CratonVM.** No `.rs` file was written.
Every VM-side number below comes from the stored corpus logs under
`regression-suite/corpus/out/`; every oracle number comes from `java`/`javac`/
`javap` on HotSpot 25.0.3 on this host, today. Every Rust change wanted is a
NOMINATION in §8 with exact literal old/new text.

**Files added by this lane:**
`regression-suite/src/RServiceLoaderDoubleSource.java` (new vector), this
record. Nothing else.

---

## 0. Headline, with the denominators

| claim as queued (§R11) | measured |
|---|---|
| "It is INTERMITTENT — nine other classes in the same run were fine" | **False.** 18 classes ran; 14 were fine. The 4 failures partition **perfectly by BINARY**: 0 of 14 on the binary that ran 21:34–22:37, **4 of 4** on the binary that ran 22:49–22:50. The shipping binary was replaced mid-run. |
| "CratonVM's `ServiceLoader` enumerated one provider twice" | **True**, and now attributable end-to-end. |
| "the lead: `getResources` returned the same URL twice" | **Falsified, on the oracle, without needing CratonVM at all.** JDK 25's `ServiceLoader$LazyClassPathLookupIterator` carries a `Set<String> providerNames`; two identical descriptor URLs on `-cp` produce **one** provider. Under `--jdk-only` the real `ServiceLoader` bytecode runs, so a duplicate URL cannot reach the caller. |
| the `java/util/Enumeration$Impl` refusal is the lead | **COINCIDENCE.** It is present in **18 of 18** `.cv.log`s in that run — all 14 green ones included. §5. |

**What it actually is.** CratonVM scans the **application class path** for
`module-info.class` and registers every one in its `ModuleRegistry`
(`classloading/src/class_manager.rs:2807-2815`). Commit `f0a472dcf`
(2026-08-12 21:46) then added an unconditional `ModuleLayer.boot()` call to
`vm-cli/src/main.rs`, whose native walks **every registered module** and calls
`ServicesCatalog.getServicesCatalog(systemClassLoader).register(module)`
(`native-builtins/src/jboss_jdkspecific.rs:376-451`). `junit-jupiter-engine-5.14.4.jar`
carries both a `module-info.class` declaring
`provides org.junit.platform.engine.TestEngine with …JupiterTestEngine` **and**
the matching `../../../apps/META-INF/services` descriptor — measured, §3. So the real
`ServiceLoader` finds `JupiterTestEngine` twice: once from the module source,
once from the class-path source. JUnit's `LinkedHashSet<TestEngine>` keeps both
distinct instances and `EngineIdValidator` throws.

A real JVM cannot be put in this state: a modular jar reached through the
**class** path is an unnamed-module citizen whose `module-info` the JDK ignores
outright. This repo already knows that rule and writes it down verbatim —
`native-builtins/src/service_loader.rs:1266-1278` gives it as the reason the
native ServiceLoader path skips the module source for a class-path loader. The
same rule was never applied to `populate_boot_layer_modules`.

---

## 1. The failure, read off the bytecode rather than the name

`EngineIdValidator.validate` in `junit-platform-launcher-1.14.4.jar`, `javap -c`:

```
 0: new  java/util/HashSet          // Set<String> ids
…
55: aload_1
56: aload_3
57: invokeinterface TestEngine.getId:()Ljava/lang/String;
62: invokeinterface java/util/Set.add:(Ljava/lang/Object;)Z
67: ifne 96                          // fall through == add returned FALSE
70: new  org/junit/platform/commons/JUnitException
74: ldc  "Cannot create Launcher for multiple engines with the same ID '%s'."
```

and its input, `LauncherFactory.collectTestEngines`:

```
17: new java/util/LinkedHashSet
24: ServiceLoaderTestEngineRegistry.loadTestEngines:()Ljava/lang/Iterable;
33: invokedynamic accept:(Ljava/util/Set;)…   // engines::add
38: invokeinterface java/lang/Iterable.forEach
```

with `loadTestEngines()` being exactly
`ServiceLoader.load(TestEngine.class, ClassLoaderUtils.getDefaultClassLoader())`.

So the exception is thrown **iff two identity-distinct `TestEngine` objects
report the same id.** `TestEngine` overrides neither `equals` nor `hashCode`, so
the `LinkedHashSet` de-duplicates by identity and keeps both.

That reading matters because it admits **three** mechanisms, not one, and the
queued lead named only the third:

1. `ServiceLoader` handed out `JupiterTestEngine` twice;
2. `HashSet<String>.add` answered `false` on a first insert, or
   `LinkedHashSet`'s iterator repeated an element — either produces the
   identical message with `ServiceLoader` entirely innocent. Both classes are
   natively implemented in CratonVM (`native-collections`,
   `NativeKind::Bridge`, **kept** under `--jdk-only`), so neither was free;
3. `getResources` returned the descriptor URL twice.

The new vector measures all three separately (§7).

---

## 2. It is not intermittent: the run changed binary underneath itself

`out/bc-java-jdk-only-20260812-213333`, 18 classes, one process each, all from
one `cp.args`. Ordered by `.cv.log` mtime:

| # | class | started | threw |
|---|---|---|---|
| 1–14 | `asn1` … `pqc.crypto.test` | 21:34:57 → 22:37:32 | 0 |
| 15 | `pqc.math.ntru.test.AllTests` | 22:49:52 | **yes** |
| 16 | `util.encoders.test.AllTests` | 22:50:26 | **yes** |
| 17 | `util.io.pem.test.AllTests` | 22:50:39 | **yes** |
| 18 | `util.utiltest.AllTests` | 22:50:52 | **yes** |

Four contiguous failures at the end of eighteen is 1 in 3060 by chance. The
discriminator is in the logs themselves. Every run emits the `gc::guard`
autobox WARN, and its text names a debug flag:

| logs 1–14 | logs 15–18 |
|---|---|
| `Run with CRATONVM_DBG_TOARRAY=1 to resolve class_id to a name.` | `Run with CRATONVM_DBG_LAYOUT=1 to resolve class_id to a name.` |

**18 of 18 correlation, no exceptions.** That string changed in commit
`713523e73` "fix(gc): the autobox guard advertised a flag that does nothing"
(2026-08-12 20:57 −0300). So `/c/craton/jdkonly-wave2-target/release/cratonvm.exe`
was rebuilt between 22:37 and 22:49 and the last four rows ran a **different
binary** from the first fourteen.

**0 of 14 on binary A. 4 of 4 on binary B.** That is a deterministic
regression, not an intermittent defect, and every instrument built on the
"intermittent" premise — a repeated probe hunting for a rare duplicate — would
have been pointed at the wrong axis.

*(This lane cannot read binary B's exact commit. What is measured is the
partition and that binary B postdates `713523e73`. `f0a472dcf` (21:46) is the
only commit in that window that touches this mechanism, and §4 shows its change
is sufficient by construction. The one-command falsifier is in §6.)*

---

## 3. The class path is clean — measured, not assumed

`out/bc-java-jdk-only-20260812-213333/cp.args`, parsed and each entry opened:

* 31 entries, **0 exact duplicates**;
* duplicate *basenames* only: `main`, `test` (different directories) and
  `junit-4.13.2.jar` (two paths, and JUnit 4 declares no `TestEngine`);
* **exactly two entries carry `../../../apps/META-INF/services/org.junit.platform.engine.TestEngine`**:

```
junit-jupiter-engine-5.14.4.jar -> org.junit.jupiter.engine.JupiterTestEngine
junit-vintage-engine-5.14.4.jar -> org.junit.vintage.engine.VintageTestEngine
```

Both jars **also** carry a `module-info.class`. `javap -v` on the extracted
descriptors:

```
module org.junit.jupiter.engine@5.14.4   provides org/junit/platform/engine/TestEngine
                                           with org/junit/jupiter/engine/JupiterTestEngine
module org.junit.vintage.engine@5.14.4   provides org/junit/platform/engine/TestEngine
                                           with org/junit/vintage/engine/VintageTestEngine
```

One provider, declared twice, in two places the JDK reads through two different
doors. That is the whole defect surface.

---

## 4. The mechanism, by construction

### 4.1 What `--jdk-only` leaves running

`NativeKind::SyntheticStub` is the only kind `JdkOnly` refuses
(`native-api/src/registry.rs:4627`). Therefore, in the failing runs:

| surface | kind | in play under `--jdk-only`? |
|---|---|---|
| `java.util.ServiceLoader.*` natives | `SyntheticStub` | **no** — real JDK bytecode runs |
| `ClassLoader.getResources` (`cl_get_resources`) | essential/`Bridge` | yes |
| `java.util.HashSet` / `LinkedHashSet` natives | `Bridge` | yes |
| `ModuleLayer.boot` / `findModule` / `modules` | `Bridge` | **yes** (stated at `vm-cli/src/main.rs:4110-4112`) |

### 4.2 The chain

1. **`classloading/src/class_manager.rs:2807-2815`** — `ClassManager::new`
   scans bootstrap, extension **and application** class paths for
   `module-info.class` and registers each descriptor. App-class-path entries
   are stamped `automatic = true`; `vm/src/vm/vm_init.rs:1342-1347` re-registers
   genuine `--module-path` modules with `automatic = false` immediately
   afterwards, so **`automatic == true` means exactly "reached only through
   `-cp`"**.

2. **`vm-cli/src/main.rs`, added by `f0a472dcf`** — an unconditional
   `ModuleLayer.boot()` at VM start whenever `java_home` is set. Its own
   comment calls it "this VM's `ModuleBootstrap.boot()`".

3. **`native-builtins/src/jboss_jdkspecific.rs:376-451`** —
   `populate_boot_layer_modules` iterates `ctx.module_names()` (i.e. the whole
   registry, class-path entries included) and for each: `nameToModule.put`,
   `modules.add`, and `register_module_in_loader_catalog` →
   `ServicesCatalog.getServicesCatalog(systemClassLoader).register(module)`.
   The comment at `:437-443` says this mirrors what
   `ModuleLayer.defineModules` does "for boot-layer modules resolved from
   `--module-path`" — and that is the bug: it is applied to modules that came
   from `-cp`.

4. **Real `java.util.ServiceLoader`** then has two independent iterators.
   `ModuleServicesLookupIterator` reads the loader's `ServicesCatalog` and now
   yields `JupiterTestEngine`. `LazyClassPathLookupIterator` reads
   `loader.getResources("META-INF/services/…")` and yields it again, because
   its only cross-source guard is `clazz.getModule().isNamed()` — **measured
   in the JDK 25 bytecode**, `hasNextService`, bci 25-35:

   ```
   25: invokevirtual java/lang/Class.getModule
   29: invokevirtual java/lang/Module.isNamed
   32: ifeq 38          // not named -> use it
   35: goto 0           // named     -> skip it
   ```

   The class-path copy of `JupiterTestEngine` is loaded from `-cp` and is in
   the **unnamed** module, so `isNamed()` is false and the skip does not fire.
   CratonVM has created the one state that guard cannot cover: a `Module`
   object registered in a loader's catalog whose classes report
   `isNamed() == false`.

5. `LinkedHashSet<TestEngine>` keeps both instances; `EngineIdValidator`
   throws on the second `junit-jupiter`.

### 4.3 Why HotSpot could not be made to reproduce it

Two mutation controls were run on the oracle (§7.2) and **both stayed green,
for a reason worth writing down**: put the same modular jar on `-cp` *and*
`--module-path`, and the class becomes named, so the JDK's own skip fires;
put the same jar on `-cp` twice, and `LazyClassPathLookupIterator.providerNames`
collapses the duplicate name. **The oracle cannot enter this state**, which is
why the vector's red-going ability had to be demonstrated by feeding the
assertion a known-positive instead (§7.3).

---

## 5. The `Enumeration$Impl` refusal — COINCIDENCE, and the counting that says so

Per-log counts across all 18 `.cv.log`s of the failing run:

| class group | `Enumeration$Impl` refusals | threw |
|---|---|---|
| the 14 green | 8 (twice 15/16) | 0 |
| the 4 red | **7** | 4 |

* It is present in **18 of 18** logs. It cannot discriminate anything.
* The red logs show *fewer* refusals, not more — 7 rather than 8 — because
  they died earlier. Same for the other two refusal families in those logs
  (`HashMap$KeyItr`, `cratonvm/stream/LazyOp`).
* `classloader.rs:6074` is `try_alloc_concurrent_synthetic(ctx,
  ENUMERATION_IMPL_CLASS, 2)` inside `make_snapshot_enumeration`, whose `Err`
  arm falls back to `real_snapshot_enumeration` — a real
  `Arrays$ArrayList` + `Collections.enumeration`. **The refusal is handled**,
  the caller gets a real `Enumeration`, and nothing downstream can tell.
* Structurally it could not have caused this anyway: `make_snapshot_enumeration`
  is handed an already-built `URL[]`. Both arms wrap the **same array**. No arm
  of it can add an element.

**Verdict: COINCIDENCE.** Not cause, not even symptom of *this* defect — it is
correct `--jdk-only` behaviour that appears identically in every passing run,
and the only reason it drew attention is that it is printed near the throw.

This is the fourth time this session a refusal has been read as a defect. The
rule that would have closed it in one command: **before promoting a refusal to
a lead, count it in the runs that PASSED.** Here that is
`grep -c 'Enumeration\$Impl' *.cv.log` — eight seconds, 18 of 18, done.

---

## 6. The falsifier for §2, for whoever has a binary

One command settles whether this is `f0a472dcf`:

```
cratonvm --jdk-only --java-home "$JDK" \
  -cp <build>;<regression-suite/build-modules/cratonvm.jdkonly.svc> \
  -Dcratonvm.rt.cpmodule=cratonvm.jdkonly.svc \
  -Dcratonvm.rt.cpclass=com.cratonvm.jdkonly.svc.Greeter \
  -Dcratonvm.rt.cpservice=com.cratonvm.jdkonly.svc.Greeter \
  RServiceLoaderDoubleSource
```

Predicted RED at `ModuleLayer.boot() contains a module whose only source is the
CLASS path`. The A/B is the same command on a binary built with `f0a472dcf`'s
`ModuleLayer.boot()` call reverted; predicted GREEN. The suite module declares
its providers **only** in `module-info` and ships no `../../../apps/META-INF/services`, so
`ServiceLoader.load(Greeter, appLoader)` must find **zero** providers from
`-cp` — the sharpest possible contrast.

The bc-java reproduction is the four classes named in `C17` §6.2, run under
`--jdk-only` on a current binary. Expect **4 of 4**, not "sometimes".

---

## 7. The vector: `regression-suite/src/RServiceLoaderDoubleSource.java`

### 7.1 What it measures, and what a green result licenses

| section | asserts | goes red on |
|---|---|---|
| `resourceCensus` | over N iterations and both loaders: `getResources` URLs are DISTINCT, and the count does not drift between calls | the queued R11 hypothesis. **Kept as its falsifier, not as the test for this defect** — §0 shows a duplicate URL cannot reach the caller. |
| `setDiscipline` | fresh `HashSet.add` true-then-false; `LinkedHashSet` of 64 identity-distinct objects iterates exactly 64 with no repeat | mechanism 2 of §1 — the one nobody had excluded |
| `providerCensus` | no service resolves the same provider CLASS twice, on N fresh loaders; no count drift; one `ServiceLoader` iterated twice yields the SAME instances | a duplicate provider from any source, plus the JDK's documented cache |
| `classPathModuleSeparation` | a `-cp`-only module is NOT in `ModuleLayer.boot()`; its classes are in the UNNAMED module; its resolved provider set equals exactly what the `../../../apps/META-INF/services` descriptors name | **this defect** |

**It cannot be vacuously green.** `classPathModuleSeparation` *fails* — it does
not skip — when `-Dcratonvm.rt.cpmodule` / `-Dcratonvm.rt.cpclass` are absent,
and it fails when `--module-path` is in effect (which would make it measure its
own command line rather than the VM). Disabling it needs an explicit
`-Dcratonvm.rt.separation=0`, which prints
`separation DISABLED -- the only discriminating assertion did not run`.

No path, URL, module name or provider name from the environment is printed —
only counts — so the harness's cross-VM `CK` diff stays valid.

### 7.2 HotSpot oracle

JDK 25.0.3 (Microsoft build), this host, 2026-08-13. Fixture: a purpose-built
`d1-cpmod.jar` with `module-info` **and** a `../../../apps/META-INF/services` descriptor for
the same provider, on `-cp`.

```
CK RServiceLoaderDoubleSource getResources names=7 iters=64 dup=0 drift=0
CK RServiceLoaderDoubleSource setDiscipline hashset=4 linked=64
CK RServiceLoaderDoubleSource providers services=6 iters=16 dup=0 drift=0 cached=6
CK RServiceLoaderDoubleSource separation inBootLayer=false named=false svc=on
CK RServiceLoaderDoubleSource checks=1523
PASS RServiceLoaderDoubleSource (1523 checks)
```

**5 of 5 processes PASS; 3 of 3 md5-identical output.** Repeated with the
*suite's own* `modules/cratonvm.jdkonly.svc` compiled and placed on `-cp`
(the configuration §8 NOM 5 wires up): PASS, 754 checks.

### 7.3 Mutation controls — 8 run, and the three that stayed green are the finding

| # | mutation | result |
|---|---|---|
| 1 | `-Dcratonvm.rt.cpmodule` absent | **RED** — "is REQUIRED … a green result would mean nothing" |
| 2 | `--module-path` supplied alongside `-cp` | **RED** — precondition guard |
| 3 | `cpmodule=java.base`, `cpclass` still app-loaded | **RED at the target assertion** — "ModuleLayer.boot() contains a module whose only source is the CLASS path" |
| 4 | `cpclass=java.lang.String` | **RED** — "must be loaded from the APPLICATION class path" |
| 5 | the same jar on `-cp` twice | green — JDK collapses the duplicate NAME (`providerNames`) |
| 6 | a descriptor listing the provider twice | green — same reason |
| 7 | the same jar on `-cp` **and** `--module-path` | green — JDK's `isNamed()` skip fires |
| 8 | boot-layer-has-no-app-loader-module (an assertion this lane wrote and then deleted) | **RED ON HOTSPOT** — JDK 25 defines **19** boot-layer modules to the application loader with a bare `-cp`. The tempting one-liner is false on the oracle. |

5, 6 and 7 are not weaknesses of the vector: they are the measurement that the
JDK has *two* independent guards here, that both hold, and therefore that the
duplicate CratonVM produced has to come from the one state neither guard
covers. 8 is recorded because it was written, believed, and falsified inside
five minutes by running it — the comment explaining why it is absent is in the
source.

### 7.4 Scheduling

**Not** added to `JDKONLY_CLASSES` by this lane. It needs one class-path entry
the harness must supply (§8 NOM 5), and registering it without that wiring
would give the suite a vector that fails on every run for a reason that is not
the VM. See NOM 5 for the exact ordering: wiring first, registration second —
the rule `run.sh` already states for `RPriorityQueueGc`.

---

## 8. NOMINATIONS

Every patch below is exact literal old/new text. None has been compiled: this
lane may not run `cargo`.

### NOM 1 — `native-builtins/src/jboss_jdkspecific.rs` — do not promote a class-path-only module into the boot layer

This is the fix. One gate covers all three surfaces the loop writes
(`nameToModule` → `findModule`, `modules` → `modules()`, and the loader
`ServicesCatalog` → `ServiceLoader`).

OLD:

```rust
    let layer_pin = ctx.pin_native_root(layer);
    for name in names {
        let layer = ctx.read_native_pin(layer_pin, layer);
        let Ok(module) = build_module(ctx, &name, layer) else {
            continue;
        };
```

NEW:

```rust
    let layer_pin = ctx.pin_native_root(layer);
    for name in names {
        // A module whose ONLY source is the application CLASS path must not
        // enter the boot layer, and above all must not be registered in the
        // system loader's `ServicesCatalog` below.
        //
        // `ClassManager::new` scans the app class path for `module-info.class`
        // and registers what it finds (class_manager.rs, `automatic = true`);
        // `vm_init` re-registers genuine `--module-path` modules with
        // `automatic = false` right afterwards, so `automatic` here means
        // exactly "reached only through -cp". A real JVM ignores such a
        // `module-info` outright — a modular jar on the class path is an
        // unnamed-module citizen — and `service_loader.rs` already states that
        // rule as the reason ITS module source is skipped for a class-path
        // loader.
        //
        // Promoting one here gave `ServiceLoader` the SAME provider through
        // both of its doors: `ModuleServicesLookupIterator` reads this
        // catalog, and `LazyClassPathLookupIterator` reads
        // `META-INF/services`. The JDK's only cross-source guard is
        // `clazz.getModule().isNamed()`, which is FALSE for the class-path
        // copy, so nothing de-duplicates. Measured: four bc-java corpus
        // classes died with `JUnitException: Cannot create Launcher for
        // multiple engines with the same ID 'junit-jupiter'` before running a
        // test — junit-jupiter-engine.jar ships both a `provides` clause and a
        // `META-INF/services` descriptor naming one class. See
        // docs/known-issues/jdk-only/D1-R11-SERVICELOADER-DOUBLE-SOURCE-20260813.md.
        if ctx.module_is_class_path_only(&name) {
            continue;
        }
        let layer = ctx.read_native_pin(layer_pin, layer);
        let Ok(module) = build_module(ctx, &name, layer) else {
            continue;
        };
```

### NOM 2 — `native-api/src/registry.rs` — the accessor, defaulting to `false`

Defaulting to `false` means no mock in `native-builtins/src/test_utils.rs` has
to change, and a context that does not model modules keeps today's behaviour.

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

*(Also worth correcting, but NOT patched here because the surrounding doc
contains em-dashes that must survive verbatim: the doc above `module_names`
says the registry holds "`java.base` plus whatever `--module-path` supplied —
because only two sites populate it". There is a third site, and it is the
application class path. That false sentence is what makes NOM 1's bug look
impossible from the trait's side.)*

### NOM 3 — `vm/src/vm/vm_exec.rs` — the implementation

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

### NOM 4 — `classloading/src/module.rs` — the registry query

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

### NOM 5 — `regression-suite/run.sh` — the one input the vector needs

Two changes, in this order (wiring first, registration second — the rule this
file already states for `RPriorityQueueGc`):

1. A per-class **class-path** hook, because both arms are launched with a
   single `-cp "$BUILD"` (`:487` and `:518`) and no vector has ever needed a
   second entry. Something of the shape:

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

   with `-cp "$BUILD$(class_cp_extra "$c")"` at both `:487` and `:518`.
   **`$CPSEP` is new and is load-bearing**: this file has never needed a
   class-path separator, and it is `;` on Windows and `:` elsewhere. Deriving
   it from `uname` is the smallest correct thing.

2. A `class_args()` arm supplying the three properties to both VMs:

   ```sh
       RServiceLoaderDoubleSource)
         [ -n "$HAVE_MODULE" ] && printf '%s' \
           "-Dcratonvm.rt.cpmodule=$JDKONLY_MODULE -Dcratonvm.rt.cpclass=com.cratonvm.jdkonly.svc.Greeter -Dcratonvm.rt.cpservice=com.cratonvm.jdkonly.svc.Greeter"
         ;;
   ```

3. **Only then** add `RServiceLoaderDoubleSource` to `JDKONLY_CLASSES`. Until
   1 and 2 land it belongs in `UNREGISTERED_CLASSES` with this record as its
   reason, so the coverage census does not report it as forgotten.

   Verified on HotSpot with exactly this shape (`modules/cratonvm.jdkonly.svc`
   compiled to a directory and placed on `-cp`, no module path): **PASS, 754
   checks.**

### NOM 6 — `WAVE-D-QUEUE.md` §R11 — correct the two claims that would misdirect the next lane

§R11 says "It is INTERMITTENT" and names the `Enumeration$Impl` refusal as
where to look first. Both are measured false here (§2, §5). A lane that reads
§R11 and builds a repeated-sampling probe for a rare duplicate URL will find
nothing and will be right to. Replace the "**It is INTERMITTENT**" paragraph
and the "**Lead, not proof**" paragraph with a pointer to this record.

### NOM 7 — `regression-suite/corpus/run-corpus.sh` — record the binary's IDENTITY, not its path

The header of every `results.tsv` carries `# cv=/c/craton/…/cratonvm.exe`.
That is a *path*, and a path is stable across a rebuild. §2 of this record
exists because 18 rows sharing one header were produced by two different
binaries and the only surviving evidence of that was the incidental wording of
a `WARN` message.

Emit a content hash and an mtime for the `cv` binary in the header, and — the
part that actually closes it — **re-hash before each class and record the
result per row**, because a corpus run can outlive several builds on this host.
A row whose binary hash differs from the header's is not comparable with the
rest of the run, and today nothing can even ask the question.

Not offered as a literal patch: the file's owner has to choose between a
`sha256sum` per class (cheap here — one hash of a ~100 MB binary per class,
against workloads of 3–430 s) and an mtime+size pair (free, weaker). Either
beats what is there. This lane's recommendation is the hash, because mtime is
exactly what a shared-target rebuild perturbs unpredictably.

### Anchor verification

The OLD text of NOM 1–4 was matched back against the working tree
programmatically: **each occurs exactly once** in its file. They are safe to
apply mechanically. NOM 5 and 7 are shell and are described rather than
patched.

---

## 9. What is still owed

1. **A CratonVM run of §6.** Everything above is either stored evidence, source
   read, or HotSpot. The prediction is RED, deterministically, 4 of 4 — and if
   it comes back green that is a real result, because it would mean binary B
   differs from the working tree in some other way and §2's partition needs
   another explanation.
2. **The bc-java re-run** of the four classes in `C17` §6.2, which converts
   four `DIVERGE` rows into either a fixed row or a confirmed one.
3. **NOM 1's blast radius.** The gate removes class-path modules from
   `ModuleLayer.boot()` entirely, which is what HotSpot does — but
   `RJdkModule` asserts a great deal about the boot layer and is the control
   that must stay green. It runs with `--module-path`, so its module is
   `automatic = false` and the gate does not touch it; that is an argument, not
   a measurement, until the suite runs.
4. **The second catalog.** `native_module_layer_modules`
   (`jboss_jdkspecific.rs:1067-1107`) builds the LAYER's own `servicesCatalog`
   from `nameToModule.values()`. NOM 1 fixes it transitively by never putting a
   class-path module in that map — but if a later change re-populates the map
   from the registry directly, the same gate is needed there too.

---

## 10. The lesson

**A refusal that appears in the passing runs is not a lead.** It took one
`grep -c` over 18 files to retire the one this record was pointed at, and that
`grep` was available from the moment the logs were on disk.

**And the sharper one: "intermittent" was an artefact of the instrument.**
Four contiguous failures at the end of an 18-row run were published as
process-to-process variance. They were a rebuild. The run recorded its binary's
*path* in the TSV header (`# cv=…/cratonvm.exe`) and not its identity, so
"which binary produced this row" was unanswerable from the artefacts and had to
be recovered from a WARN message's incidental wording. **A run that cannot say
which binary produced each row cannot distinguish a regression from a flake** —
and this one described a deterministic regression using the vocabulary of a
flake, which is the most expensive way to be wrong about it.
