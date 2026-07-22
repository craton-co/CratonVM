# `java.io.DataOutputStream.writeChars(String)` silently wrote zero bytes — no exception, total data loss

## Status
**FIXED** — 2026-07-22, `dev` (same branch as
`bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md`,
`fix/h2-nsme-crossclass-dispatch-20260722`). Originally filed as a same-day
follow-up discovery while root-causing that doc's Cluster B residual; fully
root-caused and fixed later the same day. NOT H2-specific and NOT the same
mechanism as that doc's two clusters — a plain `java.io.DataOutputStream`
correctness bug, reproducible with no H2, sockets, or networking at all.

## Severity
**HIGH** — silent, no-exception data loss on a widely-used core `java.io`
API. Any code that calls `DataOutputStream.writeChars(String)` (H2's own
wire protocol does, for every string field in its TCP handshake) lost that
data entirely with no error signal, corrupting whatever byte stream
followed.

## Symptom
```java
ByteArrayOutputStream bos = new ByteArrayOutputStream();
DataOutputStream dos = new DataOutputStream(bos);
dos.writeChars("abc");
dos.flush();
System.out.println(bos.toByteArray().length);
```
- **HotSpot JDK 25**: prints `6` (3 chars × 2 bytes, UTF-16BE).
- **CratonVM before this fix** (`--java-home /home/victor/jdk25`, with or
  without `--nojit`): prints `0`. No exception, no WARN log, no diagnostic
  of any kind.
- **After this fix**: prints `6`, byte-identical to HotSpot
  (`0 97 0 98 0 99`).

## Downstream impact (how this was found)
H2's wire protocol (`org.h2.value.Transfer.writeString`) uses exactly this
call (`out.writeInt(s.length()); out.writeChars(s);`) for every string field
in its TCP handshake (database name, original URL, etc.). Because
`writeChars` wrote nothing, the length prefix was written but the character
data wasn't — every string field after the first was invisibly truncated to
zero content while the length prefix on the wire still claimed real content
followed. The server-side reader (`Transfer.readString`) then read a
**different, later** field's raw bytes as if they were this field's
misaligned length prefix, eventually landing on a garbage huge value and
throwing `java.lang.OutOfMemoryError: Requested array size exceeds VM limit`
attempting `new StringBuilder(garbageLen)`. The client saw the connection
close and reported a bare `EOFException: Unexpected EOF` with no indication
of the real cause — only visible with H2 server-side tracing explicitly
enabled (`Server.createTcpServer(..., "-trace")`), which the H2 test suite
does not do by default, so this looked outwardly like a socket/networking
bug until isolated. Confirmed via a standalone (no H2 suite) `-trace`-enabled
repro: `org.h2.server.TcpServerThread.run` at
`TcpServerThread.java:119` (`String db = transfer.readString();`).

## Root cause — CONFIRMED (this doc's original hypothesis was wrong about the bytecode shape)

`java/io/DataOutputStream` (`native-io/src/lib.rs:8448`) has native
overrides for `write(I)V`, `write([BII)V`, `writeBoolean`, `writeByte`,
`writeShort`, `writeChar(I)V` (singular), `writeInt`, `writeLong`,
`writeFloat`, `writeDouble`, `flush`, `close`, `size` — but no override for
`writeChars(Ljava/lang/String;)V`, so it falls through to real JDK 25
bytecode. **The original investigation pass assumed the classic JDK 8-era
implementation** (`out.write((v>>>8)&0xFF); out.write(v&0xFF);` in a loop)
**and wrote a repro against that assumption that happened to still exercise
the bug, but for the wrong reason** — a custom class with that exact old
shape, tested side-by-side against the real class, worked fine, proving the
bug wasn't in the general "inherited-field-receiver loop with 2
`invokevirtual` calls per iteration" pattern.

