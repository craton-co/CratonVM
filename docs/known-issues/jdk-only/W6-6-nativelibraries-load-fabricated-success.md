# `NativeLibraries.load` returned `true` for every library in the universe

**Status:** FIXED in source 2026-08-07 (lane W6-6, JDK-only wave 2). Not yet
verified against a binary — see *How to verify* below.

## The hole, and why it was worth a lane on its own

A sibling lane narrowed `System.loadLibrary`'s allowlist
(`native-builtins/src/lang_system.rs::is_vm_provided_jdk_library`) so that
`loadLibrary("sunec")`, `("jvm")` and — on Windows — `("jsig")` correctly throw
`UnsatisfiedLinkError`, matching HotSpot 25 name for name.

That fix governs exactly one native: the intercepted `java/lang/System`
`loadLibrary`. **CratonVM has a second road to library loading**, and it was
still fabricating success at the end of it:

```
java.lang.ClassLoader.loadLibrary(...)                     ← real JDK bytecode
  jdk.internal.loader.NativeLibraries.loadLibrary(Class,String)
    findFromPaths → loadLibrary(Class,File) → NativeLibraryImpl.open()
      jdk.internal.loader.NativeLibraries.load(impl, path, isBuiltin, throwIfFail)
        └── native-builtins/src/lib.rs   →   return Ok(Some(Value::Int(1)))
```

The old body computed a handle, **dropped the error**, and returned `Int(1)`
unconditionally. So `sunec`, `jvm`, `jsig`, and any third-party JNI library that
is genuinely not on `java.library.path`, all reported "loaded" to any caller
that reached loading through real `ClassLoader`/`NativeLibraries` bytecode rather
than through the intercepted `System.loadLibrary`. Same divergence, other road.

This is the campaign's dominant defect species: **a fabricated success where the
spec mandates a failure**, producing a plausible wrong answer rather than a
crash, which is why it survived.

## The real contract, measured

From `javap -p -c jdk.internal.loader.NativeLibraries` and
`<java.home>/lib/src.zip` on JDK 25.0.3 (Microsoft build, Windows x64):

```java
/*
 * Return true if the given library is successfully loaded.
 * If the given library cannot be loaded for any reason,
 * if throwExceptionIfFail is false, then this method returns false;
 * otherwise, UnsatisfiedLinkError will be thrown.
 */
private static native boolean load(NativeLibraryImpl impl, String name,
                                   boolean isBuiltin,
                                   boolean throwExceptionIfFail);
```

Four facts that decide the implementation, each of which would produce a
*different* bug if guessed:

1. **The only caller is `NativeLibraryImpl.open()`**, which returns the boolean
   straight through (`javap`: `invokestatic NativeLibraries.load; ireturn`).
   `NativeLibraries.loadLibrary(Class,String,boolean)` maps `false` to a null
   `NativeLibrary`, and `findFromPaths` then tries the **next** directory of
   `sun.boot.library.path` / `java.library.path`. So `false` means *"not in this
   directory"*, not *"broken"*.

2. **`throwExceptionIfFail` decides which of the two failure shapes applies**, and
   it is not a constant:

   ```java
   private boolean throwExceptionIfFail() {
       if (loadLibraryOnlyIfPresent) return true;
       File file = new File(name);
       return file.exists();
   }
   ```

   `loadLibraryOnlyIfPresent` is `ClassLoaderHelper.loadLibraryOnlyIfPresent()`,
   which is `return true;` on every platform except macOS. So on Windows and
   Linux the argument arrives **`true`** and a failure is a **throw**; on macOS
   the `false` return is the live path. Getting this backwards in either
   direction is a fresh bug — assume-`false` keeps the silent wrong answer,
   assume-`true` turns a directory miss into a spurious crash. The implementation
   reads the argument.

3. **`name` is NOT a bare library name.** `loadLibrary(Class,File)` passes
   `file.getCanonicalPath()`, so the native sees `<java.home>\bin\zip.dll` or
   `<java.home>/lib/libzip.so`. Every policy question is phrased in bare names,
   so the path has to be decoded first — see `bare_native_library_name`.

