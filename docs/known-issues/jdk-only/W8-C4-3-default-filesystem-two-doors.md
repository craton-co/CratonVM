# W8-C4-3 — the default `FileSystem` has two doors and only one of them is wired

> **STATUS: NOMINATION.** `native-builtins/src/phases_late/nio_file.rs` is not
> this lane's file. The defect is located exactly — it is a **missing native**,
> not a fabricated receiver and not a disagreement between two natives — and the
> singleton the missing native needs already exists with exactly two callers.
> The "after" column is **PREDICTED**: this lane may not run the CratonVM binary.

Lane C4, 2026-08-12. Oracle: Temurin `jdk-25.0.3.9-hotspot`, windows/x64.
Probe: `scratchpad/c4/BootFsIdentity.java`.
Predecessor: `W7-97-initphase2-skipped.md` nomination 1, which measured the
symptom, named no owner beyond "the `sun.nio.fs` natives", and has not been
taken.

---

## 1. The invariant, and the oracle

Two invariants, and a third that only boot-loader-defined JDK classes exercise:

1. `FileSystems.getDefault()` returns the **same object** every call.
2. `Paths.get("x").getFileSystem() == FileSystems.getDefault()`.
3. `sun.nio.fs.DefaultFileSystemProvider.theFileSystem()` returns **that same
   object**.

Door 3 is not a curiosity. `jdk.internal.jimage.ImageReaderFactory`'s static
initialiser is, verbatim from JDK 25 `src.zip`:

```java
if (ImageReaderFactory.class.getClassLoader() == null) {
    fs = (FileSystem) Class.forName("sun.nio.fs.DefaultFileSystemProvider")
            .getMethod("theFileSystem").invoke(null);
} else {
    fs = FileSystems.getDefault();
}
BOOT_MODULES_JIMAGE = fs.getPath(JAVA_HOME, "lib", "modules");
```

`ImageReaderFactory` is boot-loader-defined, so it takes the **first** branch —
always. Any boot JDK class that needs a `Path` before the file system service
is up does the same.

`java --add-opens java.base/sun.nio.fs=ALL-UNNAMED -cp out BootFsIdentity`,
verbatim:

```text
getDefault==getDefault|true
Paths.get(x).getFileSystem()==getDefault|true
Path.of(x).getFileSystem()==getDefault|true
good.getPath(x).getFileSystem()==getDefault|true
theFileSystem.class|sun.nio.fs.WindowsFileSystem
getDefault.class|sun.nio.fs.WindowsFileSystem
theFileSystem==getDefault|true
theFileSystem.equals(getDefault)|true
theFileSystem.provider()==getDefault.provider()|true
bad.toString().length|59
String.valueOf(bad).length|59
concat(bad).length|59
bad.equals(good)|true
good.equals(bad)|true
bad.getNameCount|5
bad.getFileSystem()==theFileSystem|true
Files.exists(bad)|true
Files.exists(good)|true
readAttributes(bad).size|144908395
readAttributes(good).size|144908395
```

HotSpot has **one** `WindowsFileSystem`, and it is the same object through both
doors, because `WindowsFileSystemProvider` is a singleton
(`DefaultFileSystemProvider.INSTANCE`) holding one `theFileSystem` field, and
`FileSystems.getDefault()` reaches it via that same singleton's
`getFileSystem(URI)`.

W7-97's measured CratonVM `--jdk-only` column: `theFileSystem==getDefault`
**false**, `Files.exists(bad)` **false**, `readAttributes(bad)`
**`NoSuchFileException: `** with an EMPTY message, `bad.equals(good)` **false**,
`String.valueOf(bad)` **`""`** while `bad.toString()` is a correct 59
characters. `ImageReader.open(Path.of(home,"lib","modules"))` from user code
answers 70 module names correctly — the jimage machinery is fine — and
`readAttributes` over a `getDefault()` path answers the exact 144908395 bytes.
So both halves work; only the *object that minted the path* is wrong.

## 2. The actual failure, named

Not a fabricated receiver. Not a wrong singleton identity between two natives.
**One door is native and the other has no native at all.**

* **Door 1 is native and singleton-correct.**
  `native-builtins/src/phases_late/nio_file.rs:787` registers
  `java/nio/file/FileSystems.getDefault()Ljava/nio/file/FileSystem;`, which
  calls `p57_default_filesystem_singleton` (line 11470). That helper stashes
  the object in the **real** `FileSystems$DefaultFileSystemHolder.
  defaultFileSystem` static — static storage is a GC root, so the reference
  survives moving collections — and hands the same object back forever. Its doc
  comment records why: JUnit's `File`-typed `@TempDir` and cassandra's
  `File(Path)` compare `path.getFileSystem()` against `FileSystems.getDefault()`
  with `==`, and a fresh allocation per call broke every one of them. The object
  is a 3-field synthetic stamped with the **abstract** `java/nio/file/FileSystem`
  class, whose `getClass()` is aliased to report `sun.nio.fs.WindowsFileSystem`
  (`native-builtins/src/lib.rs`, `jdk_concrete_getclass_alias`) — which is why
  W7-97 saw both objects "report class `sun.nio.fs.WindowsFileSystem`" and the
  alias hid the split.
