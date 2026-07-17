# `NestedJarFile.close()`'s `super.close()` bypasses the registered `ZipFile.close()` native, NPEs on null `res`

**Status: OPEN — found 2026-07-17**

## Symptom

| Class | Failing test | Note |
|---|---|---|
| `org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests` | `getInputStreamWhenNoCachedClosesJarFileOnClose` | NPE is `Suppressed` under a Mockito verification failure |
| `org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests` | `getContentTypeWhenNotKnownInStreamButKnownNameReturnsDeducedType` | NPE is the primary (only) failure |
| `org.springframework.boot.loader.net.protocol.jar.UrlJarFilesTests` | `getCachedWhenCachedReturnsCachedJar` | NPE is `Suppressed` under a `NullPointerException` from a mocked jar file |

```
java.lang.NullPointerException: Cannot invoke "java.util.zip.ZipFile$CleanableResource.clean()"
   java.util.zip.ZipFile.close(ZipFile.java:817)
   org.springframework.boot.loader.jar.NestedJarFile.close(NestedJarFile.java:392)
   org.springframework.boot.loader.net.protocol.jar.UrlNestedJarFile.close(UrlNestedJarFile.java:62)
   org.springframework.boot.loader.zip.AssertFileChannelDataBlocksClosedExtension$OpenFilesTracker.assertAllClosed(AssertFileChannelDataBlocksClosedExtension.java:82)
   org.springframework.boot.loader.zip.AssertFileChannelDataBlocksClosedExtension.afterEach(AssertFileChannelDataBlocksClosedExtension.java:53)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader.org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests.out.log`
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader.org.springframework.boot.loader.net.protocol.jar.UrlJarFilesTests.out.log`

## Root cause (confirmed, file:line precision)

`NestedJarFile.close()` (`apps/spring-boot/loader/spring-boot-loader/src/main/java/org/springframework/boot/loader/jar/NestedJarFile.java:391-392`) calls `super.close()`. `NestedJarFile extends JarFile`, and real `java.util.jar.JarFile` does **not** override `close()` — it inherits `java.util.zip.ZipFile.close()`. The `super.close()` call compiles to `invokespecial` against the constant-pool method reference `java/util/jar/JarFile.close()V` (the immediate compile-time superclass), matching JVMS semantics for super calls.

`vm/src/runtime/interpreter.rs::execute_invoke_kind` (line 18519) resolves the dispatch class for `invokespecial` as the raw constant-pool class name (`method_class_name`, i.e. `java/util/jar/JarFile`), not the receiver's runtime class. This is then passed into `vm/src/vm/vm_exec.rs::invoke_on_class_shared_inner`, which calls `find_method_recursive(class_id="java/util/jar/JarFile", "close", "()V", ...)` (`vm_exec.rs:13454`). Since `JarFile` itself declares no `close()` bytecode, resolution walks up to `ZipFile`, and `declaring_id` becomes `java/util/zip/ZipFile` — real, concrete bytecode, not `is_native()`.

At that point (`vm_exec.rs:13461-13470` onward), whether a registered native is allowed to override this concrete bytecode is gated by a long, explicit class+method allowlist (`check_override`). That allowlist has an entry for `java/util/jar/JarFile` covering `<init>`, `getManifest`, `stream`, `entries`, `getEntry`, `getJarEntry`, `getInputStream`, `size`, `close`, `getName` (`vm_exec.rs:14403-14417`) — but **no entry for `java/util/zip/ZipFile`** (confirmed via `grep -n "ZipFile" vm/src/vm/vm_exec.rs`, only two hits, both inside that `JarFile` list's comments). Because the *declaring* class resolved by `find_method_recursive` for this call is `ZipFile`, not `JarFile`, the allowlist check tests the wrong string and fails, `check_override` stays `false`, and the interpreter runs the real `ZipFile.close()` bytecode instead of the registered `native_jarfile_close` (`native-io/src/zip_real_jar.rs:856-872`, itself registered for both `java/util/jar/JarFile` and `java/util/zip/ZipFile` at `zip_real_jar.rs:952-1010` — the native exists and is reachable by name, it's just never looked up for this call shape).

Real `ZipFile.close()` (`ZipFile.java:817`) calls `this.res.clean()`. CratonVM's synthetic `<init>` for `JarFile`/`ZipFile` (`native_jarfile_init_file` et al., same file) never runs the real constructor, so the private `res` (`ZipFile$CleanableResource`) field is never populated and stays `null` — the exact same never-populated-`res` mechanism documented (for `stream()`/`getComment()`) by the already-merged fix `f1b2950a2` (`fix/zipfile-res-npe-20260712`, merged into `dev` the same day). That fix registered `stream()`/`getComment()` because they were the two `ensureOpen()`-calling methods left ungated at the time; this is the same class of gap for `close()`, but reached only through the invokespecial/super-call path, not a direct call — direct `jarFile.close()` calls on a concrete `JarFile`/`ZipFile`-typed receiver go through `invokevirtual`, whose dispatch (interpreter.rs's own `execute_invoke_kind` fast path, not `vm_exec.rs::invoke_on_class_shared_inner`) resolves the receiver's *actual* runtime class first and does hit the native — so ordinary, non-subclassed `.close()` calls are unaffected. Only a subclass's `super.close()` (or any other `invokespecial` call whose CP-referenced class doesn't itself declare the target method in real bytecode) is exposed.

**Fix direction (not applied — investigation/documentation only):** either add `java/util/zip/ZipFile` to the same allowlist entry that already covers `java/util/jar/JarFile` at `vm_exec.rs:14403-14417`, or (more robust against the same recurrence in other JDK class hierarchies) key the allowlist check off the *originally CP-referenced* class for `invokespecial` sites rather than the `find_method_recursive`-resolved declaring class, since a super-call's caller intent is "run whatever `JarFile`/its ancestors provide for `close()`", which is exactly what the `JarFile` allowlist entry was written to guarantee.

## Affected classes

| module | class |
|---|---|
| loader/spring-boot-loader | org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests |
| loader/spring-boot-loader | org.springframework.boot.loader.net.protocol.jar.UrlJarFilesTests |
