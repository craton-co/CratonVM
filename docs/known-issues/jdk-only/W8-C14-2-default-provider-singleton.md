# W8-C14-2 — the default `FileSystemProvider` was never a singleton at all

> **STATUS: FIX APPLIED** in `native-builtins/src/phases_late/nio_file.rs`.
> HotSpot column measured; CratonVM "after" values **PREDICTED**.
>
> This record **overturns** W8-C4-3's decision to defer `instance()`. That
> lane's stated reason — the return type is platform-specific — is correct and
> is handled below. Its unstated assumption, that deferring is *neutral*, is
> not: `instance()` compiles to a bare `getstatic` whose class-init
> reconstructs the very second file system W8-C14-1 exists to eliminate.

Lane C14, 2026-08-12. Oracle: Microsoft OpenJDK `25.0.3+9`, windows/x64.

---

## 1. Three doors, one object — and CratonVM had none of them

HotSpot creates `WindowsFileSystemProvider` exactly once, in
`DefaultFileSystemProvider.<clinit>`. Measured
(`scratchpad/c14/FsIdentity.java`):

```
provider==provider|true
getDefault.provider==instance|true
theFileSystem.provider==getDefault.provider|true
installedProviders.get0==getDefault.provider|true
installedProviders.get0 stable|true
```

CratonVM before this change:

| door | registration | behaviour |
|---|---|---|
| `FileSystem.provider()` | `nio_file.rs` | **fresh object every call** |
| `FileSystemProvider.installedProviders()` | `nio_file.rs` | **three fresh objects every call** |
| `DefaultFileSystemProvider.instance()` | *none* | real bytecode → real `WindowsFileSystemProvider` |

So `fs.provider() == fs.provider()` — a FileSystem failing to agree with
**itself** — was false. This is not the "two natives disagree" shape and not
the "one native missing" shape of W8-C14-1; it is *no singleton anywhere*.

## 2. Why `instance()` could not be deferred

`javap -c -p --module java.base sun.nio.fs.DefaultFileSystemProvider`:

```
public static sun.nio.fs.WindowsFileSystemProvider instance();
  Code:
     0: getstatic     #7    // Field INSTANCE:Lsun/nio/fs/WindowsFileSystemProvider;
     3: areturn

static {};
  Code:
     0: new           #14   // class sun/nio/fs/WindowsFileSystemProvider
     3: dup
     4: invokespecial #19   // Method sun/nio/fs/WindowsFileSystemProvider."<init>":()V
     7: putstatic     #7
    10: return
```

and `WindowsFileSystemProvider.<init>` (src.zip) is:

```java
theFileSystem = new WindowsFileSystem(this, StaticProperty.userDir());
```

An un-intercepted `instance()` is a `getstatic` that triggers `<clinit>`, and
`<clinit>` **constructs a second `WindowsFileSystem`**. Leaving it out is not a
smaller version of the fix; it re-opens the hole W8-C14-1 closes. Its callers:

* `java.nio.file.FileSystems$DefaultFileSystemHolder.getDefaultProvider()` —
  `FileSystemProvider provider = DefaultFileSystemProvider.instance();`
* `sun.nio.ch.UnixDomainSockets.generateTempName()`, verbatim:

```java
final Path path = Path.of(dir, "socket_" + rnd);
if (path.getFileSystem().provider() != sun.nio.fs.DefaultFileSystemProvider.instance()) {
    throw new UnsupportedOperationException(
            "Unix Domain Sockets not supported on non-default file system");
}
```

That is a raw `!=` between door 1 and door 3, inside `java.base`, that throws
when it fails. It is also the reason the three doors had to move **together**:
registering `instance()` while `provider()` still minted per-call objects would
have left this comparison exactly as broken as before — a half-closed fix that
looks addressed in the changelog and is not.

## 3. The fix as applied

A provider is now cached **on the FileSystem object that owns it**, in a new
slot:

```rust
pub(crate) const P57_FS_PROVIDER_FIELD: usize = 3;
pub(crate) const P57_FS_SLOTS: usize = 4;
```

Why on the object rather than in a Rust-side cache: a `OnceLock<ObjectRef>`
would be process-global (surviving across in-process VMs) and invisible to the
GC as a root. The default FileSystem is already GC-rooted through the
`FileSystems$DefaultFileSystemHolder.defaultFileSystem` static that
`p57_default_filesystem_singleton` writes, so hanging the provider off it
inherits correct rooting **and** correct VM scoping for free.

`P57_FS_SLOTS` is named rather than spelled `4` at each site because a
synthetic class's width being declared in several places is a recorded defect
family here; the two allocation sites now both read the constant.

New helpers: `p57_fs_scheme`, `p57_alloc_provider`, `p57_fs_provider`,
`p57_default_provider_singleton`. Rewired: `FileSystem.provider()`,
`FileSystemProvider.installedProviders()` element 0, and the new
`DefaultFileSystemProvider.instance()` registration.

### The platform-specific descriptor

