# HIB-CV-15 — `URL.setURLStreamHandlerFactory` is a no-op → `new URL("vfszip:…")` throws `MalformedURLException: unknown protocol`

**Severity:** Medium — fails any test that registers a custom URL stream-handler protocol. 1 confirmed class method: `JarVisitorTest.testJarVisitorFactory` (Hibernate's JBoss-VFS `vfszip:`/`vfsfile:` archive scanning).
**Status:** ✅ FIXED (worktree `fix/hibernate-full-suite`, `native-builtins/src/lib.rs` `native_url_set_stream_handler_factory_guard`).
**Mode:** Interpreter (JIT-off census) — not a JIT bug.
**HotSpot:** not affected.

## Symptom

`JarVisitorTest` reports `ok=8 failed=1`; the one failing method is `testJarVisitorFactory`:

```
java.net.MalformedURLException: unknown protocol: vfszip
```

The test registers a custom handler factory and then constructs a `vfszip:` URL:

```java
// JarVisitorTest.testJarVisitorFactory (HHH-6806)
URL.setURLStreamHandlerFactory( protocol -> {
    if ("vfszip".equals(protocol) || "vfsfile".equals(protocol))
        return new URLStreamHandler() {
            protected URLConnection openConnection(URL u) { return null; }
        };
    return null;
} );
...
jarUrl = new URL( defaultPar.toURL().toExternalForm().replace("file:", "vfszip:") );  // <-- throws
```

## Root cause

In real-JDK mode CratonVM does **not** intercept `new URL(String)` nor `URL.getURLStreamHandler(String)` — both run the real `java.net.URL` bytecode. The real `getURLStreamHandler("vfszip")` consults the static field `java.net.URL.factory` (an app-installed `URLStreamHandlerFactory`); `vfszip` is not a built-in protocol so the factory is the only way to resolve it.

But CratonVM *does* intercept the installer, `URL.setURLStreamHandlerFactory`, with `native_url_set_stream_handler_factory_guard` — a re-entrancy guard added for the Spring Boot loader (which installs a factory that recurses through class-init). The guard **only tracked recursion depth and returned without ever storing the factory** anywhere. So the static `factory` field stayed `null`, `getURLStreamHandler("vfszip")` found neither a cached handler, nor a factory, nor a built-in `sun.net.www.protocol.vfszip.Handler`, and threw `MalformedURLException: unknown protocol: vfszip`.

In short: the installer was swallowed, so the real lookup path had nothing to consult.

## Fix

Make the guard, on the outermost (non-reentrant) install, publish the factory into the real `java.net.URL.factory` static field via `set_static_field_by_name("java/net/URL", "factory", fac)`. `setURLStreamHandlerFactory` is a *static* method, so `args[0]` is the factory itself. The real `getURLStreamHandler` bytecode then consults it. Re-entrant installs (depth > 0) remain a no-op, preserving the "outermost factory wins" semantics the Spring Boot guard was added for.

Built-in protocols (`file:`, `jar:`, `http:`, …) are unaffected: the real `getURLStreamHandler` treats them as non-overrideable and resolves them via the built-in handler set *before* ever consulting the factory, so installing an app factory cannot disturb their resolution.

## Verification

`JarVisitorTest` 9/9 (was 8/9); the previously-failing `testJarVisitorFactory` now passes. No regression to the other 8 methods of the class (still pass), and built-in `file:`/`jar:` URL handling across the rest of the suite is unchanged.
