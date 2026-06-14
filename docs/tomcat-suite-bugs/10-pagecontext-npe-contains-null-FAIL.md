# Bug 10 — JSP PageContext testBug49196 NPE  (FAIL)

**Status:** OPEN. Real CratonVM bug (HotSpot PASSes).
**Severity:** Low/Medium. **Repro class:**
`jakarta.servlet.jsp.TestPageContext` — `Tests run: 1, Failures: 1`.

## Symptom

```
java.lang.NullPointerException: Cannot invoke contains on null
  at jakarta.servlet.jsp.TestPageContext.testBug49196(TestPageContext.java:34)
```

A `.contains(...)` call on a null reference at the test's line 34. The test
(`testBug49196`) exercises a `PageContext` / EL scenario; some object that should
be non-null (a collection, String, or a `PageContext`-derived value) is null
under CratonVM.

## Root cause (hypothesis)

A CratonVM API returns null where the JSP `PageContext` / EL path expects a
populated value (the test then calls `.contains` on it). Could be an EL
evaluation, a scoped-attribute lookup, or a `PageContext` accessor returning null
under CratonVM. Needs the test source (line 34) to identify which call yields the
null. Likely related to the other EL/JSP FAILs (07, 08) — a shared EL/introspection
gap.

## Next steps

- Read `TestPageContext.java:34` (`testBug49196`) to find the null source.
- Cross-check with bugs 07/08 (EL resolver / ImportHandler) — may be one root
  cause in the EL evaluation path.

## Reproduction

```
cratonvm.exe -Xmx2g -cp <cp> org.junit.runner.JUnitCore \
  jakarta.servlet.jsp.TestPageContext   # CWD: apps/tomcat
# -> Tests run: 1, Failures: 1 ; HotSpot: PASS
```
