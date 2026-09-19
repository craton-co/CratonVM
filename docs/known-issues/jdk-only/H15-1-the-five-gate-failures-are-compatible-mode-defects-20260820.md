# H15-1 — the five vectors that gate every strict-mode change are Compatible-mode defects, and `--jdk-only` has already fixed all five

**Status: DIAGNOSED, NOT FIXED.** This lane wrote no Rust (contract: diagnosis
only, six other lanes editing Rust concurrently). Every remedy is a written-out
patch in `H15-2` / `H15-3` / §5 below, applied nowhere.

**Date** 2026-08-20
**Lane** H15 (`--jdk-only` completion, wave H)
**Subject** the five `SUITE=all` failures used as the verdict-neutrality bar for
every wave-H change: `RImmutableFactoryTypes`, `RJdkProxyIface`,
`RJdkFunctionCombinators`, `RJdkEnumerations`, `RServiceLoaderDoubleSource`
**Worktree** `C:/craton/cratonvm/.claude/worktrees/agent-a65ae41225a6314c6`
**Base** cut at `26e4b5db4`, fast-forwarded to
`claude/jdk-only-mode-handoff-09b48c` = `fe59bf9d9` before any work. The gap was
**58 commits**: lanes H4/H5/H6/H7/H8's merges and records, `H0-2`'s correction
banner, `H0-3`, `H0-4` (the blast-radius table), `H0-5` (the two HashMap
mechanisms), `HANDOFF-20260820.md`, and eight landed Rust fixes (`f83c24f68`, `b2dbf77e9`,
`e9f08d42b`, `e4e091bda`, `10bccbef4`, `317c3d5d0`, `45d6649ae`, `c33ba71fc`).
Two of those documents — `H0-2`'s correction banner and `H4-1` — bear directly
on §4 and on `H15-2` and are credited there. **None of the eight code changes
touches any of the five defects in this record** (checked: they are native-io
registrations, two JIT direct-bind doors, and a `MemoryUsage` slot lookup).

**Binary** `C:/craton/target-jdkonly-h2/release/cratonvm.exe`, built
2026-08-20 17:32 at `fe59bf9d9`. **Never rebuilt** — this lane must not build.
**Oracle** HotSpot **25.0.3+9**, resolved on this host as
`/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot` (**not** the Eclipse Adoptium
path 66 records cite — `H5-1` §6.1).

Every claim below is marked **MEASURED** (this lane ran it, today, on that
binary) or **ARGUED** (this lane read it).

---

## 0. The headline, in one table

**MEASURED.** Each of the five, run twice: once with no `CRATONVM_ARGS`
(the `SUITE=all` arm, i.e. Compatible / `--real-jdk`) and once with
`--jdk-only`. Same binary, same classpath, same launch args, same minute.

| vector | Compatible (`SUITE=all`) | `--jdk-only` | HotSpot 25.0.3+9 |
|---|---|---|---|
| `RImmutableFactoryTypes` | **FAIL** — 1 divergence of 219 | **PASS (219 checks)** | PASS (219 checks) |
| `RJdkProxyIface` | **FAIL** — 7 of 9 steps | **PASS (38 checks, 9 steps)** | PASS (38 checks, 9 steps) |
| `RJdkFunctionCombinators` | **FAIL** — `Function.andThen` | **PASS (452 checks)** | PASS (452 checks) |
| `RJdkEnumerations` | **FAIL** — empty `Hashtable.keys()` | **PASS (70 checks)** | PASS (70 checks) |
| `RServiceLoaderDoubleSource` | **FAIL** — 2 providers vs 0 descriptors | **PASS (1266 checks)** | PASS (1266 checks) |

**Five of five. Strict mode is the arm that is RIGHT.**

That inverts the assumption the bar encodes. `--jdk-only` is described
throughout this directory as "an internal-diagnostic policy in wave 1 … expected
to fail where `--real-jdk` passes" (`run.sh:161-164`). On this set the opposite
holds, and it holds unanimously: **the strict policy is not a diagnostic that
costs correctness here — on these five it IS the correctness.** Every one of
these five defects is repaired by the act of refusing a stand-in.

