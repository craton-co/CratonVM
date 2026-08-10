# `TestNamingContext` — `ContextBindings.getContext()` returns null right after the binding was registered

| | |
|---|---|
| **Status** | OPEN — confirmed CratonVM-only, fails identically on all 3 GC backends |
| **HotSpot** | PASS (`OK (2)`, fresh-verified 2026-08-10) |
| **CratonVM** | FAIL 2/2, reproduces on default GC, G1, and ZGC alike |
| **Discovered** | 2026-08-10, cross-referencing FAILs common to all three GC-backend full-suite runs |

## Symptom

```
1) testGlobalNaming(org.apache.naming.TestNamingContext)
java.lang.NullPointerException: Cannot invoke "javax.naming.Context.lookup(String)" because "context" is null
	at org.apache.naming.TestNamingContext.doLookup(TestNamingContext.java:153)
	at org.apache.naming.TestNamingContext.testGlobalNaming(TestNamingContext.java:54)

2) testModuleEquivalentToComp(org.apache.naming.TestNamingContext)
(same shape)
```

`testGlobalNaming` calls `tomcat.enableNaming()`, starts the server, then
immediately does:

```java
Context webappInitial = ContextBindings.getContext(ctx);
Object obj = doLookup(webappInitial, COMP_ENV + "/" + LOCAL_NAME);
```

`webappInitial` comes back `null` on CratonVM — `ContextBindings` (Tomcat's
static registry mapping a catalina `Context`/thread to its JNDI `Context`)
either never got the binding that `enableNaming()` + `tomcat.start()` should
have registered, or the lookup by the same key that was just inserted fails
to find it. This is backend-independent (identical on default GC, G1, ZGC),
so it's not a GC/moving-object issue with the registry's map — more likely a
key-identity or registration-ordering gap.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore org.apache.naming.TestNamingContext
```

HotSpot control: `OK (2 tests)` in 0.7s.

## Suspected root cause (not yet isolated)

`org.apache.naming.ContextBindings` keys its registry either by the
catalina `Context` object's identity or by the current thread — not yet
checked against `native-io`/`native-collections` internals to see whether
CratonVM's implementation of whatever backing map it uses (`Hashtable`,
`ConcurrentHashMap`, or a `ThreadLocal`) diverges here. Given the family of
prior identity/`ThreadLocal`-keyed lookup bugs already fixed elsewhere in
this codebase (e.g. the redefine/side-table-immunity and warmed-dispatch-memo
issues), a similar identity-mismatch between registration and lookup is a
reasonable first place to check. No existing known-issue doc covers this
signature (checked `docs/known-issues` and `docs/internal` — the only hit
was an old raw results listing, not a dedicated bug doc).