4. **The "already loaded in another classloader" `UnsatisfiedLinkError` is raised
   by bytecode above this native**, out of the static `loadedLibraryNames` set,
   before `load` is ever entered. It is not this body's job and must not be
   duplicated here.

## What the body does now

`native-builtins/src/lib.rs`, three branches in order:

1. **Real load.** `ctx.load_native_library(path)`. A genuine third-party JNI
   library shipped beside an app really does open here. Only this branch yields a
   usable handle, stored as `lib_index + 1` — the same encoding
   `NativeLibrary.findEntry0` (panama.rs) decodes with `- 1`, with `0` reserved
   as the JDK's own "not loaded" sentinel that `open()` asserts on entry.

2. **The allowlist.** `lang_system::is_vm_provided_jdk_library(bare)`, reused, not
   copied — two lists would drift. These libraries never `dlopen` in this
   process (they link against `libjvm`/`jvm.dll`, which this process is not), yet
   a **cold** `loadLibrary` of them succeeds on HotSpot, so answering failure
   would be a fresh divergence in the opposite direction. The handle stays `0`,
   which is correct: nothing was opened, and every entry point these libraries
   exist to provide is already registered in this process as a Rust native, so
   `findEntry0` answering "symbol not found" is never reached for real work.

   Measured JDK 25 Windows truth, which this reproduces:

   | LOAD | THROW |
   | --- | --- |
   | `java zip net nio jimage verify management management_ext instrument extnet prefs j2pkcs11 sunmscapi` | `sunec jsig jvm` |

   `zip` is added **on this path only**. It is deliberately off the
   `System.loadLibrary` list because of the *dynamic* "already boot-loaded by
   `java.base`" rule (`Inflater.<clinit>` → `ZipUtils.loadLibrary()` →
   `BootLoader.loadLibrary("zip")`), which `RJdkJni.java:189-202` measures. On
   *this* path that rule is enforced above us by the JDK's own
   `loadedLibraryNames` bytecode, so the only question left for the native is the
   cold one, whose measured answer is LOADS.

3. **Truthful failure.** `UnsatisfiedLinkError` when `throwExceptionIfFail`,
   `false` otherwise.

`NativeLibraries.unload(String,boolean,long)` is now registered too — see below.

## The `RJdkJni` trap this had to avoid

A blanket "attempt the real load and fail" would have been wrong.
`RJdkJni.libraryLoading` (`regression-suite/src/RJdkJni.java:184-202`) asserts
that **a JDK-shipped library DOES load**, falling back from `zip` to `net`.
CratonVM never `dlopen`s libzip/libnet — their entry points are Rust natives — so
without branch 2 that assertion fails on both arms.

`RJdkJni` reaches loading through `System.loadLibrary`, which is intercepted, so
this change cannot move that test either way. The allowlist is respected here
anyway, because the *next* caller may well arrive by the bytecode road.

## Sibling surfaces audited in the same files