### What that means for the acceptance criterion

The wave-H rule — *"every lane must re-measure this exact five and confirm it is
unchanged"* — is sound as a **regression tripwire** and is not challenged here.
But it has been read as "five open strict-mode defects carried as background
noise", and that reading is wrong in a way that matters: **no strict-mode change
can move any of these five, because all five already pass in strict mode.**
A lane that touches only strict-mode code and sees this set unchanged has
learned nothing about its change from this set. The five are a **Compatible-mode
regression guard** that happens to be red at rest.

The corollary is sharper. **A change that makes one of these five go GREEN in
the `SUITE=all` arm is not a violation of verdict-neutrality — it is the fix.**
The bar as written ("verdict-neutral, not green") cannot distinguish
"you broke something" from "you fixed the thing everyone has been stepping
over", because both are "the five moved". §6 N1 proposes the one-line
sharpening.

---

## 1. The instrument, so the numbers are reproducible

`run.sh` is at `fe59bf9d9` and was **not edited**. Two of the five need
launcher-supplied arguments and cannot be run bare:

* `RServiceLoaderDoubleSource` needs `class_cp_extra()`'s second class-path
  entry and `class_args()`'s three `-D` properties (`harness-guard.sh`). Run
  bare it fails on its own precondition check, which is the vector working as
  designed, not the defect.
* the other four run bare and reproduce identically bare and under `run.sh`
  (**MEASURED**, both ways, for all four) — so **trap 4 is discharged in both
  directions**: none of the five needs the suite's concurrency to appear, and
  none of them is an artefact of running alone.

Suite-level re-measurement of the stated baseline, this worktree, today:

```
CV=C:/craton/target-jdkonly-h2/release/cratonvm.exe \
JDK=/c/Program\ Files/Microsoft/jdk-25.0.3.9-hotspot \
SUITE=all ONLY="RImmutableFactoryTypes RJdkProxyIface RJdkFunctionCombinators \
                RJdkEnumerations RServiceLoaderDoubleSource" bash regression-suite/run.sh
→ REGRESSION SUITE: 0 passed, 5 failed
```

**A note on what the runner shows and what it hides.** All five print
`FAIL  cratonvm rc=1` with **no signature**, and all five additionally raise
`HARNESS ERROR [G2] … nothing survives extract()`. **That is not five crashes
and not five timeouts** (trap 5) — every one is a clean `AssertionError` from
the vector's own `check()`, printed on stderr, with the VM exiting 1 through its
normal path (`[cratonvm] shutdown hooks: … trigger=uncaught`). No SIGSEGV, no
`rust panic`, no truncation, no abort. The `[G2]` flag is the by-construction
one `run.sh:729-737` documents: a vector that throws before its banner leaves an
empty `cv.key`, so G2+G3 fire for it every time and are correctly **not counted**
as independent findings. Why the runner's own signature line came out empty for
all five is a separate and unexplained defect — N2. **Reading the five
assertions required running each vector directly and reading its stderr.** A lane that
reads only the `run.sh` summary sees five identical opaque rows, which is
exactly how five distinguishable defects stayed unclassified as a group.

---

## 2. The five assertions, verbatim

**MEASURED.** CratonVM Compatible mode, this binary, stderr:

| vector | the diverging value |
|---|---|
| `RImmutableFactoryTypes` | `Map.of(k,v) must be instanceof AbstractMap (got false); getClass()=java.util.ImmutableCollections$Map1` |
| `RJdkProxyIface` | `ClassCastException: class java.lang.invoke.MethodHandle cannot be cast to class RJdkProxyIface$Greeter` (×5 steps, ×1 `$Sink`, ×1 `$Risky`) |
| `RJdkFunctionCombinators` | `Function.andThen is the fabricated compatibility class java.util.function.Function$AndThen` |
| `RJdkEnumerations` | `empty Hashtable.keys(): the carrier is a fabricated compatibility class, java.util.Enumeration$Impl` |
| `RServiceLoaderDoubleSource` | `ServiceLoader's provider set must be exactly what the META-INF/services descriptors on the class path name: got 2 provider(s), descriptors name 0` |