* **Door 2 has no native.** `grep -rn "theFileSystem" --include=*.rs` over the
  whole tree returns exactly **one** hit, a comment in `vm-cli/src/main.rs`.
  `git log -S'theFileSystem' -- native-builtins/src/phases_late/nio_file.rs` is
  empty: it has never been registered. Under `--jdk-only` the real JDK bytecode
  therefore runs — `DefaultFileSystemProvider.<clinit>` →
  `new WindowsFileSystemProvider()` → `new WindowsFileSystem(this, userDir)` —
  producing a genuine real-bytecode `WindowsFileSystem` that the entire `p57_*`
  native layer has never heard of.

**Why its `Path`s are inert follows mechanically.** `p57_read_path`
(line 7460) reads the path string from `P57_PATH_FIELD` (= field 0). A real
`sun.nio.fs.WindowsPath`'s field 0 is a `WindowsFileSystem` **object**, so
`read_string` correctly refuses and the fast path fails. The documented
fallback is a virtual `toString()` — but dispatch was made receiver-aware in
2026-07, so *any* `Path`-subtype receiver's `toString()` routes straight back
into this same native, and the thread-local `IN_TOSTRING_FALLBACK` guard
(installed against a real `EXCEPTION_STACK_OVERFLOW`) then returns
**`String::new()`**. Every file-IO native downstream is handed an empty path.
That is precisely the empty `NoSuchFileException: ` W7-97 measured, and it is
why `Files.exists` answers false for a path whose `getNameCount()` is a correct
5.

The guard is right and should stay. The bug is upstream of it: nothing should
be handing these natives a foreign `Path` in the first place, because nothing
should be handing out a second `FileSystem`.

## 3. NOMINATION 1 — register `theFileSystem` on the existing singleton

`native-builtins/src/phases_late/nio_file.rs`, inside
`pub fn register_phase57_nio_file(r: &mut NativeMethodRegistry)`. That function
sets `r.set_category(NativeKind::Bridge)` at line 94, so a registration placed
inside it is a **Bridge** and `--jdk-only` keeps it — which is the whole point,
since strict mode is the mode where the defect bites.

`p57_default_filesystem_singleton` is the helper that already exists, with two
callers today (`Path.getFileSystem` and `FileSystems.getDefault`). This adds
the third, which is the one that was missing.

OLD (exact, at line 783-796):

```rust
    // --- FileSystems.getDefault() → FileSystem ---
    let file_systems = "java/nio/file/FileSystems";
    r.register(
        file_systems,
        "getDefault",
        "()Ljava/nio/file/FileSystem;",
        |ctx, _args| {
            let fs = p57_default_filesystem_singleton(ctx)?;
            Ok(Some(Value::Object(Some(fs))))
        },
    );
```

NEW:

