# Bug TC0622 — JSP runtime-compiled classes (`org.apache.jsp.*_jsp`, JSTL TLV hierarchy) fail to load on CratonVM → empty 500 → `String.contains()` on null

> **✅ RESOLVED — FIXED + MERGED dev `650f149b`** (branch
> `claude/clever-kapitsa-697e6c`, fix commit `c07e677a`, 2026-06-29). All 6
> cluster tests pass (TestPageContext, TestScopedAttributeELResolver,
> TestImportELResolver, TestOptionalELResolverInJsp, TestCompositeELResolver,
> TestSessionCookieConfig); also fixed one of TestJspServlet's two failures
> (testBug56568b). No regressions vs the dev baseline. Root cause was **four
> layered real-JDK class-resolution defects**, each surfacing only once the
> previous was fixed — NOT the ByteChunk/contains symptom below (that is correct
> Tomcat behaviour for an empty 500):
> 1. `URLClassLoader.findClass` was never served by a native in real-JDK mode.
>    Jasper's `JasperLoader` overrides BOTH `loadClass` overloads and calls
>    `findClass()` directly, bypassing CratonVM's `loadClass` native and reaching
>    the real `URLClassLoader.findClass` bytecode whose shimmed `ucp.getResource`
>    returns null → CNF for `org.apache.jsp.*_jsp`. Fixed with `ucl_real_find_class`
>    (resolves from the global dynamic classpath the loader's `<init>` registered)
>    + `intercept_urlclassloader_subclass_find_class` (the findClass CP methodref
>    names the *subclass*, so the static-class force-native gate missed it).
> 2. `defineClass1` resolved a class's superclass/interfaces only via the global
>    classpath, never the defining loader — JSTL `JstlCoreTLV` → `JstlBaseTLV` (both
>    in a `/WEB-INF/lib` jar served by Tomcat's `WebResourceRoot`) failed to define.
>    Fixed with `preload_supertypes_via_loader` (JVMS §5.3.5).
> 3 & 4. Runtime symbolic refs among webapp classes (`new JstlCoreTLV$Handler`,
>    `invokestatic XmlUtil.newXMLReader`) resolved only globally. Fixed with a
>    strictly-additive defining-loader fallback (`drive_defining_loader_load`) on a
>    global resolution miss, in `resolve_class_loader_aware` + `execute_invokestatic`.
>
> See memory `reference_tc0622_jsp_webapp_loader_resolution` for full detail. The
> original analysis below is retained for historical context.

> **One-line root cause:** The `String.contains(...)` NPE at line 34 of both
> tests is a **downstream symptom**, not the bug. The real defect is server-side:
> Jasper compiles the JSP/tag to a servlet class with ECJ, but the generated
> `org.apache.jsp.bug49nnn.bug49196_jsp` (resp. the `JstlCoreTLV` →
> `JstlBaseTLV` TagLibraryValidator class hierarchy) **cannot be loaded/defined
> on CratonVM's webapp class loader**. The HTTP request therefore returns a 500
> with an **empty body**; `ByteChunk.toString()` returns `null` on an unwritten
> chunk (`ByteChunk.java:606 isNull() → return null`), and the test's
> `result.contains("OK")` NPEs on that null. HotSpot loads the generated class
> fine, gets a 200 body containing "OK", and passes.

**Severity:** Medium-High (blocks the entire runtime-JSP-compilation path: any
test that fetches a `.jsp`/`.tag` and asserts on the response body).
**Status on CratonVM:** FAIL. **HotSpot:** PASS.
**Run date:** 2026-06-22
**Binary:** dev `df11ac00` (worktree `C:\craton\CratonVM-tctest`).

## Affected classes (cluster of 4 — same root cause family)

Reported pair:
- `jakarta.servlet.jsp.TestPageContext` (`testBug49196`)
- `jakarta.servlet.jsp.el.TestScopedAttributeELResolver` (`testBug49196`)

Two siblings in the same run with the identical server-side fingerprint
(`ClassNotFoundException: org.apache.jsp.*`):
- `jakarta.servlet.jsp.el.TestImportELResolver`
  (`org.apache.jsp.bug6nnnn.bug66582_jsp`, `…bug66441_jsp`)
- `jakarta.el.TestOptionalELResolverInJsp`
  (`org.apache.jsp.tag.web.echo_tag` — "Unable to load class for JSP")

## Symptom

Client-side NPE that the test reports (TestPageContext, identical shape for the
ELResolver test):

```
java.lang.NullPointerException: Cannot invoke "String.contains(java.lang.CharSequence)"
    at jakarta.servlet.jsp.TestPageContext.testBug49196(TestPageContext.java:34)
```

The test body is trivial — it fetches the JSP and asserts on the body:

```java
ByteChunk res = getUrl("http://localhost:" + getPort() + "/test/bug49nnn/bug49196.jsp");
String result = res.toString();          // <-- null when the response had no body
Assert.assertTrue(result.contains("OK")); // line 34: NPE on null `result`
```

`TomcatBaseTest.getUrl(String)` discards the HTTP status code and returns only
the body `ByteChunk`. `ByteChunk.toString()` (`ByteChunk.java:584` →
`toString(...)` line 606) returns **`null`** when `isNull()` (the chunk was
never written to — i.e. the response had an empty body). So an empty 500
deterministically becomes a `.contains()`-on-null NPE.

The **actual** failure is in the embedded Tomcat's server log:

`TestPageContext.log.err` (and the two siblings — same signature):
```
ERROR [...[/test].[jsp]] Servlet.service() for servlet [jsp] threw exception
  [org.apache.jasper.JasperException: java.lang.ClassNotFoundException:
   org.apache.jsp.bug49nnn.bug49196_jsp]
  with root cause (java/lang/ClassNotFoundException: org.apache.jsp.bug49nnn.bug49196_jsp)
```

`TestScopedAttributeELResolver.log.err` fails one step earlier — at TLD
validation, in the JSTL `TagLibraryValidator` class hierarchy:
```
WARN  cratonvm_native_builtins::lang_system: ClassLoader.defineClass1(
        org/apache/taglibs/standard/tlv/JstlCoreTLV) failed:
        ClassFile(ClassNotFound { class_name: "org/apache/taglibs/standard/tlv/JstlBaseTLV" })
ERROR [...[/test].[jsp]] ... JasperException: Failed to load or instantiate
        TagLibraryValidator class: [org.apache.taglibs.standard.tlv.JstlCoreTLV]
  with root cause (java/lang/NullPointerException:
        Cannot invoke "java.lang.Class.getPackageName()" because "c" is null)
```
Here `defineClass1(JstlCoreTLV)` aborts because its **superclass**
`JstlBaseTLV` is not found; the returned `Class c` is then null, and Jasper's
`c.getPackageName()` NPEs. Same family: a class produced/required by the Jasper
pipeline is not loadable, the request 500s with an empty body, and the client
NPEs on `null.contains(...)`.

## Root cause (analysis)

A Java compiler **is** available — `ecj-4.39.jar` is on the classpath
(`C:\Users\Victor\tomcat-build-libs\ecj-4.39\ecj-4.39.jar`), and both JSP source
files exist under `test/webapp/bug49nnn/bug49196.jsp` and
`test/webapp/bug6nnnn/bug62453.jsp`. So this is **not** a "no compiler"
environmental gap.

The defect is in CratonVM's handling of the Jasper compile→define→load cycle for
**dynamically generated webapp classes** and their dependency hierarchies:

1. Jasper translates the `.jsp` to a `org.apache.jsp.*_jsp` Java source and
   invokes ECJ to compile it, then asks the webapp class loader to load that
   freshly written class. CratonVM throws `ClassNotFoundException` for it —
   either the compiled bytes were never written/visible to CratonVM's webapp
   loader, or the loader can't pick up a class generated at runtime into the
   work dir. (The 4-test cluster all share exactly this
   `ClassNotFoundException: org.apache.jsp.*` signature.)

2. The `JstlBaseTLV`/`JstlCoreTLV` variant shows the same loader weakness on a
   **superclass-resolution** path: `defineClass1` is given `JstlCoreTLV` but its
   super `JstlBaseTLV` can't be resolved from the (jstl/standard) jar, so the
   define fails and Jasper gets a null `Class`, then NPEs in
   `getPackageName()`. There is already a related hardening note in
   `native-builtins/src/phases_late.rs` (≈ line 16026) for exactly this class
   (a prior zero-length-class read that produced a `ClassFormatError` on
   `JstlCoreTLV`); the current failure is the next link — its superclass not
   being found by the same loader path.

The `String.contains()`/`ByteChunk.toString()==null` chain is correct Tomcat
behavior and **not** a CratonVM ByteChunk bug — it is purely how an empty 500
surfaces in these tests.

## Reproduction

Suite was running concurrently; rely on the captured `.log.err` stacktraces
above. To reproduce in isolation:

```powershell
cd C:\craton\CratonVM\apps\tomcat
$exe = "C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe"
$cp  = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
& $exe -Xmx2g -cp $cp org.junit.runner.JUnitCore jakarta.servlet.jsp.TestPageContext
# Expect: NPE "Cannot invoke String.contains(...)" at TestPageContext.java:34,
# and in the server log: ClassNotFoundException: org.apache.jsp.bug49nnn.bug49196_jsp
```

Server-side core repro (no test harness): fetch any `.jsp` from the test webapp
and observe the `org.apache.jsp.*_jsp` `ClassNotFoundException` in the Jasper
log — the generated servlet class is never loadable.

## Recommendation

**FIX / investigate (VM-side, not a test-env artifact).** The cluster is a single
real defect in the runtime-JSP-compile-and-load path. Two angles to pin:

1. **Generated-class loading:** trace where Jasper writes the compiled
   `org.apache.jsp.*_jsp` class and why CratonVM's webapp class loader returns
   `ClassNotFoundException` for it (work-dir output not visible to the loader,
   or a runtime-`defineClass`/`loadClass` gap for dynamically generated
   classes). This is the dominant signature (3 of 4 tests).

2. **Superclass resolution in `defineClass1`:** make `defineClass1` resolve
   `JstlCoreTLV`'s super `JstlBaseTLV` from the jstl/standard jar (continue the
   `phases_late.rs` JarEntry-field fix that already addressed the zero-length
   read for the same class), and ensure a failed define does not hand Jasper a
   null `Class` that then NPEs in `getPackageName()`.

Not a duplicate of the existing `CRATONVM_BUGS/BUG-*` docs (DF05 is the regex
`new String(StringBuilder)` cast; no existing doc covers `org.apache.jsp.*`
runtime compilation or the JSTL TLV hierarchy). The `String.contains()` symptom
is shared with `BUG-TC0622-addcharsetfilter-contenttype-null.md` only at the
NPE shape — root cause here is JSP class loading, unrelated.
