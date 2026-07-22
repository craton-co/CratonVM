# NoSuchMethodError citing an unrelated class — `PipedInputStream.flush()V` (FIXED) and `String.create(Z)V` (still OPEN)

## Status
**Cluster A FIXED** (dev@`fix/h2-nsme-crossclass-dispatch-20260722`,
2026-07-22) — root-caused to a synthetic-vs-real-JDK field-layout drift, NOT
a class-dispatch/method-resolution bug. **Cluster B still OPEN** — not
root-caused this session.

**Correction to this doc's original framing:** the two clusters below do
**not** share a root cause, and neither is the same defect as
`bug-h2-treemap-tailmap-headmap-view-corruption.md`'s submap-view corruption
(that doc originally speculated the TreeMap bug "may be a silent instance of
this exact same underlying dispatch defect" — confirmed false: the TreeMap
bug is a fast-mode/array-mode data-source bug entirely internal to the
synthetic `TreeMap`, with no involvement of method dispatch, class
resolution, or `NativeMethodRegistry` at all). Kept as one doc for the two
NoSuchMethodError clusters since they were reported together, but they are
two independent findings.

## Cluster A — `PipedInputStream.flush()V` — FIXED

### Root cause (confirmed)
`java/io/PipedInputStream`/`java/io/PipedOutputStream` are registered in
`native-io/src/lib.rs` as a "simplified as ByteArrayI/O pair" synthetic
implementation, reusing the same 4-slot `BufferedInputStream`/
`BufferedOutputStream`-shaped natives (`native_bis_read`, `native_bos_write`,
`native_bos_flush`, ...) — i.e. slot 0 is hardcoded to mean "the delegate
stream this buffers for". On a **real** JDK 25 `PipedOutputStream` (whose
only field is `sink`, a reference to the connected `PipedInputStream`) and
`PipedInputStream`, that slot-0 assumption is wrong: `native_bos_flush`/
`native_bos_write` read slot 0 expecting an `OutputStream`-like delegate to
forward to, but on the real layout slot 0 (`sink`) is the connected
`PipedInputStream` — so `flush()`/`write()`/`close()` end up doing
`invoke_virtual(sink, "flush", "()V", ...)` on a `PipedInputStream` instance,
which declares no such method, producing the observed
`NoSuchMethodError method="java/io/PipedInputStream.flush()V"` regardless of
which real call site (`OutputStreamWriter.close()`, H2's own
`JdbcLob$LobPipedOutputStream.close()`) triggered it — explaining why the
error is byte-identical across unrelated callers (same broken synthetic
native, not a shared dispatch defect).

Same bug family as the already-fixed `StringReader`/`EnumSet`/`Pattern`/
`Matcher`/`StringJoiner`/`Cleaner` synthetic-layout drops in
`native-api/src/registry.rs`'s `drop_real_layout_synthetic` real-JDK-mode
gate (see `docs/synthetic-vs-real-explained.md`) — real
`PipedInputStream`/`PipedOutputStream` bytecode is self-contained (a
synchronized circular buffer with `wait`/`notifyAll` and thread-identity
checks; no missing native dependency), so the fix drops the synthetic
surface in real-JDK mode and lets real bytecode run, exactly like those
siblings.

### Fix
`native-api/src/registry.rs`: added `java/io/PipedInputStream` and
`java/io/PipedOutputStream` to the `drop_real_layout_synthetic` gate in
`register()`, alongside the existing `StringReader`/`EnumSet`/`Pattern`/
`Matcher` entries.

### Verification
Ran the 5 originally-affected classes against a real-JDK-mode build with the
fix:
- `org.h2.test.jdbc.TestLobApi` — PASS (was failing on this NoSuchMethodError)
- `org.h2.test.jdbc.TestSQLXML` — PASS
- `org.h2.test.jdbc.TestUpdatableResultSet` — PASS
- `org.h2.test.db.TestLob` — no longer hits `PipedInputStream.flush()V` (grepped
  clean), but times out on an unrelated, pre-existing "STW cross-thread JIT
  takeover ... waiting for cooperative mutators" stall — a different
  subsystem, not chased further here.
- `org.h2.test.jdbc.TestResultSet` — no longer hits `PipedInputStream.flush()V`
  (grepped clean), but fails on an unrelated, pre-existing
  `testDatetimeWithCalendar` DST/`Calendar`-offset assertion
  (`Expected: ...10:11:12... actual: ...09:11:12...`) — a different bug,
  not chased further here.

## Cluster B — `String.create(Z)V` — still OPEN

Not root-caused this session. Ruled out: no native is registered anywhere in
the tree under class `java/lang/String` method `create` descriptor `(Z)V` —
so this is not a simple wrong registration shadowing `String`, and (per the
TreeMap doc's correction above) `find_by_method_descriptor` is dead code with
zero call sites, so it cannot be the mechanism either. The bogus
`(class, method, descriptor)` triple is byte-identical across all three
call sites (`TestWeb`, `TestServlet`, `TestJakartaServlet`), all originating
from `Socket.getImpl()`'s internal `SocketImpl.create(boolean)` call — real
`SocketImpl` subclasses (`PlainSocketImpl`/`NioSocketImpl`) do declare a
package-private `create(boolean)`, so this looks like a genuine
method-resolution/vtable bug picking the wrong target class during that
`invokevirtual`, rather than a registry issue — needs a live interpreter
trace of `Socket.getImpl()`'s dispatch to confirm, same as this doc
previously recommended. Still deferred to a dedicated dispatch-focused
investigation.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.jdbc.TestLobApi       # Cluster A (now passes)
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.unit.TestServlet      # Cluster B (still open)
```
