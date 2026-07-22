# `java.io.DataOutputStream.writeChars(String)` silently writes zero bytes — no exception, total data loss

## Status
**OPEN** — new finding, 2026-07-22, discovered while root-causing
`bug-h2-nosuchmethoderror-cross-class-dispatch.md`'s Cluster B residual
(`TestWeb`/`TestServlet`/`TestJakartaServlet` still fail after that doc's fix
lands, now via `OutOfMemoryError` instead of `NoSuchMethodError`). NOT
H2-specific and NOT related to that doc's root cause (no native object
field-layout collision involved) — a plain `java.io.DataOutputStream`
correctness bug, reproducible with no H2, sockets, or networking at all.

## Severity
**HIGH** — silent, no-exception data loss on a widely-used core `java.io`
API. Any code that calls `DataOutputStream.writeChars(String)` (H2's own
wire protocol does, for every string field in its TCP handshake) loses that
data entirely with no error signal, corrupting whatever byte stream follows.

## Symptom
```java
ByteArrayOutputStream bos = new ByteArrayOutputStream();
DataOutputStream dos = new DataOutputStream(bos);
dos.writeChars("abc");
dos.flush();
System.out.println(bos.toByteArray().length);
```
- **HotSpot JDK 25**: prints `6` (3 chars × 2 bytes, UTF-16BE).
- **CratonVM** (`--java-home /home/victor/jdk25`, with or without `--nojit`):
  prints `0`. No exception, no WARN log, no diagnostic of any kind — the
  method call is a complete no-op.

Isolated to `writeChars` (String-arg) specifically — confirmed all siblings
work correctly on the same stream:
- `dos.writeInt(...)` × N: correct bytes.
- `dos.writeChar(int)` (singular) in an explicit loop over
  `s.charAt(i)`: correct bytes.
- A hand-written loop calling `bos.write(...)` directly (bypassing
  `DataOutputStream` entirely) with the identical shift/mask logic
  `writeChars`'s real JDK bytecode uses: correct bytes.

So the bug is specific to `DataOutputStream.writeChars(String)`'s own
bytecode execution, not `String.charAt`/loop mechanics in general, not the
underlying stream, and not `DataOutputStream`'s other write methods.

## Downstream impact (how this was found)
H2's wire protocol (`org.h2.value.Transfer.writeString`) uses exactly this
call (`out.writeInt(s.length()); out.writeChars(s);`) for every string field
in its TCP handshake (database name, original URL, etc.). Because
`writeChars` writes nothing, the length prefix is written but the character
data isn't — every string field after the first is invisibly truncated to
zero content while the length prefix on the wire still claims real content
followed. The server-side reader (`Transfer.readString`) then reads a
**different, later** field's raw bytes as if they were this field's
misaligned length prefix, eventually landing on a garbage huge value and
throwing `java.lang.OutOfMemoryError: Requested array size exceeds VM limit`
attempting `new StringBuilder(garbageLen)`. The client sees the connection
close and reports a bare `EOFException: Unexpected EOF` with no indication
of the real cause — only visible with H2 server-side tracing explicitly
enabled (`Server.createTcpServer(..., "-trace")`), which the H2 test suite
does not do by default, so this looked outwardly like a socket/networking
bug until isolated. Confirmed via a standalone (no H2 suite) `-trace`-enabled
repro: `org.h2.server.TcpServerThread.run` at
`TcpServerThread.java:119` (`String db = transfer.readString();`).

## Root cause (narrowed, not fully pinned)
`java/io/DataOutputStream` (`native-io/src/lib.rs:8448`,
`register_re1_*`-style registration block) has native overrides for
`write(I)V`, `write([BII)V`, `writeBoolean`, `writeByte`, `writeShort`,
`writeChar(I)V` (singular — reuses `native_dos_write_short`), `writeInt`,
`writeLong`, `writeFloat`, `writeDouble`, `flush`, `close`, `size` — but
**no override for `writeChars(Ljava/lang/String;)V`** (confirmed via
full-crate grep, only two other `"writeChars"` registrations exist and both
target unrelated classes: `java/io/ObjectOutputStream` in
`serialization.rs` and `RandomAccessFile` in `phases_late.rs`). So real JDK
bytecode should run for `DataOutputStream.writeChars`:
```java
public final void writeChars(String s) throws IOException {
    int len = s.length();
    for (int i = 0 ; i < len ; i++) {
        int v = s.charAt(i);
        out.write((v >>> 8) & 0xFF);
        out.write((v >>> 0) & 0xFF);
    }
    incCount(len * 2);
}
```
This reads `this.out` via `getfield` — `native_dos_init`
(`native-io/src/lib.rs:9244`) writes the constructor's inner stream to a
**hardcoded** `DOS_FIELD_OUT = 0`, but (unlike the `SOCK_HOST`/`impl`
collision this session fixed elsewhere) this one is NOT a layout
mismatch: real JDK's `DataOutputStream` extends `FilterOutputStream`, whose
sole field `out` genuinely is index 0, confirmed indirectly by every other
native (`write`, `writeInt`, etc., all reading the same `DOS_FIELD_OUT`
slot) working correctly on the exact same object. So `this.out` should read
back the correct, connected `OutputStream` for `writeChars`'s real bytecode
too — yet nothing is written and no exception is raised, with `--nojit`
ruling out a JIT-codegen-specific cause. The defect is somewhere in how the
interpreter executes this specific method's bytecode (a loop containing two
sequential `invokevirtual out.write(I)V` calls per iteration, operating on
an inherited-field receiver) — not chased further to an exact
interpreter/bytecode-index in this session.

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
        System.out.println(bos.toByteArray().length); // expect 6, got 0
    }
}
```
H2 integration repro (once this is fixed, re-verify Cluster B of
`bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md` — expect all 3
classes to fully pass, not just lose their `NoSuchMethodError`):
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.unit.TestServlet
```

## Related
- `docs/internal/h2-suite-bugs/bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md` — the doc whose Cluster B residual investigation found this.
- `native-io/src/lib.rs:8448` (`java/io/DataOutputStream` native registration block) — where the missing `writeChars` override needs to be added, or where the underlying interpreter defect this method's bytecode hits needs to be found.