Unlike `theFileSystem`, `instance()` returns the **concrete** provider type:

```rust
let instance_desc: &'static str = if cfg!(windows) {
    "()Lsun/nio/fs/WindowsFileSystemProvider;"
} else if cfg!(target_os = "macos") {
    "()Lsun/nio/fs/MacOSXFileSystemProvider;"
} else {
    "()Lsun/nio/fs/LinuxFileSystemProvider;"
};
```

The Windows arm is **verified by javap** on the image in use. The other two are
**UNVERIFIED** — this lane has only a Windows JDK. That is an acceptable risk
only because of the failure mode: a wrong descriptor produces a registration
that never matches (a dead registration, the `W7-88` shape), which is exactly
today's behaviour — not a crash and not a wrong answer. Guessing the descriptor
is survivable; guessing the returned **value** would not be. **Nomination: run
`javap -p --module java.base sun.nio.fs.DefaultFileSystemProvider` on the Linux
image and confirm the `Linux` arm before relying on it there.**

## 4. Two bugs found while writing the fix

**4.1 — A latent out-of-bounds read that the fix would have introduced.**

Caching in slot 3 is only safe on *our* 4-slot synthetic. A real
`sun.nio.fs.WindowsFileSystem` has exactly **three** instance fields (`javap`:
`provider`, `defaultDirectory`, `defaultRoot`), so slot 3 is one past its end —
and dispatch has been receiver-aware since 2026-07, so a real FileSystem
receiver's `provider()` call routes into this native. An unguarded read would
have been an out-of-bounds field read on a real JDK class, which is a genuine
defect in this VM rather than a cosmetic one.

`p57_fs_provider` therefore opens with a hard width guard
(`ctx.object_num_fields(fs) > P57_FS_PROVIDER_FIELD`) and answers an undersized
receiver with the default provider singleton — which is also the *correct*
answer, since the only real FileSystem that can reach it is the platform
default one. The recursion terminates because the default singleton is always
allocated at `P57_FS_SLOTS`.

**4.2 — A pre-existing wrong answer in the code being replaced.** The old
`provider()` sniffed the scheme by reading slots 2 then 1 for jar/jrt markers:

```rust
Ok(this) if matches!(ctx.get_field(this, P57_FS_JRT_FIELD), Value::Object(Some(_))) => "jrt",
Ok(this) if matches!(ctx.get_field(this, P57_FS_JAR_FIELD), Value::Object(Some(_))) => "jar",
_ => "file",
```

On a **real** `WindowsFileSystem` receiver those slots are `defaultDirectory`
and `defaultRoot` — both non-null Strings. The sniff therefore answered
**`"jrt"` for the platform's own file system**. Slots 1 and 2 are in bounds on
a 3-field object, so this was a silent wrong answer, not a diagnostic. It is
fixed by construction: the sniff now runs only inside the width-guarded branch,
and undersized receivers get the `"file"` singleton. The fixture pins it
(`prov.scheme`).

## 5. Predicted effect

| row | before (**PREDICTED** red) | after (**PREDICTED**) |
|---|---|---|
| `prov.stable` (`fs.provider()==fs.provider()`) | false | **true** |
| `prov.installedIsDefault` | false | **true** |
| `prov.installedStable` | false | **true** |
| `prov.viaPath` | false | **true** |
| `prov.scheme` on a real receiver | `jrt` | **`file`** |
| `getDefault.provider==instance` (needs `--add-opens`) | false | **true** |
| `UnixDomainSockets.generateTempName()` | `UnsupportedOperationException` | **succeeds** |

The "before" column is marked PREDICTED, not measured: this lane cannot run the
VM. It is read directly off the replaced source (a `try_alloc_concurrent_
synthetic` per call cannot return a stable identity), which is a strong
inference but still an inference.

## 6. Residuals — deliberately not fixed here

1. **jar/jrt providers still mint per call.** `installedProviders()` elements 1
   and 2, and `provider()` on a mounted jar/jrt FileSystem, still allocate.
   HotSpot has one `ZipFileSystemProvider` and one `JrtFileSystemProvider`.
   Fixing it needs a rooted stash for non-default providers, which the
   FileSystem-slot trick does not supply (each jar FS is itself freshly
   allocated — see residual 3). No in-tree witness demands it yet.
2. **The `installedProviders()` List object is still fresh per call.** HotSpot
   caches the list itself in a static, so `installedProviders() ==
   installedProviders()` is true there and false here. The fixture asserts
   *element* identity, which is what the javadoc contract and every known
   caller actually depend on.
3. **Jar/zip mounts have no registry at all** — `p57_alloc_jar_filesystem` has
   no cache keyed by jar path, so two `newFileSystem` calls on the same jar
   yield two file systems, and `FileSystemAlreadyExistsException` appears
   **nowhere in the repo** (grep across all `*.rs`: zero hits). HotSpot throws
   it on the second call and `FileSystems.getFileSystem(jarURI)` afterwards
   returns that same object. This is the largest remaining member of the
   family; see W8-C14-3 nomination 2.
