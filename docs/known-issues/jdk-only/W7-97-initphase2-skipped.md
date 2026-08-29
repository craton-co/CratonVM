# `System.initPhase2` is skipped — and it cannot currently be run, because the boot-loader's `FileSystem` hands out inert `Path`s

> **STATUS 2026-08-12: the skip is CORRECT and the stated reason for it was
> WRONG.** `initPhase2` was skipped on the premise that the module graph "pulls
> in subsystems we don't implement". That premise was never tested. It has now
> been tested, by invoking `System.initPhase2` reflectively inside the VM under
> test, and it is false in the interesting direction: nothing about the module
> graph stops it. It dies four frames earlier, in `java.nio`, on a defect that
> has nothing to do with modules — `sun.nio.fs.DefaultFileSystemProvider
> .theFileSystem()` returns a SECOND `WindowsFileSystem` whose `Path`s do not
> work. **That `java.nio` defect, not the module system, is the thing blocking
> `initPhase2`**, and it is filed below as the nomination this record exists to
> hand over.
>
> The behavioural half is closed: the module system is now initialised in the
> `initPhase2` slot in `vm-cli/src/main.rs`, via the call this VM actually has
> for it, before the init level advances to `SYSTEM_BOOTED`.

## 1. The symptom, and why it hid

Under `--jdk-only`, `ServiceLoader` returned **zero** module-declared providers
for the whole process, while classpath `../../../apps/META-INF/services` providers kept
working. A `ServiceLoader` probe therefore reads green; only a provider declared
by a `provides` clause in the JDK image is lost.

Measured on `/c/craton/jdkonly-wave2-target/release/cratonvm.exe` (mtime
2026-08-12 20:18 — a binary that predates *both* the `ModuleLayer.boot()`
stopgap in `f0a472dcf` and the change described here, so these rows are the true
before-state), against HotSpot 25.0.3.9 as the oracle. Provider **identities**
were compared, not just counts:

| service | HotSpot 25 | CratonVM `--jdk-only` | CratonVM `--real-jdk` |
|---|---|---|---|
| `java.nio.file.spi.FileSystemProvider` | 2 | **0** | 2 |
| `java.util.spi.ToolProvider` | 9 | **0** | 9 |
| `javax.tools.JavaCompiler` | 1 | **0** | 1 |
| classpath `../../../apps/META-INF/services` control | 1 | 1 | 1 |
| `ToolProvider.getSystemJavaCompiler() != null` | true | **false** | true |

Controlled: three consecutive `ServiceLoader.load` calls with no `ModuleLayer`
touch answered 0 every time, so it is not warm-up.

`--real-jdk` was green *because* the `ServiceLoader` natives
(`native-builtins/src/service_loader.rs`, `NativeKind::SyntheticStub`) cover the
gap. `--jdk-only` drops that kind at registration and runs the real
`java.util.ServiceLoader` bytecode — so strict mode did not merely lose the
compatibility layer, it inherited a **silent wrong answer** the compatibility
layer had been hiding. The consequence was found in a corpus and not in a probe:
H2's `SourceCompiler` branches on `ToolProvider.getSystemJavaCompiler()`, null
only under `--jdk-only`, and so takes a `com.sun.tools.javac` path HotSpot never
runs — a divergence four levels from its cause.

## 2. Why `initPhase2` was skipped, and whether that reason holds

The skip site (`vm-cli/src/main.rs`, before this change) said:

> `WP1.3: initPhase2 / initPhase3 are pure-Java methods on java.lang.System that
> finalise modules + classpath and install ClassLoader.scl. We don't run them
> end-to-end in cratonvm (the real-JDK module graph resolution pulls in
> subsystems we don't implement) […] INTENTIONAL (reviewed): skipping
> initPhase2/3 here is a deliberate boot-sequencing choice, NOT a silent
> wrong-result stub. […] Running the real initPhase2/3 is gated on module-system
> subsystems we do not yet implement; if/when those land this skip should be
> revisited.`

Three claims, adjudicated:

1. **"the module graph pulls in subsystems we don't implement"** — **not
   established, and misleading.** `initPhase2` never reaches module resolution.
   It dies in `java.nio` (§3). The comment named a plausible suspect and no one
   ever ran the thing.