Stated as divergences against the oracle rather than as "the test fails":

| | CratonVM (Compatible) says | HotSpot 25.0.3+9 says |
|---|---|---|
| `Map.of("k","v") instanceof AbstractMap` | **false** | **true** |
| `MethodHandleProxies.asInterfaceInstance(Greeter.class, mh)` | returns **the `MethodHandle` itself** | returns a generated proxy implementing `Greeter` |
| `f.andThen(g).getClass().getName()` | `java.util.function.Function$AndThen` | a generated `…$$Lambda/0x…` |
| `new Hashtable<>().keys().getClass().getName()` | `java.util.Enumeration$Impl` | `java.util.Collections$EmptyEnumeration` |
| `ServiceLoader.load(Greeter.class, appLoader)` over a `-cp` modular jar | **2** providers | **0** providers |

---

## 3. Do they group? Three mechanisms, not one and not five

The brief warned both ways: `RMapGcStress` was one defect wearing four faces;
the HashMap blast radius was at least two mechanisms. Here it is **three**, and
the evidence for each grouping is the shared source site, not the shared
symptom.

### Mechanism A — a `SyntheticStub` stand-in that Compatible mode keeps and does not need (3 of 5)

`RJdkProxyIface`, `RJdkFunctionCombinators`, `RJdkEnumerations`.

**ARGUED, from the registrations.** All three are the same shape:
a native registered under `NativeKind::SyntheticStub`, which
`NativeMethodRegistry::register_inner` **drops** under `JdkOnly` and **keeps**
under `Compatible` (`NativeKind::allowed_in` returns unconditional `true` for
`Compatible` — `H4-1` §2 established this and it is unchanged). The real JDK
bytecode behind each one **runs correctly** — that is what the `--jdk-only`
column of §0 proves, per vector, with its full check count. So Compatible mode
is dispatching to a stand-in whose only justification (that no real bytecode
answers) was already false, and in two of the three cases the in-tree comment
**says so in as many words**:

> *"So there IS a working real-bytecode fallback, which is exactly what `Bridge`
> asserts there is not"* — `native-builtins/src/phases_late/streams.rs:3203-3213`

Full treatment, per vector, with three separate remedies: **`H15-3`**.

### Mechanism B — the `instanceof`/`checkcast` opcodes do not consult the display-class rule that `Class.isInstance` does (1 of 5)

`RImmutableFactoryTypes`.

Not mechanism A: nothing is refused and nothing is fabricated at the failing
call. The receiver is a `cratonvm/internal/UnmodifiableMap` stamp whose
`getClass()` is aliased to the real `ImmutableCollections$Map1`; the **reflective**
subtype path follows that alias and answers correctly, the **opcode** path does
not follow it and answers `false`. Full treatment, including the correction it
forces on `H0-2` §5: **`H15-2`**.

### Mechanism C — a class-path-only module's `provides` clause reaches `ServiceLoader` through a door the boot-layer fix did not close (1 of 5)

`RServiceLoaderDoubleSource`. One member, treated in full in §5 below because it
shares nothing with A or B.

**Why C is not A** even though `--jdk-only` also fixes it: the strict-mode
repair here is not a refused stand-in, it is that strict mode declines the
**whole native `ServiceLoader`** and the real bytecode never consults CratonVM's
module registry at all. The Compatible-mode defect is a missing filter inside a
native that is otherwise doing its job — a wrong answer, not a substitution.

---

## 4. The three leads in the brief, adjudicated

