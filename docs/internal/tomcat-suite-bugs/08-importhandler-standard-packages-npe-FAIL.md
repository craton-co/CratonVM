# Bug 08 — ImportHandler standard-package class list NPE  (FIXED)

**Status:** FIXED (branch `fix/tomcat-ffm-module-bugs`, worktree `C:/craton/CratonVM-tcdefer`).
**Repro class:** `jakarta.el.TestImportHandlerStandardPackages` — now `OK (1 test)`
(was `Tests run: 1, Failures: 1`). HotSpot: PASS.

## Symptom (original)

```
java.lang.NullPointerException: Cannot invoke get on null
  at jakarta.el.TestImportHandlerStandardPackages.checkPackageClassList(...:58)
  at jdk.internal.module.SystemModuleFinders$SystemModuleFinder.find(SystemModuleFinders.java:278)
```

Line 58 is `ModuleFinder.ofSystem().find("java.base").get().open().list()...`.
SystemModuleFinders.java:278 is `return Optional.ofNullable(nameToModule.get(name));`
— the NPE is `nameToModule` being **null**.

## Root cause

`ModuleFinder.ofSystem()` was a **synthetic stub** (`native-builtins/src/lib.rs`):
it allocated a bare `SystemModuleFinders$SystemModuleFinder` whose `nameToModule`
field is never populated and overrode `findAll()` to return an empty Set. The stub
existed only so Spring's `PathMatchingResourcePatternResolver.<clinit>`
(`ofSystem().findAll().stream()...`) would not NPE. But `find(String)` ran the real
bytecode `nameToModule.get(name)` on the null field → NPE.

Why the genuine `SystemModuleFinders.ofSystem()` could not simply be unstubbed:

1. **Fast path is unavailable.** `SystemModulesMap.allSystemModules()` returns
   **null** in CratonVM. The generated `SystemModules$all`/`$0..$5`/`$default`
   classes are produced by **jlink** and live only in the linked `lib/modules`
   jimage — *not* in the `jmods/java.base.jmod` CratonVM boots from (which ships
   the placeholder `SystemModulesMap` whose `allSystemModules()` returns null).
2. **Slow path (`ofModuleInfos`) is too heavy + needs JLMA.** It (a) builds every
   module's descriptor through `JavaLangModuleAccess.newModuleBuilder` (unwired —
   NPE) and (b) eagerly memory-maps the ~140 MiB run-time image. Paying that on
   every `ofSystem()` caller (Spring calls it at boot) would OOM the 256 MiB
   default heap.

Two further VM gaps blocked the real `ImageReader` path even when reached:
- `jdk/internal/jimage/NativeImageBuffer.getNativeMap` was unimplemented →
  `UnsatisfiedLinkError` in `BasicImageReader.<init>`.
- `sun.arch.data.model` was **unset** → `BasicImageReader` computed `IS_64_BIT=false`
  → `MAP_ALL=false` → it opened a `FileChannel` (→ missing
  `WindowsNativeDispatcher.initIDs`) instead of using the whole-image map.

## Fix (three changes)

1. **Lazy system-module finder** (`native-builtins/src/lib.rs`): keep
   `ofSystem()`/`findAll()` cheap (findAll → empty, no eager image read, **no
   Spring regression**), but make `SystemModuleFinder.find(name)` return a
   `ModuleReference` whose `open()` builds a **real** `SystemModuleReader` on
   demand. `ModuleReferenceImpl.open()` is overridden to delegate to a non-null
   `readerSupplier` (real module-path references) and otherwise build the system
   reader. The ~140 MiB image is therefore read **only** when code actually
   traverses module contents (`reader.list()/read()`), via the genuine JDK
   `ImageReader`/`BasicImageReader`.
2. **`NativeImageBuffer.getNativeMap`** (`native-io/src/lib.rs`,
   `register_io_natives`): reads the run-time image into a real **heap**
   `ByteBuffer` (`ByteBuffer.wrap`). A heap buffer is required because CratonVM's
   absolute `getInt`/`asIntBuffer`/`slice` read back correctly from heap buffers
   (verified) but not from its mapped/direct snapshot path.
3. **`sun.arch.data.model`** (`vm/src/vm/vm_init.rs`): publish the pointer width
   (HotSpot parity), derived from `size_of::<usize>()*8`, so `MAP_ALL=true` and
   `BasicImageReader` uses the whole-image map instead of a FileChannel.

## Verification

```
TestImportHandlerStandardPackages   -> OK (1 test)          (HotSpot: PASS)
ModuleFinder.ofSystem().find("java.base").open().list()
  java/lang/*.class count = 976                              (== HotSpot 976)
ModuleFinder.ofSystem().findAll().size() = 0                 (cheap; unchanged
                                                              vs prior stub —
                                                              Spring unaffected)
TestImportHandler (sibling)         -> OK (17 tests)         (no regression)
TestByteChunk                       -> OK (8 tests)          (no regression)
```

`find().open().list()` needs an adequate heap (the test uses `-Xmx2g`) because the
image is read into a heap `ByteBuffer`; a default-heap caller that *traverses*
module contents will OOM (findAll-only callers are unaffected). A fully
memory-efficient path would require an off-heap `DirectByteBuffer` whose absolute
reads work in real-JDK mode — a separate, deeper ByteBuffer/Unsafe effort.

## Reproduction

```
cratonvm.exe -Xmx2g --add-opens java.base/java.lang=ALL-UNNAMED -cp <cp> \
  org.junit.runner.JUnitCore jakarta.el.TestImportHandlerStandardPackages
# CWD: apps/tomcat ; -> OK (1 test)
```
