# `TestStaticFieldELResolver` — reflection can't find a static field literally named after an enum constant

| | |
|---|---|
| **Status** | OPEN — confirmed CratonVM-only, fails identically on all 3 GC backends |
| **HotSpot** | PASS (`OK (34)`, fresh-verified 2026-08-10) |
| **CratonVM** | FAIL 2/34, reproduces on default GC, G1, and ZGC alike |
| **Discovered** | 2026-08-10, cross-referencing FAILs common to all three GC-backend full-suite runs |

## Symptom

```
1) testGetValue09(jakarta.el.TestStaticFieldELResolver)
jakarta.el.PropertyNotFoundException: No public static field named [GET_TYPE]
was found on exported class [jakarta.el.TestStaticFieldELResolver$MethodUnderTest]
	at jakarta.el.StaticFieldELResolver.getValue(StaticFieldELResolver.java:62)
	at jakarta.el.TestStaticFieldELResolver.testGetValue09(TestStaticFieldELResolver.java:115)

2) testGetType09(jakarta.el.TestStaticFieldELResolver)
(same shape, via StaticFieldELResolver.getType)
```

Both failing tests do the same thing: resolve a static field on the nested
class `MethodUnderTest` whose *name* is the string form of the enum constant
`MethodUnderTest.GET_TYPE` (`resolver.getValue(context, new
ELClass(MethodUnderTest.class), MethodUnderTest.GET_TYPE.toString())`). Since
this is being tested at all, `MethodUnderTest` must declare an actual public
static field literally named `GET_TYPE` (distinct from the enum constant of
the same simple name used to select it) — `StaticFieldELResolver`'s own
reflection-based lookup for that field comes back empty on CratonVM, and
throws exactly the exception this test isn't expecting.

This is backend-independent — identical failure on the default generational
GC, G1, and ZGC — so it's a reflection/classfile-metadata gap, not a GC
interaction.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
<cratonvm.exe> --java-home "<real JDK 25>" -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore jakarta.el.TestStaticFieldELResolver
```

No server/sockets needed — fast, pure reflection test (0.1s on both VMs), one
of the easier ones in this batch to isolate further.

## Suspected root cause (not yet isolated)

Not yet checked against CratonVM's `Class.getFields()`/`getDeclaredFields()`
native implementation. Candidates: a static field whose simple name
collides with an enum constant's name on the same nested class may be
getting deduplicated, shadowed, or dropped somewhere in CratonVM's
field-table construction; or `StaticFieldELResolver`'s specific lookup path
(likely `Class.getField(name)` filtered to `Modifier.isStatic` +
`Modifier.isPublic`) sees a different field list than HotSpot does for this
class. No existing known-issue doc covers this signature (checked
`docs/known-issues` and `docs/internal` — no hits).