```rust
    // --- FileSystems.getDefault() → FileSystem ---
    let file_systems = "java/nio/file/FileSystems";
    r.register(
        file_systems,
        "getDefault",
        "()Ljava/nio/file/FileSystem;",
        |ctx, _args| {
            let fs = p57_default_filesystem_singleton(ctx)?;
            Ok(Some(Value::Object(Some(fs))))
        },
    );
    // --- sun.nio.fs.DefaultFileSystemProvider.theFileSystem() → FileSystem ---
    //
    // The SECOND door to the same singleton, and the one boot-loader-defined
    // JDK classes use. `jdk.internal.jimage.ImageReaderFactory`'s <clinit>
    // takes it unconditionally (`if (getClassLoader() == null)`), so the
    // runtime-image path `<java.home>/lib/modules` is built by this call and
    // not by `FileSystems.getDefault()`.
    //
    // Unregistered, `--jdk-only` ran the real `DefaultFileSystemProvider`
    // bytecode, which constructs its OWN `WindowsFileSystem`. Every `Path` that
    // second filesystem mints is a real-bytecode `WindowsPath` whose field 0 is
    // a FileSystem object, not a String, so `p57_read_path`'s fast path fails,
    // its `toString()` fallback re-enters this same native, the
    // `IN_TOSTRING_FALLBACK` guard returns "", and the file-IO natives are
    // handed an EMPTY path: `Files.exists` false, `readAttributes` throwing
    // `NoSuchFileException` with an empty message — measured, and that empty
    // message is what kills `System.initPhase2` with JNI_ERR.
    //
    // HotSpot 25 returns one shared instance through both doors (measured,
    // `theFileSystem==getDefault|true`), because both reach the single
    // `WindowsFileSystemProvider.INSTANCE`. Returning the singleton here is
    // that identity, not an approximation of it.
    // docs/known-issues/jdk-only/W8-C4-3-default-filesystem-two-doors.md
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

**Descriptor, verified against the image rather than recalled** —
`javap -p --module java.base sun.nio.fs.DefaultFileSystemProvider` on Temurin
25.0.3.9/windows:

```text
public class sun.nio.fs.DefaultFileSystemProvider {
  private static final sun.nio.fs.WindowsFileSystemProvider INSTANCE;
  private sun.nio.fs.DefaultFileSystemProvider();
  public static sun.nio.fs.WindowsFileSystemProvider instance();
  public static java.nio.file.FileSystem theFileSystem();
  static {};
}
```

`theFileSystem` returns the **interface type** `java.nio.file.FileSystem` on
every platform, so `()Ljava/nio/file/FileSystem;` is portable. It is `static`,
so `args` carries no receiver — hence `_args`.

**`instance()` is NOT nominated with it, deliberately.** Its return type is
platform-specific (`Lsun/nio/fs/WindowsFileSystemProvider;` here,
`Lsun/nio/fs/LinuxFileSystemProvider;` on the Linux image), so one registration
cannot serve both legs, and nothing measured needs it: `ImageReaderFactory`
calls `theFileSystem()`. If a caller for `instance()` turns up, the honest
shape is a `cfg!(windows)`-selected descriptor returning
`FileSystems.getDefault().provider()`, and it needs its own measurement.

### Predicted effect

| `BootFsIdentity` row | before (W7-97, measured) | after (PREDICTED) |
|---|---|---|
| `theFileSystem==getDefault` | false | **true** |
| `bad.equals(good)` | false | **true** |
| `Files.exists(bad)` | false | **true** |
| `readAttributes(bad)` | `NoSuchFileException: ` | **144908395** |
| `String.valueOf(bad).length` | 0 | **59** |
| every `getDefault` row | already correct | **unchanged** |

## 4. Verification (PREDICTED — not run here)

```
javac -d out BootFsIdentity.java                      # scratchpad/c4
java --add-opens java.base/sun.nio.fs=ALL-UNNAMED -cp out BootFsIdentity
cratonvm.exe --java-home $JAVA_HOME --jdk-only \
    --add-opens java.base/sun.nio.fs=ALL-UNNAMED -cp out BootFsIdentity
cratonvm.exe --java-home $JAVA_HOME --real-jdk \
    --add-opens java.base/sun.nio.fs=ALL-UNNAMED -cp out BootFsIdentity
```

Every line is `KEY|value`; diff the arms line for line. `--add-opens` is
required — `sun.nio.fs` is not exported from `java.base`, and without it
`setAccessible(true)` throws `InaccessibleObjectException` on HotSpot too. If
the CratonVM arm prints `theFileSystem|UNAVAILABLE …`, that is the probe's own
reach, not a result: fix the opens before reading anything into it
(`W7-…-add-opens-was-parsed-then-ignored` is the precedent).

**Then re-test what this actually unblocks**, which is the point of the change:
invoke `System.initPhase2(false, false)` reflectively under `--jdk-only` and
read its return code. W7-97 §3.1 disarms the trap waiting there — pass
`printStackTrace = false`, because printing a stack trace walks module metadata
and materialises the boot layer as a side effect, which looks exactly like
"running initPhase2 fixed it" and is not.

## 5. Residuals this does NOT close

1. **The `String.valueOf` / concat asymmetry** (W7-97 nomination 2): an object
   whose own `toString()` answers 59 correct characters renders as `""` under
   `String.valueOf` and string concatenation, in the same expression. §2 gives
   a mechanism that explains the `""` — the re-entrancy guard — but **not** why
   a direct `toString()` escapes it while the concat path does not. That gap is
   unresolved here and stays a separate nomination. If nomination 1 lands, the
   symptom disappears for this receiver without the underlying dispatch
   question being answered; do not read its disappearance as a fix.
2. **`initPhase2` remains skipped** in `vm-cli/src/main.rs`. This removes the
   stated blocker; it does not turn the call on, and turning it on is a separate
   decision with its own measurement (`SystemModuleFinders.ofSystem()` took the
   slow `ofModuleInfos()` fallback, so a repaired `initPhase2` may parse 70
   `module-info.class` files on every boot).
3. **`java/lang/ModuleLayer.boot` is still registered twice** (W7-97 nomination
   3), with the working one winning by last-writer-wins registration order.
   Untouched here.
4. **No suite fixture is shipped for this.** The invariants that need no flags
   (`getDefault` is a singleton; `Paths.get("x").getFileSystem() ==
   getDefault()`) are already green in CratonVM by construction — the
   `p57_default_filesystem_singleton` stash exists precisely because they once
   were not — so a fixture asserting only those would be a ratchet, not a
   witness. The witnessing half needs `--add-opens java.base/sun.nio.fs=
   ALL-UNNAMED` on the runner, and `regression-suite/run.sh` is not this lane's
   file. If a taker wants it scheduled, the fixture is `BootFsIdentity.java`
   as written and the runner needs that one flag.
