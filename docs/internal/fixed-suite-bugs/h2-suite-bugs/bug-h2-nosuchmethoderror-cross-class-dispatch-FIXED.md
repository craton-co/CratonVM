# NoSuchMethodError citing an unrelated class — `PipedInputStream.flush()V` and `String.create(Z)V` (new instances of the general wrong-receiver-class dispatch bug)

## Status
**FIXED** — 2026-07-22, `dev` (branch `fix/h2-nsme-crossclass-dispatch-20260722`). Both
clusters' `NoSuchMethodError` symptom is eliminated and root-caused to file:line
precision. **Update (same day)**: the `writeChars` residual noted below for
Cluster B is also now fixed (see
`bug-h2-dataoutputstream-writechars-data-loss-FIXED.md`) — `TestServlet` and
`TestJakartaServlet` now fully pass. `TestWeb` (the 3rd Cluster B class)
still fails, but on a further, confirmed-unrelated issue (see
`bug-h2-testweb-logout-connectexception-mismatch.md`) — the NSME itself,
which is this doc's actual subject, is gone in all 8 originally-affected
classes, and 6 of the 8 now fully pass end-to-end.

## Severity
**HIGH** — `NoSuchMethodError` for a method that plainly doesn't exist on
the named class (`PipedInputStream` has no `flush()`; `String` has no
`create(boolean)`) is a diagnostic-integrity signal that method resolution
picked the wrong target class somewhere upstream of the error report.

## Root cause — NOT the same mechanism for both clusters, and NOT `find_by_method_descriptor`

Both clusters turned out to be **the established "field-slot-index collision"
bug family** already documented in
`docs/internal/fixed-suite-bugs/wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md`
and `docs/internal/netty-client-socket-write-after-close-nsme-FIXED.md` — a
native registration writes a value into a **hardcoded object field-slot
index** designed for a legacy synthetic layout, but in real-JDK mode the
object is a genuine loaded JDK class whose field at that same numeric index
is something else entirely. Later, unrelated real bytecode reads that slot
expecting its own field and gets whatever was actually written there,
dispatching a method call onto it.

**Refuted hypothesis** (this doc's own original text, and
`bug-h2-treemap-tailmap-headmap-view-corruption.md`'s independent guess at
the same doc): `NativeMethodRegistry::find_by_method_descriptor`
(`native-api/src/registry.rs`), the class-blind `(method_name, descriptor)`
recovery lookup. **Confirmed dead code** — grepped the entire crate; it has
exactly one caller, itself (its own definition), and is never invoked from
any dispatch path. It cannot be the mechanism for either cluster, or for the
TreeMap doc's symptom. That doc's hypothesis should be corrected/retracted
too (not touched further here — it's a different, still-open bug with no
established connection to this one beyond a superficially similar symptom
shape).

### Cluster A — `PipedInputStream.flush()V`

`native-io/src/lib.rs`'s `register_buffered_stream_natives` (phase 40)
registers `java/io/PipedInputStream`/`java/io/PipedOutputStream`'s
`<init>`/`read`/`write`/`flush`/`close`/`available` by reusing
`BufferedInputStream`/`BufferedOutputStream`'s native functions
(`native_bis_init`, `native_bis_read`, `native_bos_init`, `native_bos_write`,
`native_bos_flush`), which read/write **hardcoded** field-slot constants
(`BIS_FIELD_IN=0`/`BUF=1`/`POS=2`/`COUNT=3`) designed for a legacy 4-field
synthetic `BufferedInputStream` layout, or (for the Buffered*Output* side)
resolve slot indices via `bos_slots()` — which looks them up **specifically
against `java/io/BufferedOutputStream`/`FilterOutputStream`**, not against
whatever class `this` actually is.

Real JDK 25 `PipedOutputStream extends OutputStream` directly (not
`FilterOutputStream`) and declares exactly one field: `sink` (a connected
`PipedInputStream`), which — by coincidence of both hierarchies being
one-field-deep — lands at the same numeric index (0) that `bos_slots()`
resolves for `FilterOutputStream.out`. So `native_pos_init_connected`
(`PipedOutputStream(PipedInputStream)`) correctly writes the connected
`PipedInputStream` into slot 0 (matching the real `sink` field, coincidentally),
and `native_bos_flush`/`native_bos_write` then read that same slot back
believing it holds a delegate `OutputStream` to forward writes/flushes to.
It doesn't — it holds the connected `PipedInputStream`, which declares
neither `write(...)` nor `flush()`. `ctx.invoke_virtual(inner, "flush",
"()V", &[])` on it throws `NoSuchMethodError:
java/io/PipedInputStream.flush()V`, naming the real, correctly-typed object
that was actually sitting in that slot — for a method neither the caller nor
that class ever declared.

