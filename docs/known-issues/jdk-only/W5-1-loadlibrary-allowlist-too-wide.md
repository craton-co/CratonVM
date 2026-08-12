# W5-1 — the `System.loadLibrary` allowlist was too wide

Status: fixed. Its named residual — no loader-scoped `loadedLibraryNames` — is
fixed too, **in strict mode only**, 2026-08-11; see "The residual, closed"
below. Nothing has been built or run for that change.

Measured on 2026-08-07 on JDK 25.0.3 Windows x64, HotSpot as the oracle. The
JDK path in the original text was `C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot`;
on this host the same build now lives at
`C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot`, which is what the
2026-08-11 `javap` and `lib/src.zip` readings below used.

## The divergence

One character of `CK` output, and `run.sh` compares `CK`/`PASS` lines, not exit
codes — `RJdkJni` exited 0 and still failed the suite:

```
HotSpot :  CK RJdkJni loadedLibrary=net mapped=foo.dll
CratonVM:  CK RJdkJni loadedLibrary=zip mapped=foo.dll
```

`RJdkJni.java:189-202` tries `System.loadLibrary("zip")` first and falls through
to a `net` probe only if `zip` throws `UnsatisfiedLinkError`. HotSpot takes the
fallback. CratonVM did not, because W2-7's `is_vm_provided_jdk_library`
allowlist answered "success" for `zip`.

## The predicted cause was wrong; the prediction was right

Wave 2 predicted the failure correctly but attributed it to static linking —
"`zip` is folded into `libjava` on this image". It is not. `zip.dll` is a real,
separate file in `<java.home>/bin`, and a *cold* `System.loadLibrary("zip")`
loads it fine. The actual rule is dynamic and has nothing to do with linkage:

`jdk.internal.loader.NativeLibraries` refuses to load the same library FILE into
two different class loaders. `java.base` boot-loads `zip.dll` itself —
`Inflater.<clinit>` -> `ZipUtils.loadLibrary()` ->
`BootLoader.loadLibrary("zip")` — so once anything has touched `java.util.zip`,
an app-class-loader `System.loadLibrary("zip")` throws
`UnsatisfiedLinkError: Native Library …\zip.dll already loaded in another classloader`.

Measured, same JVM image, three states:

| state | `loadLibrary("zip")` |
|---|---|
| cold, classpath is a directory | LOADS |
| after any `java.util.zip` native use | THROWS (already loaded in another classloader) |
| cold, but classpath is a `.jar` | THROWS (already loaded in another classloader) |

The rule is per-library-file and symmetric — pre-touching `java.net` makes
`net` *and* `nio` throw; pre-touching `java.util.prefs` makes `prefs` throw.

`RJdkJni` puts itself in the second state deliberately: `main` calls
`zipNatives()` immediately before `libraryLoading()`. That is why the oracle
says `net`, and it is a *test-produced* state rather than a file-layout fact,
so it holds identically on Linux.

## The measurement

`System.loadLibrary(x)` from the app class loader, nothing pre-loaded,
JDK 25.0.3 Windows x64:

```
java             LOADS
zip              LOADS
net              LOADS
nio              LOADS
jimage           LOADS
verify           LOADS
management       LOADS
management_ext   LOADS
instrument       LOADS
extnet           LOADS
prefs            LOADS
j2pkcs11         LOADS
sunec            THROWS UnsatisfiedLinkError: no sunec in java.library.path
sunmscapi        LOADS
jsig             THROWS UnsatisfiedLinkError: no jsig in java.library.path
jvm              THROWS UnsatisfiedLinkError: no jvm in java.library.path
```

Corroborated by the image contents: `<java.home>/bin` has no `sunec.dll`, no
`jsig.dll`, no `jvm.dll` (the last lives in `bin/server`, which is on neither
`java.library.path` nor `sun.boot.library.path`).

## Why the allowlist is the lever at all

Because the real load always fails first. `LoadLibraryW` on the JDK's own DLLs
from a non-JVM process returns `ERROR_MOD_NOT_FOUND` (126) — they are linked
against `jvm.dll`, which CratonVM's process does not have:

```
java, zip, net, nio, jimage, verify, management, management_ext,
instrument, extnet, prefs        LoadLibrary FAILED err=126
j2pkcs11, sunmscapi              LoadLibrary OK   (self-contained)
```

So `load_library_or_throw` reaches the allowlist for every interesting name,
even though `<java.home>\bin` is on this host's `PATH` and therefore on
CratonVM's `java.library.path`.

