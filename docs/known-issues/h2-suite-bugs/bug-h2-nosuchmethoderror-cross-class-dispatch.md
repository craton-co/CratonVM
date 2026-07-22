# NoSuchMethodError citing an unrelated class — `PipedInputStream.flush()V` and `String.create(Z)V` (new instances of the general wrong-receiver-class dispatch bug)

## Status
**OPEN** — new repros of an already-tracked general bug family, 2026-07-21.
`docs/known-issues/README.md` already tracks a "wrong-receiver-type/virtual-dispatch"
class of bug (see e.g. `jit-osr-linux-regression-triad.md` and
`http-client-simpleclienthttpresponsetests-mockito-dispatch-bugs.md`'s
"intermittent `(class, method, descriptor)`-substituting `NoSuchMethodError`;
several hypotheses refuted, root cause still open"). This doc adds two new,
cleanly-isolated H2-suite triggers for that same symptom shape rather than
opening an unrelated bug.

## Severity
**HIGH** — `NoSuchMethodError` for a method that plainly doesn't exist on
the named class (`PipedInputStream` has no `flush()`; `String` has no
`create(boolean)`) is a diagnostic-integrity signal that method resolution
picked the wrong target class somewhere upstream of the error report.

## Affected test classes

**Cluster A -- `PipedInputStream.flush()V`** (6 classes, all real
`java.io.PipedOutputStream`/`PipedInputStream` pairs, all PASS on HotSpot):
`org.h2.test.db.TestLob`, `org.h2.test.jdbc.TestLobApi`,
`org.h2.test.jdbc.TestSQLXML`, `org.h2.test.jdbc.TestUpdatableResultSet`,
`org.h2.test.jdbc.TestResultSet`, `org.h2.test.unit.TestShell` (added
2026-07-22, via `bug-h2-suite-residual-fail-triage.md` -- `org.h2.tools
.Shell.println()`/`.print()` call `out.flush()` where `out` wraps a
`PipedOutputStream`; dispatch resolves it to the nonsensical
`PipedInputStream.flush()V`, logged as a WARN and silently swallowed
rather than propagated, so the Shell tool output is never written to the
pipe and the reading side sees immediate EOF).

```
WARN NoSuchMethodError method="java/io/PipedInputStream.flush()V" caller="java/io/OutputStreamWriter.close()V @pc=7"
WARN NoSuchMethodError method="java/io/PipedInputStream.flush()V" caller="org/h2/jdbc/JdbcLob$LobPipedOutputStream.close()V @pc=4"
```
Two **unrelated** call sites (`OutputStreamWriter.close()`, real JDK
bytecode; and H2's own `JdbcLob$LobPipedOutputStream.close()`, which calls
`super.close()` — i.e. `PipedOutputStream.close()`) both get the identical
bogus target `PipedInputStream.flush()V` — a method that doesn't exist on
`PipedInputStream` in the real JDK at all (`flush()` belongs to
`OutputStream`/`Writer`/`Flushable`, never `InputStream`). `TestLob` fails
outright (`IOException: Pipe not connected`, from the corrupted stream state
after this); the other four surface it as a downstream
`JdbcSQLFeatureNotSupportedException: "Stream setter is not yet closed."`

**Cluster B — `String.create(Z)V`** (3 classes, all PASS on HotSpot):
`org.h2.test.server.TestWeb`, `org.h2.test.unit.TestServlet`,
`org.h2.test.unit.TestJakartaServlet`.
```
WARN NoSuchMethodError method="java/lang/String.create(Z)V" caller="java/net/Socket.getImpl()Ljava/net/SocketImpl; @pc=67"
```
Byte-for-byte identical across all three (same caller, same pc, same bogus
target). `java.lang.String` has no `create(boolean)` method in the real
JDK. Each class's embedded HTTP client subsequently sees
`java.io.EOFException: Unexpected EOF` / `HttpURLConnection response failed:
connection closed before response head` — consistent with the socket setup
being disrupted by whatever this `NoSuchMethodError` actually corrupted or
skipped.

## Analysis
Neither cluster was root-caused down to the exact interpreter/dispatch code
path in this session (time-boxed — see "Status" above for why this is
treated as a rediscovery rather than a fresh investigation). What is
established:
- Both clusters produce **byte-identical** bogus `(class, method,
  descriptor)` triples across multiple, code-wise-unrelated call sites —
  this is a strong signal of a *shared*, deterministic mis-resolution
  (e.g. a stale/overly-coarse method-resolution cache, or a class-agnostic
  fallback path being taken when it shouldn't be — see
  `native-api/src/registry.rs`'s `find_by_method_descriptor`, documented as
  a last-resort, class-blind `(method_name, descriptor)` lookup meant only
  for the "receiver's `class_id_of` reports as 0/`Object`" recovery case),
  not two independent one-off gaps.
- Neither is JIT-specific in the sense of a codegen bug local to one
  method — the caller classes involved (`OutputStreamWriter`,
  `JdbcLob$LobPipedOutputStream`, `Socket`) are all real, unrelated JDK/H2
  classes, and the SAME bogus target recurs regardless of which one calls
  in.
- See also `bug-h2-treemap-tailmap-headmap-view-corruption.md`, whose
  `TreeMap.tailMap()` corruption may be a **silent** (no exception) instance
  of this exact same underlying dispatch defect, just not surfacing as an
  explicit `NoSuchMethodError` because the wrongly-resolved handler in that
  case still returns *some* object rather than failing outright. Kept
  separate pending confirmation.

## Fix direction
Needs a live interpreter trace of one of these call sites (e.g.
`JdbcLob$LobPipedOutputStream.close()`'s `invokespecial` at pc=4, or
`Socket.getImpl()`'s call at pc=67) to see which native-lookup path is
actually taken and why it resolves to the wrong class — genuinely deferred
to a dedicated dispatch-focused investigation, consistent with the existing
open items in `docs/known-issues/README.md` for this bug family.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.jdbc.TestResultSet     # Cluster A
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.unit.TestServlet       # Cluster B
```