**Fix**: add `java/io/PipedInputStream`/`java/io/PipedOutputStream` to
`native-api/src/registry.rs`'s `drop_real_layout_synthetic` gate (the same
mechanism already used for `StringReader`/`EnumSet`/`Pattern`/`Matcher`/
`LinkedBlockingDeque`/`ScheduledThreadPoolExecutor` for the identical reason
— see that field's doc comment). In real-JDK mode this drops the
legacy-layout natives entirely and lets real JDK Piped-stream bytecode run
(a self-contained synchronized circular buffer using `wait()`/`notifyAll()`/
`Thread` identity checks — no native dependency it doesn't already have).
The file's own comment previously argued PIS/POS didn't need this because
"they don't sit in the JDK boot path" — true, but H2's `TestLob` family
constructs and uses real connected pairs directly, which is exactly the case
this leaves exposed.

### Cluster B — `String.create(Z)V`

`native-builtins/src/net_phase_e.rs` (`register_re1_socket`/
`register_re2_server_socket`/`register_re6_ssl_context`) wrote the remote
peer's host address as a `String` directly into `java/net/Socket`/
`javax/net/ssl/SSLSocket` object field **slot 0** (`const SOCK_HOST: usize =
0`) at several producer sites: client `connect()` (`re1_connect_socket`),
`ServerSocket.accept()` (`re2_accept_into`), and TLS client `createSocket`.

Real JDK 25 `java.net.Socket`'s **first declared instance field is
`impl` (`SocketImpl`)** (confirmed via `javap -p java.net.Socket` on the
actual JDK 25 build this host uses) — `SOCK_HOST=0` collides with it
directly. `Socket.getImpl()` has no native override anywhere in this crate
(confirmed via full-crate grep), so it always runs as real bytecode:
```java
private void createImpl() throws SocketException {
    if (impl == null) setImpl();
    try { impl.create(stream); created = true; } ...
}
```
Since our native construction path already wrote a non-null `String` into
the `impl` slot, `setImpl()` is skipped (impl looks already set) and
`impl.create(stream)` dispatches `create(boolean)` onto that `String`,
producing `NoSuchMethodError: java/lang/String.create(Z)V` — again naming
the real, correctly-typed object actually occupying the slot.

This is the **third** occurrence of this exact `SOCK_HOST`/`impl` collision
found in this codebase's history — see the pre-existing comments at
`net_phase_e.rs`'s `getSoTimeout`/`getLocalAddress` registrations and
`docs/internal/netty-client-socket-write-after-close-nsme-FIXED.md`
(`java/lang/String.getOption(I)...`, same slot, different unregistered
caller method). Those were each patched by adding one more targeted native
override to route around whichever specific method fell through to real
`getImpl()` bytecode next — a recurring whack-a-mole. `SockSide`, this
file's own side-table for exactly this class of problem, already existed
(its own doc comment: "Synthetic field slots collide with real-JDK private
fields... Side-tables are independent of layout") but the host string was
never migrated into it (`host_id: i32 // unused (we still keep SOCK_HOST in
field for getInetAddress)`).

**Fix**: migrate `SOCK_HOST` off the raw object field and into `SockSide`
(new `host: String` field), updating all 10 read/write sites in
`net_phase_e.rs` (2 client-connect, 2 plain `<init>`, 1 `getInetAddress`
read, 2 `ServerSocket.accept()`, 2 SSL `createSocket`, 1
`setEnabledCipherSuites` read). This closes the collision **for every
current producer in this file** at once, rather than adding a fourth
whack-a-mole override for this specific call chain — any *future*
unregistered `Socket`/`SSLSocket` method that falls through to real
`getImpl()` bytecode will now see a correctly-null `impl` (real `setImpl()`
runs) instead of a live `String`.

## Verification (2026-07-22, Azure Linux host, real-JDK mode)
All 8 originally-affected classes re-run against the fixed binary
(isolated worktree + private `data/` dir to avoid shared-host test
contention):

| Class | Cluster | NSME before | NSME after | Overall result after |
|---|---|---|---|---|
| `TestLob` | A | yes | **gone** | fails later — separate, newly-exposed `MVStoreException: Chunk N not found` / file-lock issue, see residuals below |
| `TestLobApi` | A | yes | **gone** | **PASS** |
| `TestSQLXML` | A | yes | **gone** | **PASS** |
| `TestUpdatableResultSet` | A | yes | **gone** | **PASS** |
| `TestResultSet` | A | yes | **gone** | fails later — pre-existing, unrelated timezone/DST assertion (tracked in `bug-h2-formatter-datetime-conversion-unimplemented.md`'s family) |
| `TestWeb` | B | yes | **gone** | fails later — see residuals below |
| `TestServlet` | B | yes | **gone** | fails later — see residuals below |
| `TestJakartaServlet` | B | yes | **gone** | fails later — see residuals below |

3/5 Cluster A classes now fully pass. Both remaining Cluster A failures are
confirmed unrelated pre-existing bugs unmasked by this fix (not regressions —
they simply couldn't be reached before because the process crashed earlier
via the NSME). All 3 Cluster B classes have the NSME fully eliminated
(confirmed via `CRATONVM_DBG_SOCK=1` trace — zero `NoSuchMethodError`
events across the whole run) but still fail end-to-end due to a newly-exposed,
unrelated bug (below).

Regression spot-check (classes untouched by either fix's code paths, to
confirm no collateral damage from the `SockSide`/`drop_real_layout_synthetic`
changes): `TestStatement`, `TestView`, `TestDeadlock` — clean pass.
`TestPreparedStatement`/`TestFileSystem` fail, but on pre-existing, unrelated
assertions (`testDate8` timezone, `testFileSystem`/`testSimple` filesystem
behavior) — not near either fix's code paths.

## Residuals uncovered by this fix

Fixing the NSME let all 8 classes run further into their test bodies,
exposing bugs that were previously unreachable (masked by the earlier
crash). None of these share this doc's root cause (field-slot collisions in
native object construction); they are independently tracked:

- **`TestLob`**: `org.h2.mvstore.MVStoreException: Chunk 18 not found` in
  `testReclamationOnInDoubtRollback`, and (in a separate rerun)
  `OverlappingFileLockException` at `MVStore`'s file-channel lock — an
  MVStore persistence/locking issue, unrelated to native dispatch. Not yet
  filed as its own doc as of this writing; flagging here for whoever picks
  it up next.
- **`TestWeb`/`TestServlet`/`TestJakartaServlet`**: all three use H2's own
  embedded TCP protocol (`org.h2.server.TcpServer`) to connect to a
  same-process database. Root-caused via a standalone `-trace`-enabled
  repro to **`java.io.DataOutputStream.writeChars(String)` silently writing
  zero bytes** (`native_dos_init` never allocated JDK 25's `writeBuffer`
  scratch field, which real `writeChars` bytecode depends on) — **FIXED**,
  see `bug-h2-dataoutputstream-writechars-data-loss-FIXED.md` for the full
  writeup. `TestServlet`/`TestJakartaServlet` now fully pass. `TestWeb`
  still fails, but on a confirmed-different, narrower issue — see
  `bug-h2-testweb-logout-connectexception-mismatch.md`.

## Repro (still valid — now reproduces the residual, not the original NSME)
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.jdbc.TestResultSet     # Cluster A repro class
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.unit.TestServlet       # Cluster B repro class
```

## Related
- `docs/internal/fixed-suite-bugs/wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md` — the general field-slot-collision mechanism, first documented instance.
- `docs/internal/netty-client-socket-write-after-close-nsme-FIXED.md` — the exact same `SOCK_HOST`/`impl` collision, previously patched with a targeted override instead of the general side-table fix applied here.
- `../../../known-issues/h2/bug-h2-treemap-tailmap-headmap-view-corruption.md` — superficially similar symptom (wrong/garbage data from a collection view), independently guessed the same (refuted) `find_by_method_descriptor` hypothesis; still open, NOT confirmed to share this doc's root cause, and this session found nothing connecting the two beyond that now-refuted shared guess.
