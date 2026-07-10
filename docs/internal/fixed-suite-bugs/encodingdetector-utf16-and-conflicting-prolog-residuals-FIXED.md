# TestEncodingDetector residuals — UTF-16 .jsp decode + BOM/prolog-conflict cases

**Status:** FIXED (2026-07-10). **Severity:** low-medium (was 5/22 params in one class).
**HotSpot:** PASS.

## 2026-07-10 resolution

Both symptom clusters traced back to the SAME root cause: two synthetic
native overrides introduced in `45cc4f4f` (the same commit that fixed the
`FileInputStream` backfill / `defineClass1` duplicate-define blockers this
doc's parent bug depended on) unconditionally shadowed real JDK 25 bytecode
for `java.io.BufferedInputStream` and `java.io.InputStreamReader`:

1. **`BufferedInputStream` mark/reset was a silent no-op.** Its synthetic
   `<init>` correctly allocated `buf`/`count`/`pos`/`markpos`, but
   `read()`/`read([BII)I` delegated straight to the underlying stream via
   `invoke_virtual` without ever touching those fields — so `mark()`/
   `reset()` only updated a `pos` field nothing else consulted. Every
   `mark(N); read...; reset()` consumer was affected, notably
   `EncodingDetector`'s BOM-sniff-then-rewind-then-reread-with-detected-
   encoding sequence: after `reset()` failed to rewind, `getPrologEncoding()`
   read starting mid-XML-declaration instead of at byte 0, so
   `xml_decl_encoding()` never found `"<?xml"` and every prolog-declared
   encoding silently fell back to the BOM-inferred one. This explains
   Cluster A exactly: `bom-utf8-prolog-utf16be.jspx` / `-utf16le.jspx` fell
   back to the (actually-correct) UTF-8 BOM encoding instead of the
   deliberately-wrong declared prolog encoding, so the file parsed fine
   (200) instead of failing (500) the way HotSpot's genuine
   prolog-vs-actual-bytes mismatch does; `bug60769a.jspx` never saw its
   prolog-declared `ISO-8859-1` conflict with the `web.xml`
   `<jsp-property-group><page-encoding>UTF-8</page-encoding>` override
   (`ParserController`'s `jsp.error.prolog_config_encoding_mismatch` check
   never fired because `isEncodingSpecifiedInProlog` was always `false`).

2. **`InputStreamReader.read([CII)I` ignored the charset entirely** — `chars[i]
   = stream.read() & 0xff`, a dumb byte-for-byte passthrough. Every
   multi-byte decode through `InputStreamReader` was corrupted, most visibly
   UTF-16BE/LE: each 2-byte code unit decoded as two separate Latin-1-ish
   characters (a `Utf16ReaderProbe` repro showed `<%-- hi --%>` — 12 chars —
   coming out as 24 garbled chars). This is Cluster B exactly:
   `bom-utf16be-prolog-none.jsp` / `bom-utf16le-prolog-none.jsp` are read via
   `JspUtil.getReader(..., "UTF-16BE"/"UTF-16LE", ...)` →
   `new InputStreamReader(in, encoding)`, which hit this native override.

Root-cause note for anyone hitting this shape again: the interpreter's
`invokevirtual` vtable fast path
(`execute_invokevirtual_vtable_fast` in `vm/src/runtime/interpreter.rs`)
treats "any native registered for this (class, method, descriptor)" as an
authoritative shadow over real bytecode — it does **not** consult the
`NativeKind::SyntheticStub` category the way
`vm_exec.rs::invoke_or_native`'s `real_protected_stub` check does. Tagging a
synthetic override `SyntheticStub` (and even adding the class to
`invoke_or_native`'s hardcoded allowlist) is therefore **not sufficient** to
let real bytecode win for a plain `invokevirtual` call site — only removing
the native registration entirely works, matching the established "Wave2 H2
fix" (`native-io/src/lib.rs`, 2026-05-04) / "RDR-MIGRATION"
(2026-06-01) precedent of letting real JDK 25 bytecode run for these two
classes.

Fix: removed both synthetic override blocks from `native-builtins/src/lib.rs`.
Real bytecode already works correctly — `BufferedInputStream` via
`Unsafe.compareAndSetReference`-based `buf` allocation (verified with a
standalone CAS probe), `InputStreamReader` via `sun.nio.cs.StreamDecoder`
(already correctly registered with full charset support in
`native-io/src/stream_decoder.rs`).

### Verification

`org.apache.jasper.compiler.TestEncodingDetector`: `OK (22 tests)`, matching
HotSpot exactly, reproduced twice (once immediately after the fix, once
again after merging two more `origin/dev` commits before push). Regression
probes `FisReadProbe`/`ReaderReadProbe`/`EcjClassFileReaderProbe` (used by
the `45cc4f4f` author to validate the original `FileInputStream`/classloader
fix) all still pass. `TestPropertiesRoleMappingListener` shows 6 failures
both before and after this fix (confirmed against a stashed pre-fix rebuild)
— pre-existing, unrelated to this change, not investigated further here.

## Original clusters (2026-07-10, superseded above)

### Cluster A — BOM/prolog encoding conflict not producing the expected 500
`testEncodedJsp[7]`/`[8]`/`[20]`: `bom-utf8-prolog-utf16be.jspx`,
`bom-utf8-prolog-utf16le.jspx`, `bug60769a.jspx` returned 200 instead of
HotSpot's 500.

### Cluster B — UTF-16 `.jsp` (standard syntax, no prolog) decode gap
`testEncodedJsp[10]`/`[15]`: `bom-utf16be-prolog-none.jsp` returned a garbled
body instead of `OK`; `bom-utf16le-prolog-none.jsp` timed out.