`javap -c` on the ACTUAL JDK 25 `DataOutputStream.class` shows a materially
different, modernized implementation:
```java
public final void writeChars(String s) throws IOException {
    int len = s.length();
    for (int i = 0; i < len; i++) {
        int v = s.charAt(i);
        ByteArray.setUnsignedShort(writeBuffer, 0, v);  // jdk.internal.util.ByteArray
        out.write(writeBuffer, 0, 2);                    // bulk write([BII)V, not write(I)V
    }
    incCount(len * 2);
}
```
This depends on `private final byte[] writeBuffer`, a field JDK 25 added as
a scratch buffer shared by `writeShort`/`writeChar`/`writeInt`/`writeLong`/
`writeChars` (confirmed via the constructor's bytecode: `writeBuffer = new
byte[8]`, allocated unconditionally in `DataOutputStream(OutputStream)`).
**`native_dos_init`** (`native-io/src/lib.rs:9244`) **entirely replaces**
the real constructor (it's the native override for `<init>`) and never
allocated this field — `writeBuffer` stayed `null` on every CratonVM
`DataOutputStream` instance. Since all the OTHER methods that read
`writeBuffer` in real JDK (`writeShort`, `writeChar`, `writeInt`,
`writeLong`) are ALSO natively overridden here and never touch the real
field, `writeBuffer`'s absence was completely invisible until real bytecode
for the one unregistered method (`writeChars`) ran and read it back as
`null`. Passing that `null` array into `ByteArray.setUnsignedShort(null, 0,
v)` and then `out.write(null, 0, 2)` produced **silent no-ops with no
exception** under this VM rather than a `NullPointerException` — the exact
mechanism for why nothing threw wasn't pinned down further (whether the
array-store or the `write([BII)V` call is the one swallowing it), but the
fix eliminates the null in the first place, which is sufficient and lower-risk
than chasing that secondary question.

## Fix
`native_dos_init` now also allocates `writeBuffer = new byte[8]` (via
`ctx.new_array(ArrayElementType::Byte, 8)`) and seeds it by name
(`set_field_by_name`, which is a safe no-op if the field doesn't exist —
e.g. under `--synthetic-jdk`), exactly matching what the real constructor
does. This fixes `writeChars` and, as a side effect, makes `writeUTF`'s
real-bytecode helper (also unregistered, also potentially touching
`writeBuffer` on some JDK versions) more robust too, without needing a
dedicated native override for either method.

## Verification (2026-07-22, Azure Linux host, real-JDK mode)
- Standalone repro: `bos.toByteArray().length` now `6`, bytes `0 97 0 98 0
  99` — byte-identical to HotSpot.
- Standalone H2-TCP-protocol repro (`Server.createTcpServer(...,
  "-trace")` + a JDBC connect): the handshake now completes correctly (the
  server reads the real database name/URL and returns a well-formed,
  correct SQL exception for a nonexistent database, instead of crashing
  with `OutOfMemoryError` on a garbage length).
- `org.h2.test.unit.TestServlet` and `org.h2.test.unit.TestJakartaServlet`
  (2 of `bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md`'s 3
  Cluster B classes): now **fully pass** (were failing on this bug's
  downstream `OutOfMemoryError`/`EOFException` even after that doc's own
  fix landed).
- `org.h2.test.server.TestWeb` (the 3rd Cluster B class): still fails, but
  now via a **different, confirmed-unrelated** symptom — an
  `IOException: HttpURLConnection response failed: connection closed
  before response head` from `WebClient.get("logout.do")`, where H2's own
  test expects a `ConnectException` specifically (a connect-time-refused
  vs. mid-response-reset distinction). Confirmed via `CRATONVM_DBG_SOCK`
  trace that this class's HTTP request/response cycle otherwise works
  correctly (multiple prior `GET`s in the same run return proper `200 OK`
  responses) and that H2's WebServer path never calls `writeChars`
  anywhere. Not filed as its own doc yet — noted here and in the parent
  doc's residuals section for whoever picks it up next.
- Regression spot-check (classes unrelated to `DataOutputStream`):
  `TestLobApi`, `TestSQLXML`, `TestUpdatableResultSet`, `TestStatement`,
  `TestView` — all clean, zero `NoSuchMethodError`/panics, same as before
  this fix (this change is purely additive — it seeds a field that was
  previously always `null`, matching what real bytecode already expected,
  so it cannot change behavior for anything that was working before).

## Repro
```java
// No H2, no sockets, no CratonVM-specific setup needed:
import java.io.*;
public class WriteCharsRepro {
    public static void main(String[] args) throws Exception {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        DataOutputStream dos = new DataOutputStream(bos);
        dos.writeChars("abc");
        dos.flush();
        System.out.println(bos.toByteArray().length); // HotSpot and now CratonVM: 6
    }
}
```

## Related
- `docs/internal/h2-suite-bugs/bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md` — the doc whose Cluster B residual investigation found this.
- `native-io/src/lib.rs:9244` (`native_dos_init`) — the fix site.
- The `WebServer`/`ConnectException` residual on `TestWeb` noted above is a genuinely separate, still-open issue.