**4a. "`RImmutableFactoryTypes` is a 219-check vector recorded as asking 1 of the
12 cells it could ask."** **CONFIRMED and re-measured.** `H0-2` §4's twelve
cells were re-run today against both VMs with a fresh 7-receiver probe (written
to this session's scratchpad, not to the repo; its full source is inlined in
`H15-2` §3 so it can be re-run) and **all twelve are still divergent** in
Compatible mode. The vector reports exactly one of them
(`Map.of(k,v) instanceof AbstractMap`, its `hierarchy()` block) and **218 of its
219 checks pass** — so the greenness of the other 218 is real coverage of real
rules, and the single red cell is a true 1-of-12 report, not a 1-of-12 fixture.
`H0-2` §4's `RandomAccess` row — the one with a performance contract attached —
is independently re-confirmed here (§ `H15-2` 3).

**4b. "`RServiceLoaderDoubleSource` may be diagnosed already and merely
unfixed — or the diagnosis may have expired."** **The diagnosis EXPIRED, and
that is the finding.** `D1-R11`'s mechanism (a `-cp` modular jar promoted into
`ModuleLayer.boot()`, whose providers then reach `ServiceLoader` through both
doors) is correct AND **its fix landed** (`E4-R11`): the
`ctx.module_is_class_path_only(&name) { continue; }` guard is present today in
`native-builtins/src/jboss_jdkspecific.rs:432`. **MEASURED** that it works — the
vector's `ModuleLayer.boot().findModule(...)` assertion **passes**, and its
`resolved.size() == distinct.size()` duplicate check **passes**. What still
fails is the check **after** those two, and it is a different door. §5.

**4c. "`RJdkEnumerations` fails when `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/
Hashtable` is armed, with `Cannot read field "modCount" because "this.this$0" is
null`, and prints `Properties own key count: 0`. Its unarmed failure may or may
not share that mechanism. Check; do not assume."** **CHECKED — it does NOT
share it.** **MEASURED**, unarmed Compatible run: `Properties own key count` is
not printed at all; the vector prints
`CK RJdkEnumerations properties names=14 own=13 first=base.only` — **13, not 0**
— and then `CK … chm keys=17 vals=17`, i.e. the `Properties` and
`ConcurrentHashMap` families **both pass**. It dies later, in
`hashtableEnumerations()`, and **not** on the 11-entry table (which passes: it
gets a real `Hashtable$Enumerator`) but on the **empty** one. The unarmed defect
is a size-conditioned fallback inside `native_hashtable_keys`; the armed defect
is a view carrier with a null outer reference. **Two mechanisms wearing one
vector's name.** Detail in `H15-3` §3.

**4d. "`RJdkProxyIface` fails when ConcurrentHashMap is armed too — the proxy
cache is built on a map whose contents the VM owns."** **The unarmed failure is
NOT that.** **MEASURED**: the unarmed CCE names
`java.lang.invoke.MethodHandle`, and the shim that produced it
(`native-builtins/src/lang_invoke.rs:8063-8071`) `return`s `args[1]` **without
touching any map**. Whether the armed failure is a cache defect is untested here
and stays open.

**4e. "Several classes in this corpus fail on HotSpot JDK 25 as well."**
**NONE of these five is one of those.** **MEASURED**: all five PASS on
HotSpot 25.0.3+9, with the check counts in §0's third column, in exactly the
configuration `run.sh` schedules them in.

---

## 5. Mechanism C in full — `RServiceLoaderDoubleSource`

### 5.1 The measurement that names the door

`CRATONVM_DIAG_SERVICELOADER=1`, Compatible mode, **MEASURED**:

```
[SL-DBG] ServiceLoader service=com.cratonvm.jdkonly.svc.Greeter loader_delegation=false
         descriptors=0 providers=2
         (["com.cratonvm.jdkonly.svc.internal.EnGreeter",
           "com.cratonvm.jdkonly.svc.internal.FactoryGreeter"])
