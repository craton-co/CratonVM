# TestJspDocumentParser — SAXParseException on markup preceding root element — FIXED

**Status:** FIXED. **Severity:** was low-medium. **HotSpot:** PASS.

## Summary

`org.apache.jasper.compiler.TestJspDocumentParser.testBug54801` failed:
```
1) testBug54801(org.apache.jasper.compiler.TestJspDocumentParser)
org.xml.sax.SAXParseException: The markup in the document preceding the root element must be well-formed.
```

The original diagnosis (a Xerces/SAX well-formedness-enforcement difference
from HotSpot) was **wrong**. The real bug had nothing to do with XML parsing:
it was a `FileDescriptor` field-corruption bug in CratonVM's native
`FileInputStream` implementation that broke Jasper's JSP-to-`.java`-source
compilation step. The SAXParseException was a downstream, secondary artifact.

## Root cause

Jasper's `JDTCompiler$1CompilationUnit.getContents()` reads the
JSP-generated `.java` source via:
```java
try (FileInputStream is = new FileInputStream(sourceFile);
        InputStreamReader isr = new InputStreamReader(is, encoding);
        Reader reader = new BufferedReader(isr)) { ... }
```
When this `Reader` chain closes, `InputStreamReader.close()` (real bytecode)
delegates to `sun.nio.cs.StreamDecoder.close()`, which CratonVM implements
natively (`native-io/src/stream_decoder.rs::native_sd_close`). That native
calls back into Java via `ctx.invoke_virtual(is, "close", "()V", &[])` to
close the wrapped `FileInputStream`.

`ctx.invoke_virtual`'s method resolution, unlike the bytecode interpreter's
own `invokevirtual`, can resolve to the *registered native* for
`(FileInputStream, close, ()V)` — `native_fis_close` — instead of real
bytecode, when no prior *ordinary bytecode* call to that method has already
resolved (and cached) real bytecode as the winner for that method. (A plain,
direct `is.close()` call from bytecode always correctly prefers real
bytecode, since `close()` is not force-registered — see
`force_native_over_real_jdk_bytecode` in `vm/src/runtime/interpreter.rs`.
`ctx.invoke_virtual` calls made *from native code*, however, do not
consistently apply that same preference before a cache is warm.)

`native_fis_close` (`native-io/src/lib.rs`) had a leftover line from the
legacy synthetic-layout era:
```rust
ctx.set_field(this, 0, Value::Int(-1));
```
In the real-JDK object layout, slot 0 of `FileInputStream` *is* the real
`fd: Ljava/io/FileDescriptor;` reference field. Writing `Value::Int(-1)`
into a reference-typed slot silently coerces to `Object(None)` on this
heap (the same "primitive write on a reference slot" hazard documented by
the neighboring 2026-05-20 FOS-FIX/FIS-FIX comments in this file) — so this
line unconditionally nulled `this.fd`, even in the real-JDK layout where a
real `FileDescriptor` object already exists and had already been correctly
updated a few lines above.

Once `is.fd` was null, *any* subsequent `close()` call on that same
`FileInputStream` — including the real-bytecode `is.close()` the
try-with-resources block itself calls directly as its third and final
resource-cleanup step — threw:
```
NullPointerException: Cannot invoke "java.io.FileDescriptor.closeAll(java.io.Closeable)" because "this.fd" is null
```
which Jasper's `JDTCompiler$1CompilationUnit.getContents()` catches and logs
as a JSP-compile failure ("Unable to compile class for JSP" /
`org.apache.jasper.JasperException`), which Tomcat serves as an HTTP 500 with
its default HTML error page.

`testSchemaValidation` (same test class) does a `DocumentBuilder.parse(url)`
of a **different** fixture (`valid.jspx`, itself served by the *same* broken
JSP-compile pipeline since `.jspx` is JSP-servlet-mapped) — it received that
same 500 HTML error page instead of the real file content, and correctly
(and unsurprisingly) failed to XML-parse `<!doctype html>...` as well-formed
XML. That failure's exception message and stack (`XMLDocumentScannerImpl
$PrologDriver.next` → `MarkupNotRecognizedInProlog`) is what gave this bug
its original, misleading name and diagnosis — a direct standalone SAX parse
of the real `bug54801a/b.jspx`/`valid.jspx` fixture bytes (with or without a
`LexicalHandler`, with any read-chunking pattern) parses correctly on
CratonVM and always has.

## Fix

`native-io/src/lib.rs::native_fis_close` — only mirror the closed marker into
instance slot 0 when there is **no** real `FileDescriptor` object (i.e. the
legacy synthetic layout), exactly mirroring the guard `fis_set_fd` already
uses:
```rust
if let Some(fd_obj) = fis_fd_object(ctx, this) {
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(-1));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(-1));
} else {
    ctx.set_field(this, 0, Value::Int(-1));
}
```

Fixed+merged dev via branch `fix/jspdocumentparser-sax-20260710-001`.

## Verification

- Isolated repro (no Tomcat): a `try (FileInputStream; InputStreamReader;
  BufferedReader) { ... }` block reading any file reproduced the NPE 100%
  deterministically as the *first* such chain closed in a process (a prior,
  unrelated `FileInputStream` open+close anywhere earlier in the same process
  "fixed" it for later chains — consistent with the cache-warming dispatch
  mechanism above). Passes cleanly after the fix.
- `org.apache.jasper.compiler.TestJspDocumentParser` (real-JDK, JIT on):
  11 failures (`testBug54801`, `testBug54821`, `testSchemaValidation`, and
  six `testDocument_*` valid-fixture cases) → **0 failures, 22/22 PASS**.
- Confirmed NOT a regression: `TestCompiler`/`TestParser` instability seen in
  a wider Jasper-suite spot-check reproduces identically on a pre-fix binary
  (same commit, no fix applied) — pre-existing host-load flakiness unrelated
  to this change, not investigated further here.

## Note on a related, separate StreamDecoder bug

While investigating, a **second, independent** field-layout bug in
`native-io/src/stream_decoder.rs::alloc_stream_decoder` was found (real slot
0 is `StreamDecoder`'s own `closed` field, not `in` as the old
`SD_INPUT`/`SD_ID` constants assumed) — already fixed separately and
concurrently on `dev` by commit `e5f20c9bf` (`fix(native-io): StreamDecoder
field-index mismatch corrupting adjacent object`, merged via
`395f7246a`). That is a distinct corruption from the one this doc covers
(it corrupts the `StreamDecoder`'s own fields, not the wrapped
`FileInputStream`'s) and did not need any further change here.
