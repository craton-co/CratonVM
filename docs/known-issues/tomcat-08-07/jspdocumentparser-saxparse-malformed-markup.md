# TestJspDocumentParser — SAXParseException on markup preceding root element

**Status:** OPEN. **Severity:** low-medium. **HotSpot:** PASS.

## Summary

`org.apache.jasper.compiler.TestJspDocumentParser.testBug54801` fails:
```
1) testBug54801(org.apache.jasper.compiler.TestJspDocumentParser)
org.xml.sax.SAXParseException: The markup in the document preceding the root element must be well-formed.
```
This is a regression test for a historical Jasper bug (Bug 54801) about
JSP-XML-syntax documents ("JSP documents", `.jspx` or XML-syntax JSP)
tolerating specific markup patterns before the root element. On CratonVM,
the SAX parser used by Jasper's `JspDocumentParser` rejects markup that
HotSpot's equivalent accepts — meaning some XML/SAX parser-configuration or
entity-resolution difference is causing stricter-than-expected well-
formedness enforcement.

This is adjacent to, but distinct from, the Eclipse JDT parser family
covered in
[jasper-jdt-parser-arrayindexoutofbounds.md](../jasper-jdt-parser-arrayindexoutofbounds.md)
(now FIXED as of 2026-07-08) — `TestJspDocumentParser` exercises the XML/SAX
JSP-document parsing path, not the JDT Java-source parser, so it is a
separate code path and was not resolved by that fix.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName jspdocparse `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.jasper.compiler.TestJspDocumentParser
```

## Recommendation

Locate the `testBug54801` fixture (likely a `.jspx` test resource with
some specific pre-root-element content — comment, PI, or DOCTYPE-adjacent
markup) and trace which SAX/XML parser factory CratonVM's JDK resolves for
`javax.xml.parsers.SAXParserFactory` (or whatever Jasper's
`JspDocumentParser` uses internally). Compare parser feature flags
(namespace-awareness, validating, external-entity handling) between
CratonVM's real-JDK XML stack and HotSpot's default — a differing default
for one of these flags would explain stricter well-formedness rejection.

## 2026-07-09 worker evidence

The local `C:\craton\CratonVM-tomcat-0807-fixture-20260709-001` worktree does
not contain `apps\tomcat-suite-runner`, so the class-level Tomcat repro could
not be rerun here. Upstream Tomcat 9.0.83 shows `testBug54801` requests:

- `/test/bug5nnnn/bug54801a.jspx`
- `/test/bug5nnnn/bug54801b.jspx`

Both fixtures are normal JSP documents with an XML declaration, a leading XML
comment before `<jsp:root>`, and a scriptlet body containing the literal
`${foo}`. `bug54801a.jspx` wraps the scriptlet body in CDATA; `bug54801b.jspx`
does not. HotSpot accepts both.

The relevant Tomcat code is
`org.apache.jasper.compiler.JspDocumentParser.getSAXParser`: it calls
`SAXParserFactory.newInstance()`, sets namespace-aware true, enables
`http://xml.org/sax/features/namespace-prefixes`, optionally enables
validation/schema features, then calls `factory.newSAXParser()` and
`saxParser.getXMLReader()`. CratonVM already has a documented guard in
`native-builtins/src/lib.rs` keeping the old synthetic
`register_p68_xml`/`SAXParserFactory.newSAXParser()` bridge out of real-JDK
startup because it returned a bare synthetic parser without a real
`getXMLReader()` and previously regressed Tomcat startup. Do not re-enable that
bridge as the fix.

No code change in this pass targeted this SAX path. Keep this note open until
`org.apache.jasper.compiler.TestJspDocumentParser.testBug54801` is rerun with
server-side logs and either passes or produces a narrowed CratonVM-side
exception/backtrace.