2. **"NOT a silent wrong-result stub"** — **false as of `--jdk-only`.** It was
   true while a stub covered for it. §1 is the wrong result.
3. **"level held at 2 … bumping past it would send those callers down a
   null-deref path"** — **already self-contradicted in the same file.** The CLI
   bumps to 3 and 4 unconditionally about twenty lines below, and writes the
   real `jdk.internal.misc.VM.initLevel` field to 4 (`spring-bug-05`). The
   caller the comment worried about — `ClassLoader.getSystemClassLoader()`,
   whose `default:` arm asserts `scl != null` — is a CratonVM native with no
   init-level gate at all (`native-builtins/src/classloader_real.rs`,
   `native-builtins/src/classloader.rs`), so the level it reads decides nothing.
   The stated hazard cannot occur.

So: the right call, held up by a wrong reason, for long enough that the reason
stopped describing the code around it.

## 3. Can `initPhase2` be run? No — and this is the durable part

**Ran**, one process per arm, `--jdk-only`, reflectively invoking
`System.initPhase2(boolean, boolean)` inside the VM under test:

```
INITPHASE2_RC -1
java.io.UncheckedIOException
  at jdk.internal.jimage.ImageReader$SharedImageReader.imageFileAttributes(ImageReader.java:526)
  at jdk.internal.jimage.ImageReader$SharedImageReader.newDirectory(ImageReader.java:534)
  at jdk.internal.jimage.ImageReader$SharedImageReader.buildRootDirectory(ImageReader.java:331)
  at jdk.internal.jimage.ImageReader$SharedImageReader.findNode(ImageReader.java:508)
  at jdk.internal.jimage.ImageReader.getModuleNames(ImageReader.java:169)
  at jdk.internal.module.SystemModuleFinders.ofModuleInfos(SystemModuleFinders.java:223)
  at jdk.internal.module.ModuleBootstrap.boot2(ModuleBootstrap.java:241)
  at jdk.internal.module.ModuleBootstrap.boot(ModuleBootstrap.java:169)
  at java.lang.System.initPhase2(System.java:1933)
Caused by: java.nio.file.NoSuchFileException:            <- note the EMPTY path
```

`-1` is `JNI_ERR`; HotSpot aborts VM creation on it.

**Read** (`src.zip`, JDK 25.0.3.9): `imageFileAttributes()` is
`Files.readAttributes(getImagePath(), BasicFileAttributes.class)`, and the path
comes from `ImageReaderFactory`, whose static initialiser is

```java
if (ImageReaderFactory.class.getClassLoader() == null) {
    fs = (FileSystem) Class.forName("sun.nio.fs.DefaultFileSystemProvider")
            .getMethod("theFileSystem").invoke(null);
} else {
    fs = FileSystems.getDefault();
}
BOOT_MODULES_JIMAGE = fs.getPath(JAVA_HOME, "lib", "modules");
```

`ImageReaderFactory` is boot-loader-defined, so it takes the **first** branch.
That branch is the defect. **Ran**, from user code, `--jdk-only` vs HotSpot:

| probe | HotSpot 25 | CratonVM `--jdk-only` |
|---|---|---|
| `theFileSystem() == FileSystems.getDefault()` | **true** | **false** |
| both report class | `sun.nio.fs.WindowsFileSystem` | `sun.nio.fs.WindowsFileSystem` |
| `bad.toString()` (explicit call) | correct, 59 chars | correct, 59 chars |
| `"" + bad` / `String.valueOf(bad)` | correct | **`""`** |
| `bad.equals(good)` | true | **false** |
| `bad.getNameCount()` | 5 | 5 |
| `Files.exists(bad)` | true | **false** |
| `Files.readAttributes(bad, …)` | ok, 144908395 | **`NoSuchFileException: `** |
| `Files.readAttributes(good, …)` | ok | ok |

`good` is the same path from `FileSystems.getDefault()` and works in both VMs.
So the runtime-image path is built from a **second, half-wired
`WindowsFileSystem` instance**, and every `Path` it mints is inert: correct under
a direct `toString()` call, empty under `String.valueOf`, unequal to its own
twin, and non-existent to the filesystem. That empty rendering is exactly the
empty `NoSuchFileException` message that kills `initPhase2`.

Two negative controls, both **ran**:

* the jimage machinery itself is fine — `ImageReader.open(Path.of(home, "lib",
  "modules"))` from user code returns `getModuleNames()` = **70** under
  `--jdk-only`, identical to HotSpot, with a correct `getImagePath()`;
* `Files.readAttributes` itself is fine — over a `FileSystems.getDefault()` path
  it answers the exact 144908395-byte size HotSpot does.

**Conclusion: `initPhase2` cannot be run today, and the blocker is not the
module system.** Repair the `java.nio` defect (§6, nomination 1) and this
becomes re-testable in one command; until then, invoking `initPhase2` would add
a guaranteed `JNI_ERR` and a stack trace to every boot and fix nothing.

### 3.1 A trap this record exists to disarm

The first run of the probe passed `printStackTrace = true`, and the provider
counts flipped 0 → 2/9/1 **after** the failed `initPhase2`. That looks exactly
like "running initPhase2 fixed it". It did not. Re-run with
`printStackTrace = false`: `rc = -1` and the counts stay **0**. The flip came
from printing the stack trace, which walks module metadata and materialises the
boot layer as a side effect. A bisect — one action per process — puts it beyond
doubt:

| action taken before the first `ServiceLoader.load` | `ToolProvider` | `JavaCompiler` |
|---|---|---|
| none | 0 | 0 |
| `String.class.getModule()` | 0 | 0 |
| `getDeclaredMethod` + `setAccessible` | 0 | 0 |
| `Class.forName("jdk.internal.module.ModuleBootstrap")` | 0 | 0 |
| `System.initPhase2(true, false)` → `rc=-1` | 0 | 0 |
| **`ModuleLayer.boot()`** | **9** | **1** |

## 4. What actually initialises the module system here, and the fix

`ModuleLayer.boot()` — and not the JDK's version of it. `register_jboss_jdkspecific`
(`native-builtins/src/jboss_jdkspecific.rs`) re-registers `ModuleLayer.boot`
last-writer-wins over the `phases_late` stub, and its `build_boot_layer` →
`populate_boot_layer_modules` → `register_module_in_loader_catalog` runs
`ServicesCatalog.getServicesCatalog(systemLoader).register(module)` for every
registered module. Both registrations are `NativeKind::Bridge`, which is why
strict mode keeps them. **That native is this VM's `ModuleBootstrap.boot()`.** It
was merely lazy: nothing invoked it during boot, so the first `ServiceLoader` in
a process saw an empty catalog.

Applied in `vm-cli/src/main.rs`:

* the call is made in the **`System.initPhase2` slot** — inside the
  `java_home.is_some()` block, immediately after `initPhase1`, and **before**
  `set_init_level(3)/(4)` and the real `VM.initLevel(4)` field write. Ordering is
  the point: `VM.initLevel(4)` is real bytecode that wakes every
  `awaitInitLevel` waiter, and a thread woken at `SYSTEM_BOOTED` is entitled to
  assume the module system came up at level 2. The prior placement (after the
  bump) left that window open.
* the earlier unconditional call at the old site is **removed**, not stacked —
  there is exactly one, so no double-init.
* its only externally ordered dependency, `ClassLoader.getSystemClassLoader()`,
  is a native with no init-level gate, so the earlier placement does not starve
  it.
* the result is **not discarded**. `let _ = vm.invoke(...)` became a `match`: a
  missing boot layer or an `Err` logs a `WARN` naming the consequence (every
  module-declared provider silently absent) and noting that HotSpot aborts VM
  creation when `initPhase2` returns non-zero. It warns rather than aborting
  because this substitute is narrower than the JDK's phase.

### 4.1 State of verification — read this before quoting an "after"

The lane that made this change **may not build**. The after-state below is
therefore a **simulation, not a binary measurement**: the same call, made from
Java as the first statement of `main` on the same 20:18 binary, which places it
before any service lookup exactly as the CLI now does — but *after*, not before,
the init-level bump.

| | before (measured) | after (simulated) | HotSpot |
|---|---|---|---|
| `--jdk-only` `FileSystemProvider` | 0 | 2 | 2 |
| `--jdk-only` `ToolProvider` | 0 | 9 | 9 |
| `--jdk-only` `JavaCompiler` | 0 | 1 | 1 |
| `--jdk-only` `getSystemJavaCompiler() != null` | false | true | true |
| `--jdk-only` classpath control | 1 | 1 | 1 |
| `--real-jdk` all four | 2 / 9 / 1 / true | 2 / 9 / 1 / true | — |