## Before / after

Removed: `zip`, `sunec`, `jvm` (all platforms) and `jsig` (Windows only).
Kept, now split by platform: `sunmscapi` on Windows, `jsig` elsewhere.

## The residual, closed — 2026-08-11

### It was still live

Checked before implementing, because this campaign has fifteen records claiming
a hand-off was never applied for a change that is in the tree today. This one
was not one of them. On dev `95b693f2d`, `loadedLibraryNames` appeared in
**five** places in the source tree and every one of them was a comment saying
the bookkeeping does not exist: this record, `W6-6`, the campaign README, and
two comments in `native-builtins/src/lib.rs` explaining that the JDK's own
bytecode holds the set on the *other* road. No table, no loader key, no call
site. Live.

### What the JDK actually specifies

`jdk.internal.loader.NativeLibraries`, JDK 25.0.3 `lib/src.zip`. The class doc
on `newInstance(ClassLoader)` states the restriction as a numbered guarantee of
the type:

> 3. Restriction on a native library that can only be loaded by one class
>    loader. Each class loader manages its own set of native libraries. The
>    same JNI native library cannot be loaded into more than one class loader.

and `loadLibrary(Class,String,boolean)` enforces it in two steps, in this order,
inside `acquireNativeLibraryLock(name)`:

```java
// find if this library has already been loaded and registered in this NativeLibraries
NativeLibrary cached = libraries.get(name);
if (cached != null) {
    return cached;
}

// cannot be loaded by other class loaders
if (loadedLibraryNames.contains(name)) {
    throw new UnsatisfiedLinkError("Native Library " + name +
            " already loaded in another classloader");
}
```

The two structures are **not the same shape**, and that is the whole point:

| structure | scope | on a hit |
|---|---|---|
| `libraries` — `Map<String,NativeLibraryImpl>` | **instance** field of the `NativeLibraries` that `ClassLoader` holds one of per loader (`private final NativeLibraries libraries = NativeLibraries.newInstance(this)`) | returns the SAME library — silent success |
| `loadedLibraryNames` — `Set<String>` | `static` | `UnsatisfiedLinkError` |

`ClassLoader.nativeLibrariesFor(loader)` is the accessor, with
`BootLoader.getNativeLibraries()` standing in for the null loader; and
`loadLibrary(Class,String,boolean)` opens with
`if (this.loader != loader) throw new InternalError(...)`, so an instance is
bound to one loader by assertion. A single process-wide set of names cannot
answer either question: it keys on a string and carries no loader identity, so
it cannot separate "this loader already has it" (success) from "somebody else
does" (error).

The key itself is the library **file**, not the bare name:
`NativeLibraries.loadLibrary(Class,File)` sets `name = file.getCanonicalPath()`
for anything not statically linked into libjvm, and every bare-name road ends
in that overload. That is why the dynamic rule in this record's own tables is
per-library-file and symmetric.

### How it is scoped

`native-builtins/src/lang_system.rs`:

* `LOADED_LIBRARIES`, a `VmScoped<BTreeMap<i32, BTreeSet<String>>>` — loader id
  to library keys, one row per VM. **Both** JDK queries come off one table:
  "does this loader hold it" is a row lookup, "does any loader hold it" is the
  union over rows. `claim_library` does the lookup and the insert in a single
  table acquisition, which is the window HotSpot closes with
  `acquireNativeLibraryLock(name)`.
* **Was it a process global before? There was nothing before** — no state at
  all. What is added is not one either: `VmScoped` is the tree's own mechanism
  for a native side-table partitioned by `vm_identity`, already used two
  hundred lines further down this same file for `SYSTEM_ENV`/`SYSTEM_PROPS`,
  and contract §2's ban is what it exists to satisfy. Torn down from
  `forget_vm_system_singletons`, which `release_vm_native_state` already calls.
  (The pre-existing `SHUTDOWN_HOOKS` in this file *is* an unpartitioned
  `static Mutex<Vec<usize>>`. Not touched, not this lane's, named here only so
  the next reader does not take it as precedent.)
* Keyed on `NativeContext::loader_id_of_class` — `0` bootstrap, `1` platform,
  `2` application, `3+` user-defined — **never on the loader's name**. Two
  loaders can share a name; that is the species.
