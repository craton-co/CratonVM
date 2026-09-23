# W8-C14-1 — the default `FileSystem`'s second door, now wired

> **STATUS: FIX APPLIED** in `native-builtins/src/phases_late/nio_file.rs`
> (this lane's file). The HotSpot column is **measured**; every CratonVM
> "after" value is **PREDICTED** — this lane may not run the VM binary.
>
> Predecessor: `W8-C4-3-default-filesystem-two-doors.md`, which located the
> defect exactly and could not apply it because `nio_file.rs` was not that
> lane's file. This record verifies that diagnosis (it holds), **corrects its
> scope** (the caller set is far larger than the one jimage class it named),
> and **corrects one of its conclusions** (`instance()` is not safely
> deferrable — see W8-C14-2).

Lane C14, 2026-08-12. Oracle: Microsoft OpenJDK `25.0.3+9` (Temurin build),
windows/x64. Probes: `scratchpad/c14/FsIdentity.java`,
`scratchpad/c14/FsIdentityMutant.java`. Fixture:
`regression-suite/src/RFsSingleton.java`.

---

## 1. The invariant

`java.nio.file.FileSystems.getDefault()` and
`sun.nio.fs.DefaultFileSystemProvider.theFileSystem()` are two doors onto **one
object**. On HotSpot both reach the single `WindowsFileSystemProvider` created
in `DefaultFileSystemProvider.<clinit>`, which holds one `theFileSystem` field.

This is a **singleton contract, so it must be tested with `==`**. An
`.equals`-shaped assertion passes against exactly the defect described here,
and the `jdk_concrete_getclass_alias` mapping makes both objects report
`sun.nio.fs.WindowsFileSystem`, so a class-name assertion passes too. Section 3
demonstrates both traps rather than asserting them.

## 2. The oracle, verbatim

```
$ java --add-opens java.base/sun.nio.fs=ALL-UNNAMED -cp out FsIdentity
getDefault==getDefault|true
Paths.get.getFileSystem==getDefault|true
Path.of.getFileSystem==getDefault|true
getDefault.getPath.getFileSystem==getDefault|true
theFileSystem.class|sun.nio.fs.WindowsFileSystem
getDefault.class|sun.nio.fs.WindowsFileSystem
theFileSystem==theFileSystem|true
theFileSystem==getDefault|true
instance.class|sun.nio.fs.WindowsFileSystemProvider
instance==instance|true
provider==provider|true
getDefault.provider==instance|true
theFileSystem.provider==getDefault.provider|true
installedProviders.get0==getDefault.provider|true
installedProviders.get0 stable|true
getFileSystem(file:///)==getDefault|true
theFileSystem.getPath.toString.length|59
String.valueOf(theFsPath).length|59
concat(theFsPath).length|59
theFsPath.getNameCount|5
theFsPath.equals(defPath)|true
Files.exists(theFsPath)|true
Files.exists(defPath)|true
readAttributes(theFsPath).size|144908395
readAttributes(defPath).size|144908395
NEG.getPath==getPath|false
NEG.newObject==newObject|false
NEG.defaultFs==zipFsProviderScheme|false
```

`--add-opens java.base/sun.nio.fs=ALL-UNNAMED` is required **on both arms**;
without it `setAccessible(true)` throws `InaccessibleObjectException` on HotSpot
too and the probe measures its own reach.

The three `NEG.*` rows print **false**. They are there because every other row
is expected `true`: without them, an `==` that had degenerated into
always-true would pass the whole probe and the greens would mean nothing.

## 3. The mutation — proof the green rows can go red

`FsIdentityMutant.java` constructs a second `WindowsFileSystemProvider`
reflectively — precisely what CratonVM's `--jdk-only` mode did by letting the
real `<clinit>` run — and re-runs the load-bearing rows:

```
$ java --add-opens java.base/sun.nio.fs=ALL-UNNAMED -cp out FsIdentityMutant
BASE.theFileSystem==getDefault|true
BASE.getDefault.provider==instance|true
MUT.secondProvider.class|sun.nio.fs.WindowsFileSystemProvider
MUT.secondFs.class|sun.nio.fs.WindowsFileSystem
MUT.theFileSystem==getDefault|false
MUT.getDefault.provider==instance|false
MUT.secondFs.provider==instance|false
MUT.EQUALSTRAP.the2.equals(d1)|false
MUT.EQUALSTRAP.path2.equals(path1)|true
MUT.path2.toString.length|59
MUT.String.valueOf(path2).length|59
MUT.path2.getFileSystem==secondFs|true
MUT.Files.exists(path2)|true
MUT.readAttributes(path2).size|144908395
```

Two results matter beyond "the rows flip":

1. **`MUT.EQUALSTRAP.path2.equals(path1)|true`.** A `Path`-equality assertion
   stays green straight through the mutation. That is the equality-shaped test
   that would have certified this defect as fixed.
2. **`MUT.readAttributes(path2).size|144908395`.** On HotSpot a second file
   system's paths work **perfectly**. So CratonVM's inert-`Path` failure is
   **not** an inherent consequence of having two file systems — it is
   CratonVM's own field-0 layout assumption (§5). Keeping those two effects
   separate matters: closing the split removes the trigger, it does not repair
   the layout assumption, and §5 of W8-C14-3 records what still stands behind it.

## 4. Who actually calls door 2 — the scope correction

W8-C4-3 justified the fix with `jdk.internal.jimage.ImageReaderFactory`. That
is real but it is the *smallest* caller. Grepping the JDK 25 `src.zip`
(`lib/src.zip`, 15,057 files, all modules) for `theFileSystem` gives the
complete java.base caller set:

| Caller | Shape | Why it matters |
|---|---|---|
| `java.util.zip.ZipFile$Source.builtInFS` | `private static final`, then `Files.readAttributes(builtInFS.getPath(file.getPath()), ...)` | Runs on **every zip/jar open** |
| `java.io.FilePermission.builtInFS` | `private static final`, plus derived `here`, `EMPTY_PATH`, `DASH_PATH`, `DOTDOT_PATH` | Class-init of a core `java.io` type |
| `java.io.WinNTFileSystem.isInvalid()` | `DefaultFileSystemProvider.theFileSystem().getPath(pathname)` | **Windows-only**; the `File` path-validity check |
| `java.nio.file.FileSystems.getDefault()` | the `else` of `if (VM.isModuleSystemInited())` | For all of early boot, **`getDefault()` IS `theFileSystem()`** |
| `jdk.internal.jimage.ImageReaderFactory.<clinit>` | reflective `Class.forName(...).getMethod("theFileSystem")` | boot-loader-defined, always takes this branch |

The fourth row is the one that reframes the defect. `FileSystems.getDefault()`
compiles to (`javap -c`, verified):

```
 0: invokestatic  jdk/internal/misc/VM.isModuleSystemInited:()Z
 3: ifeq          10
 6: getstatic     java/nio/file/FileSystems$DefaultFileSystemHolder.defaultFileSystem
 9: areturn
10: invokestatic  sun/nio/fs/DefaultFileSystemProvider.theFileSystem:()Ljava/nio/file/FileSystem;
13: areturn
```

Since CratonVM skips `initPhase2` (`W7-97`), `VM.isModuleSystemInited()` is a
plausible **false for the whole run**. The only reason that branch was not
already the dominant path is that `getDefault()` is itself natively intercepted
so its bytecode never executes — i.e. the interception was masking how central
door 2 is.

## 5. Mechanism of the inert `Path`s — verified, not inherited

`javap -p --module java.base sun.nio.fs.WindowsPath`, instance fields in
declaration order:

```
private final sun.nio.fs.WindowsFileSystem fs;      <-- field 0
private final sun.nio.fs.WindowsPathType type;      <-- field 1
private final java.lang.String root;                <-- field 2
private final java.lang.String path;                <-- field 3
```

`p57_read_path` reads `P57_PATH_FIELD` = **0**. Both halves of W8-C4-3's claim
are therefore confirmed: the constant is 0, and a real `WindowsPath`'s field 0
is a `FileSystem` **object**, not the path `String` (which is field 3).

The chain from there, each link read rather than assumed:

1. `ctx.get_field(path_obj, 0)` yields the `WindowsFileSystem` object. **In
   bounds** — a real `WindowsPath` has 7 instance fields.
2. `ctx.read_string(that)` returns `None`. It is **correctly** guarded — see
   W8-C14-3 §1, which is where this lane departs from the brief it was given:
   this is *not* a memory-safety-adjacent wrong-typed read and does *not*
   warrant a VM-FATAL classification.
3. The documented fallback `ctx.invoke_virtual(path_obj, "toString", ...)`
   re-enters this same native, because dispatch was made receiver-aware in
   2026-07 and `Path.toString` is registered.
4. The `IN_TOSTRING_FALLBACK` thread-local — installed against a real
   `EXCEPTION_STACK_OVERFLOW` — breaks the cycle by returning `String::new()`.
5. Every downstream file-IO native receives an **empty path**. That is the
   empty-message `NoSuchFileException: ` W7-97 measured, and why `Files.exists`
   answers false for a path whose `getNameCount()` is a correct 5.

The guard in step 4 is right and stays. The defect is upstream: nothing should
have been handing these natives a foreign `Path`, because nothing should have
been handing out a second `FileSystem`.

## 6. The fix as applied

`native-builtins/src/phases_late/nio_file.rs`, inside
`register_phase57_nio_file`, immediately after the `FileSystems.getDefault()`
registration. That function's `r.set_category(NativeKind::Bridge)` is what
keeps the registration alive under strict mode:

```rust
r.register(
    "sun/nio/fs/DefaultFileSystemProvider",
    "theFileSystem",
    "()Ljava/nio/file/FileSystem;",
    |ctx, _args| {
        let fs = p57_default_filesystem_singleton(ctx)?;
        Ok(Some(Value::Object(Some(fs))))
    },
);
```

**Descriptor read off the image, not recalled.**
`javap -p --module java.base sun.nio.fs.DefaultFileSystemProvider`, Temurin
25.0.3.9 windows/x64:

```
public class sun.nio.fs.DefaultFileSystemProvider {
  private static final sun.nio.fs.WindowsFileSystemProvider INSTANCE;
  private sun.nio.fs.DefaultFileSystemProvider();
  public static sun.nio.fs.WindowsFileSystemProvider instance();
  public static java.nio.file.FileSystem theFileSystem();
  static {};
}
```

`theFileSystem` returns the **interface** type on every platform, so
`()Ljava/nio/file/FileSystem;` is portable. It is `static`, so `args` carries
no receiver.

A second fix of the identical shape, one level down, went in with it —
`FileSystemProvider.getFileSystem(URI)` was answering
`p57_alloc_default_filesystem` (a *fresh* filesystem) for the `file` scheme,
and now answers `p57_default_filesystem_singleton`. That call is exactly how
`FileSystems$DefaultFileSystemHolder.getDefaultFileSystem()` reaches the
default FS (`provider.getFileSystem(URI.create("file:///"))`), so it was a
third door onto the same singleton.

### Predicted effect

| row | before (W7-97, measured) | after (**PREDICTED**) |
|---|---|---|
| `theFileSystem==getDefault` | false | **true** |
| `getFileSystem(file:///)==getDefault` | false | **true** |
| `theFsPath.equals(defPath)` | false | **true** |
| `Files.exists(theFsPath)` | false | **true** |
| `readAttributes(theFsPath)` | `NoSuchFileException: ` | **144908395** |
| `String.valueOf(theFsPath).length` | 0 | **59** |
| every `getDefault` row | already correct | **unchanged** |

## 7. Does this change `--real-jdk`? YES — it is not strict-mode-only

`native-api/src/registry.rs`:

```rust
pub fn allowed_in(self, mode: CompatibilityMode) -> bool {
    match mode {
        CompatibilityMode::Compatible => true,
        CompatibilityMode::JdkOnly => !matches!(self, NativeKind::SyntheticStub),
    }
}
```

`Bridge` is allowed in **both** modes; `SyntheticStub` is the only kind
`JdkOnly` rejects. So:

* The registration is live in `--real-jdk` as well as `--jdk-only`.
* More importantly, **the defect was already live in `--real-jdk`**.
  `FileSystems.getDefault()` is a `Bridge` in the same function, so the default
  mode has always returned the synthetic singleton from door 1 while door 2 ran
  real bytecode and built a `WindowsFileSystem`. The two-filesystem split is
  not a strict-mode artefact; strict mode is only where it was *noticed*.

Report this as a **both-modes** change. A reviewer who assumes strict-only will
under-test it.

The concrete `--real-jdk` prediction worth running first is
`java.util.zip.ZipFile`: if `ZipFile$Source.builtInFS` is reached at all in
that mode, `Files.readAttributes` over its path should have been failing with
an empty-message `NoSuchFileException` before this change. If it was *not*
failing, that tells you CratonVM's own zip natives are shadowing
`ZipFile$Source` entirely — worth knowing either way, and cheap to check.

## 8. Residuals

1. **`String.valueOf` / concat asymmetry** (W7-97 nomination 2) is untouched.
   §5 explains the `""` via the re-entrancy guard but **not** why a direct
   `toString()` escapes it while the concat path does not. If this fix makes
   the symptom vanish for this receiver, that is the trigger being removed, not
   the dispatch question being answered — do not close it on that evidence.
2. **`initPhase2` is still skipped** in `vm-cli/src/main.rs`. This removes the
   stated blocker; turning the call on is a separate decision with its own
   measurement.
3. **`java/lang/ModuleLayer.boot` is still registered twice** (W7-97
   nomination 3).
4. The provider half is a separate record: **W8-C14-2**.
