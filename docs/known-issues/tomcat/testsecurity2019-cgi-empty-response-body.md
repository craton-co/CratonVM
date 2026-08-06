# `TestSecurity2019.testCVE_2019_0232` — CGI script response body comes back empty

| | |
|---|---|
| **Status** | OPEN |
| **Severity** | medium — single test, but points at a CGI/process-execution gap |
| **HotSpot** | PASS (`OK (3)`, fresh-verified 2026-08-06) |
| **CratonVM** | FAIL, reproduces |
| **Discovered** | 2026-08-06, complete 651-class Tomcat suite rerun |

## Symptom

```
1) testCVE_2019_0232(org.apache.tomcat.security.TestSecurity2019)
java.lang.NullPointerException: Cannot invoke "String.contains(java.lang.CharSequence)"
	at org.apache.tomcat.security.TestSecurity2019.testCVE_2019_0232(TestSecurity2019.java:178)
```

Line 178 is `Assert.assertTrue(res.toString().contains("Query string:"));`,
immediately after `Assert.assertEquals(HttpServletResponse.SC_OK, rc)` at
line 177 — so the HTTP request to the CGI-backed servlet (`CGIServlet`
running `test.bat` on this Windows fixture) returned `200 OK`, but the
response body (`res`, a `ByteChunk`) is empty, so `res.toString()` returns
`null` and the very next call NPEs.

## Ruled out: this is not a `ByteChunk` null-vs-empty semantics bug

Doc [`25-charchunk-tostring-null-vs-empty-FIXED.md`](../../internal/fixed-suite-bugs/tomcat/25-charchunk-tostring-null-vs-empty-FIXED.md)
fixed exactly this shape of bug for the sibling `CharChunk` class, so that
was the first hypothesis here. Directly probed instead of assumed:

```java
ByteChunk bc = new ByteChunk();
System.out.println(bc.toString());   // HotSpot: null
```

**HotSpot also returns `null`** from `ByteChunk.toString()` on an
empty/unwritten chunk — this is `ByteChunk`'s actual designed behavior
(unlike `CharChunk`, which was supposed to return `""`). So CratonVM's `null`
here is correct; the real defect is upstream — the CGI response body should
have had content, and doesn't.

## Suspected root cause (not yet isolated)

The test starts a `CGIServlet` and executes `test.bat` (Windows) as an
external process via `enableCmdLineArguments`/`cgiPathPrefix`, expecting the
script's own stdout (which echoes `Query string: ...`) to become the HTTP
response body. `rc == 200` proves the servlet believed the CGI process ran
successfully, but the body is empty — pointing at CratonVM's CGI output
capture (stdout piping from the child `cmd.exe`/batch-script process) rather
than the HTTP layer. Not yet checked against `native-io`'s `Process`/
`ProcessBuilder` implementation.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore org.apache.tomcat.security.TestSecurity2019
```

HotSpot control: `OK (3 tests)` in 0.7s.
