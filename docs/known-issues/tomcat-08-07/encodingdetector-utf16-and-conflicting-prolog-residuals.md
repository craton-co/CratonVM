# TestEncodingDetector residuals — UTF-16 .jsp decode + BOM/prolog-conflict cases

**Status:** OPEN. **Severity:** low-medium (5/22 params in one class, all
edge-case encoding combinations). **HotSpot:** PASS.

## Summary

Split off from
[`encodingdetector-jsp-encoding-500-failures-FIXED.md`](../../internal/fixed-suite-bugs/encodingdetector-jsp-encoding-500-failures-FIXED.md)
once that doc's primary defect (StAX prolog-encoding detection) and two
independent blocking VM regressions (FileInputStream backfill,
`defineClass1`-during-webapp-stop) were fixed/merged. With all of that in
place, `org.apache.jasper.compiler.TestEncodingDetector` (real JDK, `-Xmx2g`)
now reports:

```
Tests run: 22,  Failures: 5
```

The 5 residual failures split into two distinct symptom clusters.

## Cluster A — BOM/prolog encoding conflict not producing the expected 500

`testEncodedJsp[7]` (`bom-utf8-prolog-utf16be.jspx`), `[8]`
(`bom-utf8-prolog-utf16le.jspx`), and `[20]` (`bug60769a.jspx`) are
deliberately-crafted fixtures where the file's actual BOM/byte encoding and
its `<?xml ... encoding="...">` prolog declaration contradict each other.
HotSpot treats this as invalid input and the JSP compile fails (500).
CratonVM now returns 200 for all three:

```
java.lang.AssertionError: expected:<500> but was:<200>
```

No server-side `ERROR` is logged for these requests — the JSP compiles and
serves successfully, meaning whatever is supposed to make the
prolog-declared encoding fail against the actual (different) byte encoding
isn't happening. Likely candidates, not yet root-caused:
- `EncodingDetector`'s prolog-wins-over-BOM logic (per its own doc comment,
  "the prolog is always at least as specific as the BOM ... any encoding
  specified in the prolog should take priority") selects the (deliberately
  wrong) prolog encoding, and the resulting `InputStreamReader`/
  `CharsetDecoder` for that encoding is being too lenient about invalid byte
  sequences (replacing instead of throwing `MalformedInputException`),
  producing content that (accidentally) still parses as valid JSP.
- Or the JSPX `JspDocumentParser`/SAX path isn't propagating a decode error
  into a compile failure the way HotSpot's does.

## Cluster B — UTF-16 `.jsp` (standard syntax, no prolog) decode gap

`testEncodedJsp[10]` (`bom-utf16be-prolog-none.jsp`) and `[15]`
(`bom-utf16le-prolog-none.jsp`) are the simplest possible UTF-16 cases: a
BOM, no XML prolog (these are `.jsp`, not `.jspx`, so they go through
`ParserController`'s non-XML/standard-syntax branch, which still
auto-detects via `EncodingDetector` since `isExternal` is false for plain
`.jsp`). Both expect 200 with body `OK`.

- `[10]` returns 200 but with a garbled body:
  ```
  org.junit.ComparisonFailure: expected:<[O]K> but was:<[<   %   -   -      ...
  ```
  The visual shape (ASCII characters interspersed with what looks like NUL
  bytes rendered as spaces/blanks) is consistent with UTF-16 bytes being
  decoded 1-byte-at-a-time as if the encoding were single-byte
  (ISO-8859-1/UTF-8), i.e. the detected `sourceEnc="UTF-16BE"` is not
  actually being honored when `JspReader`/`Parser` re-read the file.
- `[15]` times out entirely (`SocketTimeoutException: Read timed out`) — a
  hang rather than a wrong answer, suggesting the UTF-16LE case hits a
  different (infinite-loop-shaped) code path than UTF-16BE's garbled-output
  case, not just a byte-order mirror of the same bug.

Not yet root-caused. Worth checking first: `JspReader`'s own
`InputStreamReader`/`StreamDecoder` construction for the non-XML
(`.jsp`) syntax path (`ParserController.java` ~line 359,
`new JspReader(ctxt, absFileName, sourceEnc, jar, err)`), specifically
whether it actually threads `sourceEnc` through to a real UTF-16BE/LE
`Charset` decode or silently falls back to something single-byte. The `[15]`
hang additionally suggests a possible infinite-loop in a
`Reader`/`CharsetDecoder` boundary case for UTF-16LE specifically (odd
number of remaining bytes / surrogate handling / BOM-skip interacting badly
with 2-byte character alignment) rather than a straightforward wrong-charset
substitution.

## Reproduction

```bash
ln -s /data/data/apps/tomcat/output/build/conf /data/data/apps/tomcat/conf  # one-time harness fixup, if not already present
cd /data/data/apps/tomcat
cratonvm --java-home /home/victor/jdk25 -Xmx2g \
  -cp "$(cat .suite/cp-linux-fixed.txt)" \
  org.junit.runner.JUnitCore org.apache.jasper.compiler.TestEncodingDetector
```

To isolate a single parameter, patch
`test/org/apache/jasper/compiler/TestEncodingDetector.java`'s
`testEncodedJsp()` to print `responseBody.toString()` on any
`rc != expectedResponseCode`/body mismatch before the assertion — this is
how Cluster A/B's exact failure shapes above were captured (Tomcat's
`ErrorReportValve` HTML body embeds the full root-cause stack trace even
though the server log line truncates it to a one-line summary).