* The loader is resolved by `requesting_loader_id`, from the JDK's own
  `fromClass` argument where one exists (`Runtime.load0`/`loadLibrary0`, which
  real `Runtime` bytecode fills from `Reflection.getCallerClass()` and which
  `ClassLoader.loadLibrary` itself turns into a loader with
  `fromClass.getClassLoader()`), and otherwise from `frame_class_ids()` —
  each live frame's already-resolved `ClassId`, not a name re-resolution of a
  captured stack trace, which would collapse two same-named classes from
  different loaders onto whichever the class table saw first.
* No `ObjectRef` is stored, so no GC root scan and no post-collection remap are
  needed. Holding the loader *mirror* would have needed both, and would have
  keyed loader identity on an address that moves.

### Where it does not know, and does not guess

`requesting_loader_id` answers `None` when neither source names a class — a load
with no Java frame under it. `loaded_by` then records nothing and throws
nothing. Recording under a stand-in loader id would make the *next* load, the
one from the real owner, throw: the error would be manufactured, just later and
further from the cause. Throwing outright manufactures it immediately. Of the
two failure directions the missed error is the one that leaves a caller's own
`catch (UnsatisfiedLinkError)` fallback reachable.

### Mode

**Strict (`--jdk-only`) only.** `Compatible` is unchanged: the four triples are
registered once per mode and the `Compatible` arm keeps today's bodies with
`LoaderScoping::Off`, under which every success arm of `load_library_or_throw`
returns the `Ok(None)` it returns today. The gate is at registration because a
`NativeCallback` is a bare `fn` pointer with no captures and `NativeContext`
exposes no policy accessor — `CompatibilityMode` is a per-registry field on
purpose. `vm_init.rs` sets the mode before the `register_*` population pass and
says so structurally, so the gate is not vacuous. Each triple is registered
exactly once, not re-registered on top of itself, so no self-shadow row appears
in the duplicate-registration census.

### What still cannot fire, and the patch that would arm it

The record can only hold what something reports to it, and
`jdk/internal/loader/BootLoader.loadLibrary` is a deliberate no-op in
`native-builtins/src/lib.rs` — the short-circuit that stops real
`NativeLibraries` bytecode from blocking on the JDK's native-library lock during
Linux boot-class `<clinit>`. So the case this record opened with — `java.base`
boot-loads `net`, then the application asks for it — still reports success.

`lang_system::record_boot_loader_library` is written and **deliberately
unarmed**; its doc comment says so. Out-of-file patch, the closure body of that
registration (`BootLoader.loadLibrary(String)` is static, so `args[0]` is the
name):

```rust
    registry.register(
        "jdk/internal/loader/BootLoader",
        "loadLibrary",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            if let Some(Value::Object(Some(o))) = args.first() {
                let name = ctx.read_string(*o).unwrap_or_default();
                crate::lang_system::record_boot_loader_library(ctx, &name);
            }
            Ok(None)
        },
    );
```

Not applied here, and the reason is a measurement rather than caution: arming it
makes `System.loadLibrary("net")` throw for any program that has already reached
a `java.net` boot class. That is HotSpot's answer, and it is exactly what
`RJdkJni.libraryLoading` depends on NOT happening — the `zip` probe falls
through to a `net` probe that must succeed (`RJdkJni.java:189-202`) and `run.sh`
compares `CK` lines, so a spurious throw there fails the vector. Whether
CratonVM reaches `BootLoader.loadLibrary("net")` before that line is not
decidable from source. One A/B on a built binary, both modes, HotSpot oracle
beside it, settles it.

### Out-of-file: the campaign README's row for these two records

`docs/known-issues/jdk-only/README.md` §2.1 is not this lane's file. Its row for
`W5-1` / `W6-6` still reads "there is **no class-loader-scoped
`loadedLibraryNames` bookkeeping in this VM**, so the JDK's dynamic 'already
loaded elsewhere' rule cannot fire. A static allowlist cannot model it."
Replacement:

> Loader-scoped under `--jdk-only` since 2026-08-11 (`LOADED_LIBRARIES` in
> `lang_system.rs`); `Compatible` unchanged. What is left is the boot-loader
> case: `BootLoader.loadLibrary` is a no-op, so `java.base` taking `net`/`nio`/
> `prefs` for itself is never recorded. The patch that arms it is in `W5-1`,
> unapplied because it can flip `RJdkJni`'s `net` probe and that needs one A/B
> on a binary.

### Named residual: the key is the spelling, not the file

HotSpot canonicalises to a path, so `System.loadLibrary("zip")` and
`System.load("<java.home>/bin/zip.dll")` are one key there and two here. Not
rounded up: `NativeContext::load_native_library` returns a library-table index,
not the path it resolved, so a bare name found on `java.library.path` cannot be
canonicalised back to a file without redoing the search — and a key invented by
redoing it would not be the file that was actually opened. Closing this means
returning the resolved path from `load_native_library`, which is a `native-api`
change.