| surface | verdict |
| --- | --- |
| `NativeLibraries.unload(String,boolean,long)` | **WAS A HOLE, now registered.** Registered nowhere; an ACC_NATIVE method with no implementation throws `UnsatisfiedLinkError`, and this one runs on a **Cleaner thread** (`loadLibrary` registers `NativeLibraryImpl.unloader()` with `CleanerFactory.cleaner()` for every library loaded by a non-system loader), where the throw is swallowed and the library is silently never released. Harmless only while `load` never really opened anything — branch 1 makes that path live. Implemented as the same logical unload as `RawNativeLibraries.unload0`. |
| `NativeLibraries.findBuiltinLib(String)` | **CORRECT AS IS.** Returns `null`. HotSpot returns the path of a library *statically linked into libjvm*; CratonVM links none, and `null` is the JDK's own "not a built-in" answer, which `loadLibrary` handles on its ordinary path. A non-null answer here would be the fabrication. |
| `BootLoader.loadLibrary(String)` | **NO-OP, KEPT — but it is the source of a known residual.** Short-circuiting it avoids the JDK's native-library lock (which could block indefinitely on Linux boot classes). Cost: the JDK's `loadedLibraryNames` set is never populated by boot loads, so the dynamic "already loaded in another classloader" rule can never fire in this VM. Documented below. |
| `NativeLibrary.findEntry0(long,String)` (panama.rs, not this lane's file) | **CORRECT AS IS.** Decodes `handle - 1`; handle `0` decodes to `-1` and answers `0` = symbol not found. That is the right answer for a branch-2 "allowlisted but nothing opened" library. |
| `ClassLoader.findLibrary(String)` | **NOT INTERCEPTED, and should not be.** Not `ACC_NATIVE` in the real JDK — a `protected` Java method returning `null`. Real bytecode already gives the right answer. |
| `RawNativeLibraries.load0` (panama.rs) | **ALREADY TRUTHFUL.** Returns `Int(0)` on failure and does not throw, which is that native's actual contract (`RawNativeLibraryImpl.open()` maps `false` to `null`). Different road, different libraries — FFM downcalls into genuine third-party `.so`s CratonVM does not reimplement. |

## Known residual (not introduced here, and not closable from this file)

There is no class-loader-scoped `loadedLibraryNames` bookkeeping anywhere in this
VM, and `BootLoader.loadLibrary` is a no-op, so the JDK's *dynamic* rule — a
second load of the same file from a different class loader is an
`UnsatisfiedLinkError` — can only fire when the JDK's own bytecode has populated
that set within a single run. A program that uses `java.util.zip` (or
`java.net`, `java.nio`, `java.util.prefs`) and then loads the matching library by
name gets a success here where HotSpot throws. That residual is inherited
verbatim from the `System.loadLibrary` list's documented caveat; branch 2 does
not widen it, and closing it needs loader-scoped bookkeeping, not a bigger list.

## Risk: this is a behaviour change for real callers

Callers that previously believed a native backend loaded will now take their
`catch (UnsatisfiedLinkError)` fallback. **That is the correct behaviour** — it
is what Netty's `NativeLibraryLoader`, Tomcat's `AprLifecycleListener` and
tcnative all document — but it is a behaviour change for every such caller.

Suites that could move:

* **Tomcat** — `AprLifecycleListener` probes `tcnative`; expect it to log "APR
  not available" and continue on the NIO connector, which is the supported path.
* **Spring Boot / Netty (reactive, WebFlux)** — `epoll`/`kqueue`/`netty_tcnative`
  transport probes fall back to NIO. Netty is written for exactly this.
* **H2** — no JNI libraries; no exposure expected.
* Anything loading a real third-party `.so`/`.dll` that *is* present is
  unaffected: branch 1 still opens it and `JNI_OnLoad` still runs.

**The single knob if one regresses:**
`CRATONVM_DBG_NATIVELIBRARIES_LOAD_OK=1` restores the old unconditional success
for branch 3. It is a bisect aid, not a fix. The real repair for a library this
VM genuinely does implement is **one line** in
`lang_system::is_vm_provided_jdk_library` — which is where the claim belongs, and
where `System.loadLibrary` reads it too, so both roads stay in agreement.

## How to verify, once a binary exists

```
cargo build --release -p cratonvm-cli
cargo test -p cratonvm-native-builtins bare_native_library_name

# must be unchanged (this path is System.loadLibrary, not NativeLibraries.load):
target/release/cratonvm --real-jdk -cp regression-suite/build RJdkJni
target/release/cratonvm --jdk-only -cp regression-suite/build RJdkJni
java -cp regression-suite/build RJdkJni       # HotSpot 25 oracle
```

The falsifying observation for the whole lane: **if `NativeLibraries.load` is
never actually reached in either arm** — because `System.loadLibrary`,
`Runtime.loadLibrary0` and `BootLoader.loadLibrary` are all intercepted above it
— then this closes a hole that no current suite walks through, and the change is
correct but inert. Cheapest check is a `WARN`-level trace on entry to the native,
or `CRATONVM_DBG_NATIVELIBRARIES_LOAD_OK=1` producing *no* behavioural
difference anywhere.
