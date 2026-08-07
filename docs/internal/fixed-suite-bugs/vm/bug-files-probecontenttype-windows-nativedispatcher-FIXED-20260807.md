# Windows: `Files.probeContentType` died in `WindowsNativeDispatcher.initIDs()` — FIXED 2026-08-07

## Status

**✅ FIXED 2026-08-07.** Two native registrations; the JDK's own detector body
runs. Filed the same day from the `Files`-surface sweep written while closing
the `Files.setAttribute` / abstract-`FileSystemProvider` gap — it was **not** a
residual of that bug (different mechanism, different subsystem), which is why it
got its own page.

## Symptom

```
java.lang.UnsatisfiedLinkError: sun/nio/fs/WindowsNativeDispatcher.initIDs()V
        at sun.nio.fs.WindowsNativeDispatcher.<clinit>(WindowsNativeDispatcher.java:1100)
        at sun.nio.fs.RegistryFileTypeDetector.implProbeContentType(RegistryFileTypeDetector.java:55)
        at sun.nio.fs.AbstractFileTypeDetector.probeContentType(AbstractFileTypeDetector.java:75)
        at java.nio.file.Files.probeContentType(Files.java:1594)
```

and every call after the first with `NoClassDefFoundError:
sun/nio/fs/WindowsNativeDispatcher` — the class whose `<clinit>` had already
failed. Linux was unaffected: its detector reads `/etc/mime.types` in pure
bytecode.

## What it was

`Files.probeContentType` walks the installed `FileTypeDetector`s and then the
platform default, which on Windows is `sun.nio.fs.RegistryFileTypeDetector` — a
`HKEY_CLASSES_ROOT\<ext>` `"Content Type"` lookup. Its `implProbeContentType` is
pure JDK bytecode except for exactly two natives, and both were missing:

* `WindowsNativeDispatcher.initIDs()`, reached from the `<clinit>` that the
  first `asNativeBuffer(...)` call triggers, and
* `RegistryFileTypeDetector.queryStringValue(long subKey, long name)`.

Note where the stack stops: `RegistryFileTypeDetector`'s own `<clinit>`
(`BootLoader.loadLibrary("net")` / `("nio")`) had already **succeeded**, and
`AbstractFileTypeDetector.probeContentType` was running. Only the two leaves
were missing.

## The fix

`native-builtins/src/phases_late/nio_file.rs`, both `#[cfg(windows)]`:

* **`WindowsNativeDispatcher.initIDs()V` → no-op.** It caches jfieldIDs for a
  JNI layer this VM does not have; there is nothing to cache, so this is the
  honest implementation rather than a stub.
* **`RegistryFileTypeDetector.queryStringValue(JJ)Ljava/lang/String;`** → a real
  `RegGetValueW` on `HKEY_CLASSES_ROOT`, returning `null` for a missing key or
  value exactly as the JNI original does (it returns NULL and does not throw).

### Why not shadow `implProbeContentType` instead

That was the obvious one-method seam and it is the wrong one, twice over:

1. `implProbeContentType` has concrete bytecode, so a native on it only wins
   with a `check_override` entry — and that chain's own banner says every
   class-name disjunct in it is "prefer our native over the real JDK's concrete
   bytecode", i.e. precisely what contract §1.4 forbids and what must stop
   growing.
2. It would have skipped the rest of
   `AbstractFileTypeDetector.probeContentType`: its fallback to
   `URLConnection.getFileNameMap()` and its `parse()` validation. **The fallback
   is load-bearing** — `.js` has no registry entry at all on Windows and reaches
   `text/javascript` only through it.

Registering the two leaves keeps every line of JDK logic above them running.

### Why the answers have to come from the registry

A detector that answered from the properties map everywhere would look right and
not be HotSpot. The two sources disagree where both have an opinion:

| ext | Windows registry | `content-types.properties` | HotSpot/Windows answers |
|---|---|---|---|
| `.xml` | `text/xml` | `application/xml` | **`text/xml`** |
| `.zip` | `application/x-zip-compressed` | `application/zip` | **`application/x-zip-compressed`** |
| `.js` | *(absent)* | `text/javascript` | **`text/javascript`** |

`probes/ProbeContentType.java` is built around exactly those three rows, so the
transcript itself proves which half produced each answer.

### Two implementation details that are not incidental

* **`queryStringValue`'s arguments are not necessarily pointers.** They are
  addresses of NUL-terminated UTF-16 strings in JDK `NativeBuffer`s, which
  `Unsafe.allocateMemory` may back with a *tagged arena handle* rather than a
  real OS pointer. `ctx.copy_from_native_memory` is the bridge that already
  classifies the two; a raw dereference SIGSEGVs on the handle form.
* **`RegGetValueW` is called twice, sized then read.** The value is arbitrary
  user-writable registry data, so a fixed buffer would be a guess about someone
  else's content.

## Consequence worth stating

`WindowsNativeDispatcher`'s class initialiser now **succeeds**. Its ~80 other
natives are no longer unreachable behind a failing `<clinit>`; they now fail
individually, by their own name. That is the same error class and strictly more
informative — but it does move where the error appears, and a caller that used
to die at `initIDs` will now die at whichever `WindowsNativeDispatcher` native
it actually wanted.

## Verification

`probes/ProbeContentType.java`, 13 lines, diffed against HotSpot on the same
machine:

| | before | after |
|---|---|---|
| Windows | `UnsatisfiedLinkError` on line 1, `NoClassDefFoundError` on 2–13 | **byte-identical, 13/13** |
| Linux | byte-identical | byte-identical (change is `#[cfg(windows)]`) |

The Windows transcript proves both halves ran: `.xml`/`.zip` carry the
*registry* values, `.js` carries the *fallback* value.

Regression, `probes/FilesSweep.java` (43 `Files` calls) on Windows: **3
divergent lines → 2**, and both survivors are cosmetic with matching exception
types — a `ClassCastException` message clause, and a path rendered with `/`
where HotSpot uses `\`.

Rust gates: `cargo test -p cratonvm-native-builtins --lib` 3382 passed / 0
failed on Windows and 3385 / 0 on Linux; `stub_ratchet` 7 passed;
`shim_inheritance_guard` 3 passed. `registry_contracts` fails on
`native_symbol_lookups_all_pass_the_host_access_gate` — a staleness guard
("source scan found only 6 native-symbol call sites; expected at least 8"),
red on pristine `dev` and unrelated: this change adds no
`ctx.find_native_symbol` / `ctx.load_native_library` call site, and the counts
are identical between the two revisions.

Three unit tests (`registry_content_type_tests`) pin the failure shapes that
would silently suppress the JDK's fallback: an absent key, an absent value under
a *present* key (a different `RegGetValueW` error code), and an embedded NUL,
which Win32 would otherwise use to truncate the query to a different key. They
deliberately do not assert what `.txt` maps to — that is a fact about the
machine's registry, not about this code, and asserting it would be a latent
failure on a differently-configured box.

## Reproducing the original failure

```bash
javac -d /tmp/pct probes/ProbeContentType.java
cratonvm.exe --java-home <jdk25> -c /tmp/pct ProbeContentType
```

Add `-Dct.stack=1` for the full stack on each failing line.