## Separately found: `Runtime.load0`/`loadLibrary0` read the wrong argument

Not a strict-mode defect and not part of the residual — found while reading for
the loader identity, and it is the same two lines.

`javap -p java.lang.Runtime`, JDK 25.0.3:

```
  public void load(java.lang.String);
  void load0(java.lang.Class<?>, java.lang.String);
  public void loadLibrary(java.lang.String);
  void loadLibrary0(java.lang.Class<?>, java.lang.String);
```

Both `load0` and `loadLibrary0` are **instance** methods, so a native body sees
`args[0]` = the `Runtime` receiver, `args[1]` = the `fromClass` mirror, `args[2]`
= the name. The convention is stated repeatedly for this same registry — the
`Runtime.addShutdownHook` registration forty lines above these two says
"args[0] = the `Runtime` receiver, args[1] = the hook `Thread`", and
`vm_exec.rs`'s native argument marshalling describes its inline buffer as
"receiver plus a couple of operands".

Both bodies read `args.get(1)` as the name. That is the `Class` mirror, and
`read_string` of a non-`String` is `None`
(`vm_object.rs::read_string_non_string_object`), so the name arrives empty and
`Runtime.getRuntime().loadLibrary(x)` fails for every `x` with
`no  in java.library.path` — note the double space, which is the whole
signature of the bug. A wrong answer with the *right shape*, which is why it
survived: the regression suite reaches library loading only through
`System.load`/`System.loadLibrary` (`RJdkJni.java:189-217`,
`RJdkFailure.java:257`), never through `Runtime`, so nothing guards it.

Corrected on the strict arm (`runtime_load_args`). **Not** on the `Compatible`
arm: it is mode-independent — nothing about it is a compatibility-layer
substitution — so repairing it under `Compatible` is a behaviour change that
belongs to whoever owns `Compatible`, not to a jdk-only lane.

In-file patch when that is adjudicated — in the `else` arm only, and keeping
`LoaderScoping::Off` so the loader rule stays strict-only, replace each of the
two `Runtime` bodies' first three lines with the one call the strict arm makes:

```rust
            |ctx, args| {
                let (_from_class, name) = runtime_load_args(&*ctx, args);
                crate::security_manager::check_host_native_access_or_throw(ctx, &name)?;
                load_library_or_throw(
                    ctx,
                    &name,
                    LibrarySpelling::BareName,     // `AbsolutePath` on `load0`
                    None,
                    LoaderScoping::Off,
                )
            },
```

Callers that were silently getting an `UnsatisfiedLinkError` for a library that
is present will start loading it.

## Blast radius

### Of the allowlist narrowing (2026-08-07)

`java.base` does not reach this code for its own bootstrap: `java.util.zip`
loads via `BootLoader.loadLibrary` -> `jdk/internal/loader/NativeLibraries.load`
(registered separately in `native-builtins/src/lib.rs`), not via
`System.loadLibrary`. Only user-level
`System.loadLibrary`/`System.load`/`Runtime.load0`/`Runtime.loadLibrary0` are
affected. The documented `catch (UnsatisfiedLinkError)` fallbacks in Netty and
Tomcat/tcnative do not name any of the four removed libraries.

*Corrected 2026-08-11:* this paragraph used to say `NativeLibraries.load`
"unconditionally returns success". It did when this was written; `W6-6` fixed
it the same day and the parenthetical outlived the defect.

### Of the loader scoping (2026-08-11)

Strict mode only, and only where a library is loaded through
`System.load`/`System.loadLibrary`/`Runtime.load*` from **two different class
loaders in one VM**. That is not a hypothetical shape: it is what a servlet
container does when two web applications, each with its own loader, probe one
`tcnative`, and it is the case HotSpot throws on. So it can fire without any
boot-loader bookkeeping, and every caller it fires at is one written around a
`catch (UnsatisfiedLinkError)` fallback — which is the point, and is also a
behaviour change for each of them.

`RJdkJni.libraryLoading` is the corpus vector in range and should be unmoved:
its two `System.loadLibrary("zip")` calls both still throw for the reason this
record already documents (`zip` is on `DYNAMIC_ALREADY_LOADED` and off the
allowlist), so nothing is recorded for them, and the `net` probe that follows is
a single load from a single loader with an empty table under it. Unverified —
nothing has been built or run.