Provider identities matched HotSpot's exactly in every green cell, all nine
`ToolProvider` classes included. `--real-jdk` does **not** double-count with the
boot layer materialised first (9, not 18), so the `SyntheticStub` `ServiceLoader`
natives and the populated catalog coexist.

**What is NOT verified and must be on rebuild:** that materialising the boot
layer *before* the init-level bump behaves as it does after it. The reasoning is
in §4; the measurement is one command:

```
cratonvm.exe --jdk-only  -cp out SlProbe    # expect 2 / 9 / 1 / true, no "touch" argument
cratonvm.exe --real-jdk  -cp out SlProbe    # expect 2 / 9 / 1 / true
```

## 5. Should the `ServiceLoader` `SyntheticStub` retire with this?

**Not in this change, and not yet — but the case for it is now measurable
instead of theoretical.** Evidence for: with the catalog populated, `--jdk-only`
(where those natives are refused, so real `java.util.ServiceLoader` bytecode
runs) produces byte-identical counts *and* identities to HotSpot across all four
vectors, and `--real-jdk` with the stub active produces the same answers — the
stub is no longer load-bearing for module-declared providers. Evidence against
retiring it here: four SPIs is not a retirement census; the file is ~4,000 lines
covering the classpath path, the `provider()` factory form and the validation
gates that W6-2 and W7-85 built into it; and this change cannot be built by the
lane that made it, so pairing it with a large behavioural removal would put two
untested changes in one commit. Retirement wants its own pass with the vectors
W6-2 §2 lists.

## 6. NOMINATIONS

**1 (blocks `initPhase2`, and is a defect in its own right).**
`sun.nio.fs.DefaultFileSystemProvider.theFileSystem()` returns a `WindowsFileSystem`
that is not identity-equal to `FileSystems.getDefault()`, and `Path`s minted from
it are inert (`Files.exists` false, `readAttributes` throws `NoSuchFileException`
with an empty message, `equals` against the same path from the default filesystem
false, `String.valueOf` empty while a direct `toString()` is correct). HotSpot 25
returns the one shared instance. Owner: the `sun.nio.fs` natives. This is the
whole reason `initPhase2` is unreachable, and it is silently wrong for any
boot-loader-defined JDK class that builds a path this way — `ImageReaderFactory`
is simply the one that got caught. Reproduction: §3, `BootFsProbe`.

**2 (sub-defect of 1, or independent — worth separating).** `String.valueOf(p)`
and string concatenation answer `""` for an object whose own `toString()` answers
59 correct characters, in the same expression, evaluated left to right. Whatever
serves the concat path is not dispatching `toString()` virtually for this
receiver.

**3 (fragility, no observed failure).** `java/lang/ModuleLayer.boot` is
registered **twice** — `phases_late/reflect_invoke.rs` `register_p59_module`
(1-field synthetic layer, populates no catalog) and
`jboss_jdkspecific.rs` `register_jboss_jdkspecific` (`build_boot_layer`, which
does). The second wins only by last-writer-wins registration order, and the
module system now depends on that. The dead first registration should go, or the
ordering should be asserted.

## 7. Residuals

* **The after-state is unmeasured on a binary containing the change** (§4.1).
* **`initPhase3` is still skipped and is not analysed here.** It installs
  `ClassLoader.scl` and the TCCL, and `getSystemClassLoader()`'s `default:` arm
  asserts `scl != null` at level ≥ 4 — an assertion CratonVM only escapes because
  the method is natively shadowed. Nothing measured it.
* **`SystemModuleFinders.ofSystem()` took the slow fallback.** It reached
  `ofModuleInfos()`, meaning the jlink-generated `SystemModules` fast path
  answered null. Unexamined; irrelevant while nomination 1 stands, but it would
  decide whether a repaired `initPhase2` is fast or parses 70 `module-info.class`
  files on every boot.
* **`ModuleLayer.boot()` reports 70 modules where HotSpot's boot layer has 62.**
  CratonVM's layer is the whole registry, not a resolved graph. Every provider
  identity still matched, so nothing observed depends on it — but the two numbers
  are not the same object and a future record should not read the 70 as agreement.
