# 14 — `classpath:` URL protocol unresolvable → `MalformedURLException: unknown protocol: classpath` (FIXED)

**Suite:** Tomcat full-suite (`SUITE-RESULTS-tcfull-2026-06-22.md`, "Newly surfaced" table).
**Status:** ✅ FIXED on `dev` (branch `fix/tc0622-classpath-url-protocol`).
**Affected (reported):** `org.apache.catalina.webresources.TestClasspathUrlStreamHandler`,
`org.apache.tomcat.util.file.TestConfigFileLoader` (+ partially
`org.apache.catalina.core.TestPropertiesRoleMappingListener`, see residuals).

## Symptom

```
java.net.MalformedURLException: unknown protocol: classpath
java.io.IOException: Cannot obtain resource for specified location
  [classpath:org/apache/catalina/mbeans-descriptors.xml]: no readable file,
  classloader resource, or this is not a resolvable URI
```

Tomcat registers its own `classpath:` (and `war:`) URL scheme through
`TomcatURLStreamHandlerFactory`, a `URLStreamHandlerFactory` installed via
`URL.setURLStreamHandlerFactory(this)`. Both failing tests install it in
`@BeforeClass` (`TomcatURLStreamHandlerFactory.getInstance()`), then resolve a
`classpath:` URL — yet CratonVM reported the scheme as unknown.

## Root cause — three independent defects on the same path

The real `java.net.URL.getURLStreamHandler(protocol)` resolves an app scheme via:

```java
if (isOverrideable(protocol) && VM.isBooted()) {
    fac = factory;                       // static java.net.URL.factory
    if (fac != null) handler = fac.createURLStreamHandler(protocol);
    ...
}
```

and `URI.toURL()` / `new URL(String)` both funnel through it. Three things broke
that chain in CratonVM:

1. **`jdk.internal.misc.VM.isBooted()` returned `false`.** On JDK 25 `isBooted()`
   is plain bytecode `return initLevel >= SYSTEM_BOOTED(4)`, reading the static
   field `jdk.internal.misc.VM.initLevel`. CratonVM boots natively and never runs
   the real `System.initPhase2/3` that would call `VM.initLevel(int)` to set that
   field, so it stays `0` and `isBooted()` is permanently false → the factory
   branch above is skipped entirely. A native override returning `1` was
   *registered* (commit `873355f1`, for JASPIC `ResourcesMgr`) but never
   *dispatched*: `isBooted` has real bytecode, and CratonVM only prefers a native
   over real bytecode when the `(class,method,desc)` triple is in
   `force_native_over_real_jdk_bytecode`. `isBooted` was missing from that list,
   leaving the native (and the JASPIC fix) inert.

2. **The factory was published into `URL.factory` *before* `URL.<clinit>` ran.**
   `native_url_set_stream_handler_factory_guard` (HIB-CV-15) writes the installed
   factory into the static `java.net.URL.factory` field via
   `set_static_field_by_name`. But force-native dispatch runs the guard as the
   method body *without* the normal invokestatic class-init step, so if URL is
   not yet initialized the write lands in a statics vector that URL's later
   initialization re-creates and discards — `factory` reads back `null`.

3. **`URI.toURL()` was a synthetic stub with a hard-coded protocol allowlist.**
   `net_phase_e.rs`'s `URI.toURL` native rejected any scheme not in a fixed
   `KNOWN_PROTOCOLS` set (`file`/`jar`/`http`/…/`war`) with
   `MalformedURLException: unknown protocol`, never consulting the registered
   factory — so `URI.create("classpath:…").toURL()` (used by both tests) failed
   even after 1 and 2 were fixed.

## Fix

1. **interpreter.rs** — add `("jdk/internal/misc/VM", "isBooted", "()Z")` to
   `force_native_over_real_jdk_bytecode` so the already-registered native (returns
   `1`) actually shadows the real bytecode. By the time any app/JDK-library code
   calls `isBooted()` the VM is genuinely up, matching HotSpot's post-boot `true`
   (consistent with the sibling `VM.initLevel()` floor-of-2 native). Also revives
   the dormant JASPIC `ResourcesMgr` fix.

2. **lib.rs** — in the `setURLStreamHandlerFactory` guard, call
   `ctx.ensure_class_initialized("java/net/URL")` *before* publishing `factory`,
   so the static store exists and is stable when we write it.

3. **net_phase_e.rs** — for a `URI.toURL()` scheme outside the built-in set,
   delegate to the **real** `java.net.URL.<init>(String)` (un-intercepted in
   real-JDK mode) instead of rejecting. The real ctor runs `getURLStreamHandler`,
   consulting the now-published factory. This resolves `classpath:` while STILL
   throwing `MalformedURLException` for genuinely-unknown schemes — including the
   single-letter Windows drive (`C:`) case that Tomcat's
   `Bootstrap.createClassLoader` relies on catching to fall into its `*.jar`
   glob-expansion branch (verified: real `URL("C:/…")` throws `unknown protocol: c`).

A fourth, smaller item surfaced once the scheme resolved: CratonVM's synthetic
`URL.openStream` for `classpath:` threw a bare `IOException` on a missing
resource, where the real `ClasspathURLStreamHandler` throws
`FileNotFoundException` (asserted by `TestConfigFileLoader.test02`,
`@Test(expected=FileNotFoundException.class)`). Changed that arm from `ioex` to
`fnfex` to mirror the `file:` arm.

## Validation (`fix/tc0622-classpath-url-protocol` vs HotSpot)

| Class | Before | After |
|-------|-------:|------:|
| `TestClasspathUrlStreamHandler` | FAIL (unknown protocol) | **PASS** (1/1) |
| `TestConfigFileLoader` | FAIL (4/4 classpath) | **3/4** (test01/02/04 pass) |

`TestConfigFileLoader.test03` still fails — unrelated: it opens a **directory**
through a `file:` URL; HotSpot's `FileURLConnection` returns a directory-listing
stream, CratonVM's synthetic `file:` `openStream` does `fs::read` on the dir →
Windows "access denied" (os error 5). Separate `file:`-URL gap, was failing
pre-fix.

Regression checks (all green): HIB-CV-15 `vfszip`/`vfsfile` custom-factory probe
(`new URL` + `URI.toURL`), Bootstrap `C:`-drive glob still throws, `file:`/`war:`
`URI.toURL` unchanged, `cargo test -p cratonvm-native-builtins` (2688 pass; the 1
fail is a pre-existing env-specific `Runtime.exec`/SecurityManager test, untouched
by this change), vm `getstatic` round-trip tests pass.

## Residual (separate, out of scope)

`TestPropertiesRoleMappingListener` now resolves the `classpath:` scheme (no more
"unknown protocol") but its two classpath methods still fail: the role-mapping
files live in a **deployed webapp's `WEB-INF/classes`**
(`test/webapp-role-mapping/...`). HotSpot's `ClasspathURLStreamHandler` finds
them via `Thread.currentThread().getContextClassLoader().getResource(path)` — the
webapp loader. CratonVM's synthetic `URL.openStream` uses `ctx.find_resource`,
which searches only bootstrap→ext→**app** classpaths, never the thread context /
webapp classloader. That is a deeper resource-scoping gap (webapp-classloader
resource resolution), distinct from the protocol-registration bug fixed here.
