# `NativeLibraries.load` returned `true` for every library in the universe

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Headline: CLOSED, and now verified.** The unconditional
  `return Ok(Some(Value::Int(1)))` is gone; commit `b3aca74c8` replaced it with
  three ordered branches at `native-builtins/src/lib.rs:13894-13973` — real load
  at `:13913`, allowlist at `:13942-13945` (`zip` added on this path only), and
  a truthful failure at `:13964-13970` that raises `UnsatisfiedLinkError` when
  the caller asked for one and otherwise answers `Int(0)`. `throwExceptionIfFail`
  is now **read from the argument** (`lib.rs:13907`), not assumed. The
  registration is unconditional and `NativeKind::Bridge`, so **the fix is live
  in both modes** — there is no mode fork around it, and
  `NativeKind::allowed_in` (`native-api/src/registry.rs:4624-4629`) rejects only
  `SyntheticStub` under `JdkOnly`. `NativeLibraries.unload(String,ZJ)V` is
  registered too (`lib.rs:13991-14007`). The revert knob
  `CRATONVM_DBG_NATIVELIBRARIES_LOAD_OK=1` is at `lib.rs:13961`.
  Binary verification, taken 2026-08-12 on the dev binary at `ba65f1a19`: the
  shared vector `RJdkJni` runs to `PASS RJdkJni (35 checks)` in **both**
  `--jdk-only` and `--real-jdk`.
* **Residual: CLOSED elsewhere, and this record was already right about why.**
  The loader-scoped `loadedLibraryNames` landed 2026-08-11 on W5-1's road, in
  strict mode only, and correctly changed nothing here — §4 below had that
  distinction right. See W5-1-loadlibrary-allowlist-too-wide.md.
* **Residual: STILL OPEN — one, and it is W5-1's to arm.** The boot-loader case
  cannot fire on either road because `BootLoader.loadLibrary` is still a no-op
  (`native-builtins/src/lib.rs:14049-14054`; the record's old `:13813` anchor has
  rotted). Re-verified 2026-08-12: `lang_system::record_boot_loader_library`
  (`native-builtins/src/lang_system.rs:3183`) has **zero callers** tree-wide, and
  its own doc comment says so in bold. `LOADED_LIBRARIES` (`lang_system.rs:1757`)
  is real, is a `VmScoped` rather than a process global, and is torn down from
  `forget_vm_system_singletons` (`:3143`) — the strict-only half is genuinely
  landed, it just has no boot-loader event to record.

**AMENDED 2026-08-12 (W7-78-inherited-residual-closeout.md).**

* **A hazard on this record's headline fix that nobody had checked: the
  `BootLoader.loadLibrary` no-op is a `Bridge`, and it has to be.** Its
  registration uses the bare `registry.register(...)`, so its `NativeKind` is
  **ambient**. The enclosing registrar is `register_essential_natives_with_shims`
  (`native-builtins/src/lib.rs:7084`), which sets `Bridge` at `:7139-7140` and
  restores it after the one temporary `Intrinsic` window for regex
  (`:7666-7680`); line 14049 is outside that window, so the ambient kind is
  `Bridge`. That is the load-bearing fact, because `NativeKind::allowed_in`
  drops `SyntheticStub` under `JdkOnly`: had this registration drifted into a
  `SyntheticStub` window, the no-op would be dropped in strict mode, real
  `NativeLibraries` bytecode would run in its place, and the JDK's
  native-library lock — the exact thing this short-circuit exists to avoid
  during Linux boot-class `<clinit>` — would be back. **Anyone moving this
  registration must re-check the enclosing `set_category`, not just the call.**
* **Nothing further is fixable here without a run, and this is stated rather
  than guessed.** Arming `record_boot_loader_library` is one line, but it can
  only ever turn a success into an `UnsatisfiedLinkError`, and the library it
  would first claim for the boot loader is `net` — which
  `is_vm_provided_jdk_library` deliberately still carries *because* the dynamic
  rule cannot fire (`lang_system.rs:1939-1949`). Arming it without measuring
  therefore risks flipping `RJdkJni`'s `net` probe from LOADS to THROWS on the
  strict arm. The run, from the repo root:

  ```
  cratonvm --java-home "<jdk-25>" --jdk-only  -cp regression-suite/build RJdkJni
  cratonvm --java-home "<jdk-25>" --real-jdk  -cp regression-suite/build RJdkJni
  java -cp regression-suite/build RJdkJni          # HotSpot 25 oracle
  ```

  taken **with and without** the one-line arming, diffing the
  `CK RJdkJni loadedLibrary=` line. `--java-home` is not optional: a hand-run
  that omits it measures the host's default JDK and has already inverted a
  per-mode verdict once in this campaign.

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

## Known residual — closed 2026-08-11, on the other road

The residual as filed: there is no class-loader-scoped `loadedLibraryNames`
bookkeeping anywhere in this VM, and `BootLoader.loadLibrary` is a no-op, so the
JDK's *dynamic* rule — a second load of the same file from a different class
loader is an `UnsatisfiedLinkError` — can only fire when the JDK's own bytecode
has populated that set within a single run.

**It was still live when checked on 2026-08-11**, against dev `95b693f2d`, and
this record's own §4 is what said where the fix does *not* go. `grep
loadedLibraryNames` over the tree found five hits and every one was a comment
saying the state does not exist — this record, W5-1, the campaign README, and
two comments in `native-builtins/src/lib.rs`. No table, no loader key, no call
site.

**Nothing on this road changed, and that is the correct outcome.** §4 above
states the rule for this native — the "already loaded in another classloader"
error is raised by `NativeLibraries.loadLibrary` bytecode before `load` is
entered, so it is not this body's job and must not be duplicated here. That
reading survived the re-reading of the JDK source: `loadLibrary(Class,String,
boolean)` does the per-instance `libraries.get(name)` and the static
`loadedLibraryNames.contains(name)` checks, in that order, under
`acquireNativeLibraryLock(name)`, and only then constructs the
`NativeLibraryImpl` whose `open()` calls this native.

The fix went where the JDK's bytecode is *replaced* rather than run: CratonVM
intercepts `System.load`, `System.loadLibrary`, `Runtime.load0` and
`Runtime.loadLibrary0`, so for those four `ClassLoader.loadLibrary` ->
`NativeLibraries.loadLibrary` never executes and nobody consults or populates
the set. `native-builtins/src/lang_system.rs` now keeps a per-VM
`loader id -> library keys` table (`LOADED_LIBRARIES`, a `VmScoped` — not a
process global, contract §2) and applies the same two-step rule, under
`--jdk-only` only. Mechanism, the quoted specification, and the loader-identity
resolution are in W5-1-loadlibrary-allowlist-too-wide.md.

**What still cannot fire, on either road, is the boot-loader case this
paragraph opened with**, and the reason is the `BootLoader.loadLibrary` no-op
listed in the sibling-surfaces table above. `lang_system::record_boot_loader_library`
is written and deliberately unarmed; the one-line patch to that registration,
and the measurement that has to precede arming it (it can flip `RJdkJni`'s
`net` probe), are in W5-1. Branch 2 of this native does not widen the residual
and closing it is still not a bigger list.

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