```

`descriptors=0` and `providers=2`. There is no `../../../apps/META-INF/services` descriptor
anywhere for `Greeter`; both names come from the `provides` clause in
`regression-suite/modules/cratonvm.jdkonly.svc/module-info.java:22-24`, and that
jar is on **`-cp`**, not on `--module-path` (the vector asserts
`System.getProperty("jdk.module.path") == null` as a precondition and that
assertion passes).

HotSpot answers **0**, because a modular jar reached through the class path is
an unnamed-module citizen and its `module-info` — every `provides` clause
included — is ignored outright.

### 5.2 The named Rust function

`native-builtins/src/service_loader.rs`, in the provider-collection body:

```rust
    if !loader_view_is_exhaustive {
        let service_slash = service_name.replace('.', "/");
        for mp in ctx.service_providers_from_modules(&service_slash) {
            providers.push(mp.replace('/', "."));
        }
    }
```

The guard on that arm is `loader_view_is_exhaustive` — a property of the
**loader**. Its own comment states the intended rule as
*"A loader with its own recorded URL list IS a class-path loader, so skip the
module source for it"* — but the receiver here is the **system application
loader**, which does not carry such a list, so the guard is false and the arm
fires. **The guard asks about the loader; the rule is about the MODULE.**

That is the same rule `populate_boot_layer_modules` already gets right, using
the accessor built for it in the `E4-R11` landing:

```rust
        if ctx.module_is_class_path_only(&name) {
            continue;
        }
```

The two sites were fixed a day apart, and only one of them got the module-shaped
predicate. **ARGUED** from the two bodies; the `descriptors=0 providers=2` line
is the **MEASURED** half.

`ctx.service_providers_from_modules` bottoms out at
`vm/src/vm/vm_exec.rs:9511`, which forwards verbatim to
`ModuleRegistry::service_providers` (`classloading/src/module.rs:1055`) — and
that function walks `self.modules.values()` with **no filter at all**, returning
provider names with the owning module discarded. So the filter cannot be applied
at the consumer: by the time `service_loader.rs` sees the strings, the module
they came from is gone.

### 5.3 The patch — written out, NOT applied

Two hunks. The first adds the filtered query beside the predicate that already
states the rule; the second points the single VM-side accessor at it. Nothing in
`native-builtins` changes, so the three ServiceLoader-shaped consumers
(`service_loader.rs:1281`, `service_loader.rs:1538`, `servlet.rs:1633`) all
inherit the fix without three edits — and `ModuleRegistry::service_providers`
keeps its existing contract and its existing unit tests
(`classloading/src/module.rs:2329-2336`), which is why this is an ADD and not an
edit of that function.

**Hunk 1** — `classloading/src/module.rs`, immediately after
`service_providers` (currently ending at line 1064):

```rust
    /// [`Self::service_providers`], minus every module whose only source is the
    /// application CLASS path.
    ///
    /// This is the module-shaped spelling of the rule
    /// [`Self::is_class_path_only`] states and `populate_boot_layer_modules`
    /// already applies: a modular jar reached through `-cp` is an
    /// unnamed-module citizen, and a real JVM ignores its `module-info`
    /// outright — every `provides` clause included. `service_providers` cannot
    /// express it, because it returns provider NAMES with the owning module
    /// discarded, so the filter has to live here rather than at the consumer.
    ///
    /// Platform modules are unaffected: `ClassManager` stamps `automatic =
    /// false` for any `is_platform_module_name`, so `jdk.compiler`'s
    /// `provides javax.tools.JavaCompiler` — the declaration
    /// `ToolProvider.getSystemJavaCompiler()` depends on — still answers here.
    /// Genuine `--module-path` modules are unaffected for the same reason:
    /// `vm_init` re-registers them explicit.
    ///
    /// docs/known-issues/jdk-only/H15-1-the-five-gate-failures-are-compatible-mode-defects-20260820.md §5
    pub fn service_providers_from_declared_modules(&self, service_class: &str) -> Vec<String> {
        let mut providers = Vec::new();
        for desc in self.modules.values() {
            if desc.automatic {
                continue;
            }
            for p in &desc.provides {
                if p.service == service_class {
                    providers.extend(p.with.iter().cloned());
                }
            }
        }
        providers
    }
```

**Hunk 2** — `vm/src/vm/vm_exec.rs:9511-9519`, the whole method body:

```rust
    fn service_providers_from_modules(&self, service_class: &str) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .service_providers(service_class)
    }
```

becomes

```rust
    // A `-cp` modular jar's `provides` clause must NOT reach `ServiceLoader`:
    // on a real JVM that jar is an unnamed-module citizen and its
    // `module-info` is ignored outright. `service_loader.rs`'s module arm
    // guards on `loader_view_is_exhaustive` — a property of the LOADER — and
    // the system application loader carries no recorded URL list, so the guard
    // is false there and the arm fires. The rule is about the MODULE, and this
    // is the one place every consumer of the module source passes through.
    // docs/known-issues/jdk-only/H15-1-the-five-gate-failures-are-compatible-mode-defects-20260820.md §5
    fn service_providers_from_modules(&self, service_class: &str) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .service_providers_from_declared_modules(service_class)
    }
```

### 5.4 What must be re-measured before this is believed

**PREDICTED**, with falsifiers:

1. `RServiceLoaderDoubleSource` goes **GREEN** in the `SUITE=all` arm
   (1266 checks). Falsifier: it still reports `providers=2`, meaning
   `desc.automatic` is not `true` for this module — in which case the `E4-R11`
   guard would also be inert and `findModule` would be non-empty, which it is
   not, so this is a tight prediction.
2. `RJdkModule` stays **GREEN**. This is the load-bearing negative control:
   `RJdkModule.moduleServices()` asserts the `--module-path` module's providers
   arrive through **both** routes (`:233` the catalog route, `:242` the layer
   route), and those must survive. Falsifier: it goes red, meaning `vm_init`'s
   explicit re-registration is not reaching this module.
3. `RJdkServices` and the JDK's own in-process tooling stay green
   (`ToolProvider.getSystemJavaCompiler()` is the named consumer of the platform
   half). Falsifier: a `null` compiler.
4. `--jdk-only` is **unmoved** on all 104 — strict mode does not reach this
   native at all.

---

## 6. NOMINATIONS

**N1 — the wave-H acceptance bar cannot express "you fixed one of the five", and
should be split into two lines.** As written ("re-measure this exact set and
confirm it is unchanged") a lane that lands `H15-2` or `H15-3` reports a
*violation*. The bar's real content is two different claims and they want two
different tests:
* **`--jdk-only` must stay 104/104.** This is the strict-mode neutrality bar and
  it is the one that catches a broken strict-mode change. It is currently
  *unstated* in the wave-H brief, and it is the stronger of the two.
* **the `SUITE=all` failure set must not GROW, and must not change membership
  except by removal.** That admits a fix and still catches a regression. The
  five names belong in `run.sh` as an expected-fail list with this record's path
  beside them, so the next lane does not have to re-derive that they are
  Compatible-mode-only.

**N2 — `run.sh` shows five identical opaque rows for five distinguishable
defects, and I can only half-explain it. Both halves are worth someone's
afternoon.**

*Half one, MEASURED and reproduced twice.* Every one of the five prints
`FAIL  cratonvm rc=1` — the **initial** `why`, i.e. `run.sh:677`'s
`[ -n "$sig" ] && why="rc=$cvrc: $sig"` did **not** fire, so `$sig` came out
empty inside the run. **I cannot reproduce that in isolation.** Running
`run.sh:666`'s command verbatim (same `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
timeout 120 …` prefix, same absolute `-cp`, `2>&1` captured into a variable) and
then `run.sh:676`'s pipeline verbatim yields `SIGLEN=90`, not 0. Checked and
ruled out: no shadowing function or alias for `grep`/`sed`/`tail`/`head` in
`run.sh` or `harness-guard.sh`; `cvrc` is captured before `cvkey` so `$?` is not
clobbered; the block's bytes are exactly as read (`cat -A`). **So the suite's
signature line is silently inert for this whole failure class and nobody knows
why.** That is a bigger finding than the truncation below, and it is the reason
five real divergences read as one undifferentiated `rc=1`.

*Half two, MEASURED.* Even when the pipeline **does** fire, it reports nothing
useful. `tail -1` selects the `[cratonvm] main-vm run() Err (debug):` repeat over
the `returned Err:` line, and `head -c 90` then spends the entire 90-character
budget on the prefix — the isolated run returns exactly:

```
[cratonvm] main-vm run() Err (debug): Exception in thread "main" java/lang/AssertionError:
```

— cut off at the colon, **immediately before the divergence text**. `head -1`
instead of `tail -1` would not help (same prefix); what would is stripping the
`[cratonvm] …: Exception in thread "main" ` preamble before the 90-char budget
is spent, or simply raising it. Every one of §2's five assertions fits in ~120
characters after the preamble.

(Neither half applied — this lane may not edit `run.sh`.)

**N3 — the `SUITE=all` arm is the only arm that exercises Compatible mode for
the 40 `RJdk*` vectors, and nobody has said that out loud.** `JDKONLY_CLASSES`
is scheduled under `SUITE=all` **without** `--jdk-only`, which means those 40
policy vectors are, in that arm, a **Compatible-mode conformance suite** — and
§0 shows it is finding real Compatible-mode defects. `run.sh` documents the list
as "expected to fail where `--real-jdk` passes", i.e. the exact opposite
expectation. One paragraph in `run.sh` correcting that, plus the note that
`class_cv_args` pins `--jdk-only` for `RJdkSqlPackage` **for this reason and
this reason only**, would stop the next lane concluding these vectors are
mis-scheduled. They are not mis-scheduled — they are load-bearing.

**N4 — `ModuleRegistry::service_providers` returns names with the owning module
discarded, and that shape is why §5's filter could not live at the consumer.**
Two of three consumers of the module source have now needed a
per-module predicate (`populate_boot_layer_modules` needed
`module_is_class_path_only`; `service_loader.rs` needs it and does not have it).
A `Vec<(String /*module*/, String /*provider*/)>` variant would let the next
consumer ask its own question without a third accessor. Not proposed as part of
§5's patch, which is deliberately the smallest change that closes the measured
defect.

**N5 — nothing in the corpus asks `Collections.binarySearch(List.of(...), x)` and
diffs the ALGORITHM, only the answer.** `H15-2` §3 re-confirms
`List.of(...) instanceof RandomAccess` is `false` in Compatible mode; the probe
here shows `binarySearch` still returns the right index (`2`), because the
iterator fallback is correct — just O(n) instead of O(log n) and, for
`Collections.reverse`, O(n²). A vector that asserts a *result* cannot see this.
The only instrument that could is a counted-`ListIterator` receiver, and there
is no such vector.

---

## 7. What I did NOT solve

* **`RJdkFunctionCombinators` is diagnosed to its FIRST failure only.** The
  Compatible run dies at `Function.andThen`, which is the third family the
  vector exercises out of seven. `H15-3` §2 enumerates **eleven** fabricated
  combinator names across three registrars by grep (**ARGUED**), and there is no
  measurement of how many of them the vector would report once `andThen` is
  fixed. It is one fix or it is up to five; I cannot tell you which without a
  build.
* **The armed (`CRATONVM_ENFORCE_NATIVE_SHADOW`) failures of `RJdkEnumerations`
  and `RJdkProxyIface` are untouched.** §4c/§4d establish only that the *unarmed*
  failures are different mechanisms. The armed ones remain unexplained.
* **No patch here has been compiled.** All three records' patches are written
  against source read today at `fe59bf9d9`; line numbers will have moved, so
  grep the literal.
