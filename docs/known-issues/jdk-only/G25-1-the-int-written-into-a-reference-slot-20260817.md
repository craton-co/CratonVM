# G25-1 — the int written into a reference slot, and the null it actually wrote

**Status:** SOURCE-FIXED / BEFORE-MEASURED-ON-BOTH-BINARIES /
AFTER-NOT-MEASURABLE-BY-THIS-LANE.
**Provenance:** MEAS on both VMs for every "before" row, the oracle tables in
§3–§5, and the CratonVM columns of §3/§4. The mechanism in §1 is MEAS-BY-SOURCE
— it is read out of this VM's own coercion table, with the file and line, not
inferred from a symptom. HotSpot 25.0.3+9-LTS is the oracle throughout. Probe:
`scratchpad/g25/G25Probe.java` (ASCII labels only), run on BOTH VMs.

**Two binaries, and the difference between them is the point.**
`C:/craton/target-fcheck/release/cratonvm.exe` was rebuilt by the orchestrator
during this lane's session:

| binary | contains | role |
|---|---|---|
| built 00:44:49 | neither lane | "before" for G16 and for G25 |
| built 01:41:07 (**A**) | the sibling G16 lane's `net_phase_e.rs` RE.6b | "after" for G16, **"before" for G25** |

That **A** does not contain this lane's edits is measured, not assumed:
`SSLSession.getApplicationBufferSize()` on a fresh engine still reads `16384`
under **A** (probe `G25Which`), and §5 changes it to 16704. Every CratonVM
column below is measured on **A** unless it says otherwise. This lane is
forbidden to build, so there is no binary containing §1–§6 (§8).

This record fixes the ROOT CAUSE that
`G16-1-the-server-socket-impl-and-how-far-RSslLiveSession-got-20260817.md`
isolated and routed around. It also **corrects G16-1 and its own brief on the
mechanism**, which matters more than it sounds: the corrected mechanism is a
different bug species with a different blast radius, and it is why the null was
a null.

---

## 0. The headline

| vector | 00:44:49 binary (MEASURED) | binary **A**, G16 only (MEASURED) | after G25 |
|---|---|---|---|
| `RSslLiveSession` | `FAILED phase=startup`, NPE on `ServerSocket.setSoTimeout`, **0 CK rows** | `FAILED phase=verifier`, **67 CK rows** (§2.1) | not measurable |
| `RSslNullSession` | PASS, 89 checks | PASS, **89 checks** | not measurable |
| `RJdkNet` | PASS, 81 checks | PASS, **81 checks** | not measurable |
| `RJdkAsyncChannel` | PASS, 141 checks | PASS, **141 checks** | not measurable |
| `RJdkX509Intercept` | PASS, 26 checks | PASS, **26 checks** | not measurable |
| `RCrypto` | exit 0 | exit 0, **16 CK rows** | not measurable |

| surface | before (MEASURED) | after |
|---|---|---|
| `SSLServerSocket` bind identity (4 rows) | `isBound` false while open / **true after close**; `getInetAddress`, `getLocalSocketAddress` null; `toString` `ServerSocket[unbound]` | four natives, oracle-transcribed (§3) |
| `SSLServerSocket` cipher/mode surface (3 rows) | `AbstractMethodError` "has no Code attribute" | seven natives, getters AND setters (§4) |
| `SSLSession.getApplicationBufferSize` | `16384` in every state | 16704 / 16676, state-dependent (§5) |
| `SSLServerSocket.accept()` | consulted **no** `SO_TIMEOUT`, parked unboundedly | honours it, raises `SocketTimeoutException` (§6) |
| reference-slot writes, whole tree | **271 sites** found, all silent (§7) | 4 removed here; 267 nominated |


## 1. The mechanism — not a drop, and not the W7-84 guard. A null.

Both G16-1 §1 and this lane's brief say the `Value::Int` written into
`java.net.ServerSocket`'s reference-typed slot 0 was *"silently dropped by the
field-layout guard"*, and the brief further ties it to the `gc::guard` W7-84
warning. **Both halves are wrong**, and the truth is worse and more useful.

The path a native's `ctx.set_field` actually takes:

```text
NativeContextImpl::set_field           vm/src/vm/vm_exec.rs:11200
  -> resolve_field_descriptor_byte_cached(class_id, index)   -> Some(b'L')
  -> VmHeap::set_field_as(obj, index, value, b'L')           gc/src/heap.rs:880
  -> coerce_field_value_by_descriptor(value, b'L')           gc/src/heap.rs:1619
```

and the table at `gc/src/heap.rs:1674` reads, verbatim:

```rust
b'L' | b'[' => match value {
    Value::Object(_) => value,
    // A primitive landing where a reference is declared is a verifier
    // violation; degrade to null rather than surface a bogus Value
    // variant to native code. S111r29: extend this to ALL Int/Long
    // values (not just zero) ...
    Value::Int(_) | Value::Long(_) => Value::Object(None),
```

So the store is **not dropped**. The slot is actively written, and what is
written is `null`. That is the whole answer to "why was `getImpl()` null": it
was not "never assigned", it was **assigned null, by us, on purpose, by a rule
introduced to stop a different bug** (`HashMap.table` receiving `Int(capacity)`
and aborting `arraylength`). The rule is right; the caller was wrong.

Three consequences the "dropped" story hides:

1. **It is bidirectional and equally silent.** The same table coerces
   `Value::Object(None)` into an `I`/`Z` slot to `Value::Int(0)`
   (`gc/src/heap.rs:1657`). `create_ssl_server_socket`'s fourth write,
   `set_field(obj, 3, Value::Object(None))`, therefore did not "drop" either —
   it set `java.net.ServerSocket.closed = false`.
2. **It is invisible without a flag.** `overlay_check_access` (the detector
   whose comment names this exact species — *"the descriptor coercion below
   silently destroys the value (Int->null / ref->numeric)"*,
   `vm/src/vm/vm_exec.rs:11290`) runs only under `CRATONVM_DBG_OVERLAY`.
3. **The W7-84 warning is a DIFFERENT path and cannot see any of this.**
   `gc::autobox::observe_primitive_into_reference_field` fires on the heap's
   descriptor-less `set_field`, and it *boxes* rather than nulls. A native
   whose field descriptor resolves never reaches it. MEASURED, one full
   `--jdk-only` run of `RSslNullSession` with `CRATONVM_DBG_LAYOUT=1`: **every
   W7-84 warning is `class_id=ClassId(12) index=0`** — one class, one slot, the
   VM's own class-mirror populator writing a `ClassId` over
   `java.lang.Class.cachedConstructor`, exactly as the warning's own text says.
   Occurrences are rate-limited to `n < 8 || n.is_power_of_two()`, so the "~16
   warnings per VM start" in the brief is 12 log lines standing for at least 33
   stores of the same one site.

   **The brief's premise is therefore falsified in the useful direction.** The
   noisy warning is not a census of this defect; it is one unrelated boot-path
   site. The 271 real reference-slot writes in §7 produce **no warning at all**.

### 1.1 What each of the four writes did

`javap -p java.net.ServerSocket`, JDK 25.0.3+9 (`javax.net.ssl.SSLServerSocket`
declares no instance fields of its own, so these ARE the object's slots):

```text
0 private final    java.net.SocketImpl              impl
1 private volatile boolean                          created
2 private volatile boolean                          bound
3 private volatile boolean                          closed
4 private final    java.lang.Object                 socketLock
5 private volatile java.util.Set<SocketOption<?>>   options
```

| write | lands on | what actually happened |
|---|---|---|
| `Int(listener_id)` -> slot 0 | `impl` (ref) | coerced to `null`; **every inherited method reading `getImpl()` threw NPE** |
| `Int(local_port)` -> slot 1 | `created` (boolean) | `created = true` for any non-zero port |
| `Int(closed)` -> slot 2 | `bound` (boolean) | **`close()` set `bound = true`** — a socket that becomes bound by being closed |
| `Object(None)` -> slot 3 | `closed` (boolean) | coerced to `Int(0)`: `closed = false` |

`SslServerSocketState` was already the authority for all three values — every
reader consulted the side table first and only fell back to the field — so the
writes bought nothing and cost the entire inherited surface. **All four are
deleted.** The three slot constants are deleted with them, and the field
fallbacks are replaced by one named miss answer,
`ssl_server_socket_state_miss()`, because with the writes gone a fallback would
have been reading `created` and `bound` and calling them a port and a closed
flag.

A source-scanning unit test,
`the_ssl_server_socket_lifecycle_is_never_written_into_an_object_slot`, fails if
any of the five writes returns. It has to scan source: the defect is a write
that compiles, registers, and silently succeeds at doing the wrong thing, so
there is nothing to observe at runtime from inside the crate.


## 2. What this does NOT fix, and who fixes it

Deleting the writes stops the corruption. It does **not** populate `impl`, so
the 17 inherited-option rows G16-1 swept still need
`net_phase_e::register_ssl_server_socket_options` (RE.6b) — the sibling's
delegate — to answer. The two changes are complementary and neither is
sufficient alone:

* RE.6b makes `setSoTimeout`/`getSoTimeout`/`getReuseAddress`/… answer, which is
  the single call `RSslLiveSession` died on. **Measured to work** — §2.1.
* This change stops `close()` inverting `isBound`, and makes the four bind rows
  answerable at all. Not yet measurable.

### 2.1 RE.6b, settled — 0 checks to 67

G16-1 §0 could not measure its own "after" and said so. Binary **A** answers it.
MEASURED, `--jdk-only`:

```text
before (00:44:49)  CK RSslLiveSession FAILED phase=startup
                   java.lang.NullPointerException: ... "java.net.ServerSocket.getImpl()" is null
                        at java.net.ServerSocket.setSoTimeout(ServerSocket.java:711)
                        at RSslLiveSession.main(RSslLiveSession.java:801)
                   0 CK rows

after  (A)         CK RSslLiveSession handshake=25
                   CK RSslLiveSession attrs=17
                   CK RSslLiveSession distinct=6
                   CK RSslLiveSession invalidate=14
                   CK RSslLiveSession FAILED phase=verifier
                   javax.net.ssl.SSLPeerUnverifiedException: Certificate for <127.0.0.1>
                     does not match any of the subject alternative names or the common
                     name: HTTPS hostname wrong, should be <127.0.0.1>
                        at RSslLiveSession.verifier(RSslLiveSession.java:710)
                   67 CK rows
```

**One inherited setter was standing in front of a complete live TLS 1.3
handshake and 67 assertions**, exactly as G16-1 §4.5 predicted from a probe. The
vector now runs its whole `handshake` / `attrs` / `distinct` / `invalidate`
surface and dies four families later, in a place with nothing to do with
`SSLServerSocket`: `HttpsURLConnection` is not consulting the per-connection
`HostnameVerifier` the vector installs (NOMINATION 9). That is not this lane's
file, and §1–§6 do not move it.

**Ownership is now explicit in both directions.** `register_t27_natives` runs at
`lib.rs:18731`, AFTER `register_phase_e_networking` at `lib.rs:18688`, and
`register()` is last-write-wins with no unregister API — so a name added here
that RE.6b already owns does not conflict, it silently deletes RE.6b's body.
`t27_does_not_shadow_the_re6b_option_surface` (this file) is the tripwire from
this side; `ssl_server_socket_option_registrar_leaves_the_t27_owned_names_alone`
(net_phase_e) is the tripwire from theirs. The two lists are disjoint, checked
both ways.


## 3. NOM-2 — the four bind-identity rows, landed together

MEASURED on both VMs, probe `G25Probe`; the CratonVM column is binary **A**,
i.e. WITH the sibling's RE.6b already in. Ports masked as `<PORT>`.

| row | HotSpot | CratonVM before (MEASURED) | after (source) |
|---|---|---|---|
| `ssl.bound.isBound` | `true` | `false` | `true` |
| `ssl.bound.getInetAddress` | `/127.0.0.1` | `null` | `/127.0.0.1` |
| `ssl.bound.getLocalSocketAddress` | `/127.0.0.1:<PORT>` | `null` | `/127.0.0.1:<PORT>` |
| `ssl.bound.toString` | `[SSL: ServerSocket[addr=/127.0.0.1,localport=<PORT>]]` | `ServerSocket[unbound]` | same as HotSpot |
| `ssl.closed.isBound` | `true` | `true` (for the wrong reason — §1.1) | `true` |
| `ssl.closed.getInetAddress` | `/127.0.0.1` | **NPE (getImpl() is null)** | `/127.0.0.1` |
| `ssl.closed.getLocalSocketAddress` | `/127.0.0.1:<PORT>` | **NPE** | `/127.0.0.1:<PORT>` |
| `ssl.closed.toString` | `[SSL: ServerSocket[addr=/127.0.0.1,localport=<PORT>]]` | **NPE (impl is null)** | same as HotSpot |
| `ssl.wild.isBound` | `true` | `false` | `true` |
| `ssl.wild.getInetAddress` | `0.0.0.0/0.0.0.0` | `null` | `0.0.0.0/0.0.0.0` |
| `ssl.wild.getLocalSocketAddress` | `0.0.0.0/0.0.0.0:<PORT>` | `null` | same as HotSpot |
| `ssl.wild.toString` | `[SSL: ServerSocket[addr=0.0.0.0/0.0.0.0,localport=<PORT>]]` | `ServerSocket[unbound]` | same as HotSpot |

The `bound`/`closed` pair is the inversion of §1.1 caught in the act: the SAME
socket reads `isBound() == false` while it is open and listening, and `true`
after `close()`. Nothing else in the table moves between those two states on
HotSpot.

Three things the measurement changed about the plan G16-1 handed over:

1. **`toString` is not `ServerSocket[addr=…]`.** G16-1's NOM-2 predicted the
   plain `java.net.ServerSocket` rendering. The oracle prints
   `[SSL: ServerSocket[…]]`, because `sun.security.ssl.SSLServerSocketImpl`
   **overrides** `toString()` and wraps it. Transcribed, not derived.
2. **The wildcard renders differently from the literal, and it is not
   reconstructible.** A wildcard-bound `ServerSocket` reports
   `0.0.0.0/0.0.0.0` — its address is `InetAddress.anyLocalAddress()`, whose
   cached host NAME is the string `"0.0.0.0"`. `InetAddress.getByName("0.0.0.0")`
   gives `/0.0.0.0` and does not reproduce it. Only
   `new InetSocketAddress(int)` does, so that is the constructor the wildcard
   overloads use; `SSS_WILDCARD_BIND`/`SSS_WILDCARD_DISPLAY` name the pair.
3. **`close()` moves `isClosed()` and nothing else.** Every bind row answers
   after close exactly what it answered before, so `close()` carries the bind
   identity forward (`sss_closed_state`, and the unit test
   `closing_a_server_socket_does_not_unbind_it`).

**Why all four at once.** `java.net.ServerSocket.toString()` is
`if (!isBound()) return "ServerSocket[unbound]"; return "ServerSocket[" +
impl.toString() + "]";` and the `isBound()` there is an `invokevirtual` on
`this`, which finds a native registered on the receiver's class. Registering
`isBound` alone — the obvious one-row fix, and the oracle does say `true` —
walks the inherited `toString()` into `impl.toString()` on a null `impl` and
converts a row that AGREED with HotSpot into an NPE. Unit test
`sss_bind_identity_rows_move_together` is the tripwire.

`getInetAddress()` and `getLocalSocketAddress()` are built through the REAL
`java.net.InetSocketAddress` constructor bytecode rather than transcribed, so
the `hostname/literal:port` rendering, IPv6 bracketing and the
resolved/unresolved distinction are the JDK's own — the same reason RE.6b
borrows the option answers instead of writing them out.

**One disclosed divergence.** `ssl_server_bind_address` records the caller's
`InetAddress.toString()` at creation time (for `toString()`) and its
`getHostAddress()` literal (for rebuilding the address object). A caller that
binds with `InetAddress.getByName("localhost")` therefore gets
`toString()` right — `localhost/127.0.0.1` — but `getInetAddress().toString()`
reads `/127.0.0.1`, because the cached host name cannot be recovered from a
literal and recovering it would mean a reverse DNS lookup inside a getter.
Holding the caller's `InetAddress` object itself needs a global GC root; see
NOMINATION 5.


## 4. NOM-3 — three `AbstractMethodError`s, and the four setters that came with them

`javax.net.ssl.SSLServerSocket` is abstract and the object is an instance of it
directly, so a method with no native and no concrete body raises
`AbstractMethodError: ... has no Code attribute`. G16-1 measured three.

MEASURED on both VMs (`G25Probe`, CratonVM column on binary **A**), and the
setters are the reason this is seven registrations rather than three — the
probe found four MORE `AbstractMethodError` rows than G16-1's three, and all
four are setters or their supported-list twin:

| row | HotSpot | CratonVM before | after |
|---|---|---|---|
| `getUseClientMode` | `false` | **AbstractMethodError** | `false` |
| `setUseClientMode(true)` then get | `true` | AbstractMethodError | `true` |
| `getEnableSessionCreation` | `true` | **AbstractMethodError** | `true` |
| `setEnableSessionCreation(false)` then get | `false` | AbstractMethodError | `false` |
| `getEnabledCipherSuites().length` | `31` | **AbstractMethodError** | 16 (this VM's own list) |
| `getEnabledCipherSuites` == `getSupportedCipherSuites` | `true` | AbstractMethodError | `true` |
| `setEnabledCipherSuites(["TLS_AES_256_GCM_SHA384"])` then get | `[TLS_AES_256_GCM_SHA384]` | AbstractMethodError | same |
| `setEnabledCipherSuites(["SSL_NULL_WITH_NULL_NULL"])` | `IllegalArgumentException` "Unsupported CipherSuite: SSL_NULL_WITH_NULL_NULL" | AbstractMethodError | transcribed |
| `setEnabledCipherSuites(null)` | `IllegalArgumentException` "CipherSuites cannot be null" | AbstractMethodError | transcribed |
| all of the above after `close()` | unchanged | — | unchanged (side table) |

Registering only the three getters would have made them constants that lie the
moment anybody calls a setter — and the setter would *still* have been an
`AbstractMethodError`. Both messages are TRANSCRIBED, including the ordering:
the null check comes first on HotSpot, so `setEnabledCipherSuites(null)` never
reports `"Unsupported CipherSuite: null"`.

**The list is 16, not 31, and that is deliberate.** `getEnabledCipherSuites`
serves `phases_late::ssl_security::jsse_supported_suite_name_array`, which reads
`t27_tls::SUPPORTED_CIPHER_SUITE_NAMES` — the single source of truth that
`SSLSocket.getSupportedCipherSuites`, `SSLSocket.getEnabledCipherSuites`,
`SSLSocketFactory.getDefaultCipherSuites` and `SSLEngine` all already answer
from. Transcribing HotSpot's 31 here would make this VM's server socket claim
15 suites its own engine cannot negotiate, and would put a second list in the
tree — the exact drift E42 collapsed. What IS transcribed is the *relation* the
oracle establishes: enabled == supported until somebody narrows it.

`setUseClientMode` records the flag and does **not** turn the listener round;
`accept()` still performs a server handshake. That is disclosed in the code and
nominated (NOMINATION 6) rather than pretended, because a silently-ignored mode
switch is the same species as the `setNeedClientAuth` no-op this file removed in
wave 2.


## 5. NOM-6 — `getApplicationBufferSize` is two states, not one constant

G16-1 measured 16676 against CratonVM's 16384 after a live TLS 1.3 handshake and
corrected the earlier nomination (`getPacketBufferSize` is already right). This
lane re-measured the whole family across four states to find out whether a
predicate is enough or a per-suite table is needed.

MEASURED, HotSpot 25.0.3+9, `G25Probe`, in-memory `SSLEngine` pairs:

| state | `getPacketBufferSize` | `getApplicationBufferSize` |
|---|---|---|
| fresh engine, never negotiated | 16709 | **16704** |
| TLS 1.3, `TLS_AES_256_GCM_SHA384` | 16709 | **16676** |
| TLS 1.3, `TLS_AES_128_GCM_SHA256` | 16709 | **16676** |
| CratonVM, every state (before) | 16709 | **16384** |

Two states, two answers, **and no third**: within TLS 1.3 the value does not
move with the suite. So `session_has_negotiated` — the width-aware predicate
every other door in this file already uses — is the whole of the
state-dependence, and no per-suite table is needed. 16384 is neither value; it
is the RFC 8446 §5.1 TLSPlaintext cap, a floor.

**The audit the old comment asked for, done.** That comment kept 16384 as a
"safe UNDER-report", reasoning that raising a buffer-size constant without
auditing every `BUFFER_OVERFLOW` path is how a constant becomes an outage. The
audit: `do_wrap` reads **at most 16384 bytes of plaintext per call** out of
`srcs` whatever the caller offers (`bb_read_into(ctx, *bb, &mut app_bytes,
16384)` plus the `app_bytes.len() >= 16384` break), and then drains only
COMPLETE records that fit the destination. A caller that sizes its application
buffer from this accessor and fills all 16704 bytes therefore has 16384 of them
consumed, emitted as one record of at most 16406 into a
`getPacketBufferSize()`-sized destination, with a truthful `bytesConsumed`.
There is no state in which the larger number makes a record that cannot fit —
the livelock the comment feared needs the engine to promise a record bigger than
its packet buffer, and the 16384 cap is precisely what forbids that. On
`unwrap`, a larger destination is only ever safer. Unit test
`the_session_buffer_sizes_are_two_states_not_one_constant` asserts both the
values and the presence of that cap.

**The under-report was not free.** A caller that sizes a receive buffer from
this accessor, against a peer that fills a genuine 16676-byte application
record, is 292 bytes short — a `BUFFER_OVERFLOW` retry loop against a buffer the
VM has already told it is big enough.

The `--synthetic-jdk` twin at `tls.rs` still says 16384. It is
`#[cfg(feature = "synthetic-jdk")]`-only, so this copy is the live one by
default; the twin is NOMINATION 7.


## 6. NOM-1's second half — `accept()` now has a timeout

G16-1 §8: *"it did not make `setSoTimeout` reach `accept()`. `accept` parks in
`rustls_server_accept`'s poll loop unboundedly and consults no timeout at all."*

`rustls_server_accept` becomes `rustls_server_accept_within(id, Option<Duration>)`
returning a two-variant `AcceptFailure`. `TimedOut` is deliberately not a
`Failed("...timed out")`: `accept()` turns a failure into `java.io.IOException`
and an expiry into `java.net.SocketTimeoutException`, and an accept loop
distinguishes them by catching the latter and going round again —
`RSslLiveSession.serve` is written exactly that way. Collapsing them into one
string error would make an ordinary idle tick look like a dead listener.

The deadline is checked BEFORE the 20 ms poll nap and the nap is clamped to what
is left, so a 5 ms `SO_TIMEOUT` expires in about 5 ms. The 20 ms tick exists to
keep `close()` responsive, not to quantise the timeout. The timeout bounds only
the wait for a TCP peer; once one is accepted the handshake runs under the
stream's own 30 s read/write timeouts, matching JSSE, where `SO_TIMEOUT` governs
`accept()` and not the handshake behind it.

**Where the value comes from, and why through the Java door.** The timeout lives
on RE.6b's delegate, in `net_phase_e.rs`. This file must not register
`getSoTimeout` itself (§2), so `sss_accept_timeout` asks for it with
`ctx.invoke_virtual(this, "getSoTimeout", "()I")`. Dropping the error from that
call is safe, and that is a property of this VM's calling convention rather than
an assumption: `MethodCallFailed::ExceptionThrown` carries the `Throwable` **by
value** (`types/src/error.rs:44`), so there is no VM-global pending-exception
slot left dirty by discarding it. If RE.6b is ever removed and the inherited
`ServerSocket.getSoTimeout()` bytecode runs into the null `impl`, `accept()`
keeps its pre-G25 behaviour — block forever — instead of acquiring a new way to
fail.


## 7. The reference-slot census — 271 sites, and what they are not

Method: `scratchpad/g25/census.py`. For every
`ctx.set_field(<obj>, <literal-or-const slot>, Value::<primitive>)` under
`native-builtins/src`, `native-io/src`, `vm/src` and `native-api/src`, resolve
the object back to its `try_alloc_concurrent_synthetic(ctx, "CLASS", …)` and
check the slot against that class's REAL layout from `javap -p`, superclass
fields first. **271 hits.**

It is a LOWER BOUND. It resolves only single-line allocations and only
`read_native_pin` aliases, so at least one site in this very file is missed by
it (§7.1, the `X509Certificate` mirror, found by hand). It also cannot tell a
site that runs from one that does not.

| file | sites |
|---|---|
| `native-builtins/src/phases_early.rs` | 68 |
| `native-builtins/src/servlet.rs` | 22 |
| `native-builtins/src/http2.rs` | 19 |
| `native-builtins/src/phases_late/net_channels.rs` | 19 |
| `native-builtins/src/tls.rs` | 18 |
| `native-builtins/src/phases_late/collections.rs` | 16 |
| `native-builtins/src/phases_late/ssl_security.rs` | 13 |
| `native-builtins/src/t3_impl.rs` | 12 |
| `native-builtins/src/phases_late/concurrent.rs` | 10 |
| `native-builtins/src/phases_late/reflect_invoke.rs` | 9 |
| 23 further files | 65 |

| class collided with | sites | slot that gets nulled |
|---|---|---|
| `java/util/ArrayList` | 56 | `elementData` (`Object[]`) |
| `java/nio/channels/SocketChannel` | 22 | |
| `java/util/HashMap` | 19 | `table` — the case `S111r29` was written for |
| `javax/net/ssl/SSLContext` | 11 | `provider`, `contextSpi` |
| `java/util/concurrent/CompletableFuture` | 9 | `stack` |
| `java/lang/Thread` | 8 | |
| `java/util/jar/JarEntry` | 8 | |
| `javax/net/ssl/SSLEngineResult` | 8 | `status`, `handshakeStatus` |
| `java/net/Socket` | 7 | `impl`, `socketLock`, `in`, `out` |
| `javax/net/ssl/SSLSocket` | 7 | (same, inherited) |

**This is one defect species with one shape**: a synthetic slot model laid over
a real JDK class. It is NOT 271 bugs — most of these objects are never handed to
real JDK bytecode, so nulling a field nobody reads costs nothing. The ones that
bite are the ones where inherited bytecode *does* read the slot, which is
exactly the `SSLServerSocket` case. The general repair is not "stop writing
fields"; it is what `net_phase_e::dp_layout` and this file's `BbLayout` already
do — a layout-aware writer that asks which model the object is actually wearing.
NOMINATION 1.

### 7.1 The four that remain in this file, all disclosed

| site | class | slot | declares | why not fixed here |
|---|---|---|---|---|
| `accept()`, `SSS_SOCK_TLSID` | `javax/net/ssl/SSLSocket` | 2 | `socketLock` (`Object`) | shared 6-field model with `phases_late/ssl_security.rs`; under `--synthetic-jdk` these writes ARE the state. Needs a layout-aware writer, not a deletion. |
| `accept()`, `SSS_SOCK_CLOSED` | `javax/net/ssl/SSLSocket` | 3 | `in` (`InputStream`) | same |
| `alloc_engine_result` fallback | `javax/net/ssl/SSLEngineResult` | 0, 1 | `status`, `handshakeStatus` | the enum-resolution fallback; the same 4-slot model is written by `ssl_security.rs` (×2) and `tls.rs` |
| `default_ssl_context_or_create` | `javax/net/ssl/SSLContext` | 1 | `contextSpi` | width-2 model shared with `net_phase_e.rs` (×2) and read by four files |

Plus one the script missed and this lane found by hand: the
`X509TrustManager.getAcceptedIssuers` mirror writes `Value::Long(0)` into
`java/security/cert/X509Certificate` slot 2 (`subjectX500Principal`, a
reference) and a `String` into slot 1 (`Certificate.hash`, an `int`). The
canonical minter `keystore::make_x509_mirror` — which prefers a REAL
`sun.security.x509.X509CertImpl` — already exists and is used elsewhere in this
same file. NOMINATION 2.

### 7.2 The corroboration nobody was looking for

`vm/src/vm/vm_exec.rs:22505` carries a live workaround:

> *"`Socket.setKeepAlive` on a real-JDK socket can read CratonVM's synthetic TLS
> state as its private `impl` field. The resulting receiver is a String and the
> JDK attempts the impossible call below."*

That is this file's `accept()` writing the SNI host `String` into
`SSS_SOCK_HOST = 0`, which on the real `java.net.Socket` layout is `impl`. A
reference into a reference slot passes the coercion untouched, so this one is
not nulled — it lands, and a `String` becomes a `SocketImpl`. The VM already
had to grow a special case in its dispatcher to survive it. Same species, other
polarity, already costing something.


## 8. What this lane did NOT do

* **It did not run its own fix.** Binary **A** was rebuilt mid-session and
  contains G16's change but provably not this lane's (see the Provenance
  block); this lane is forbidden to run `cargo build`/`check`/`test` (the
  orchestrator owns the target dir). Every CratonVM column here is a BEFORE for
  §1–§6. Per HANDOFF §5 — *a green build proves you broke nothing, not that you
  did something* — nothing in this record claims that any row of §3, §4, §5 or
  §6 now matches, only that each is transcribed from a measurement of the
  oracle and a measurement of the divergence. **The next lane's first act
  should be to rebuild and re-run §0's six vectors, `G25Probe` on both VMs, and
  `G16Sweep`.**
* It did not populate `impl` with a real `SocketImpl`. That would make the
  inherited surface work without RE.6b's delegate and is the tidier end state,
  but it needs `java.net.ServerSocket`'s own `<init>` to run on the object (or a
  fabricated `NioSocketImpl`), and RE.6b already answers those rows correctly
  today.
* It did not fix the other 267 census sites (§7), or the four remaining in this
  file (§7.1).
* It did not touch `net_phase_e.rs`, `http_url_connection.rs`,
  `phases_late/ssl_security.rs`, `native-io/`, or anything under
  `regression-suite/`.
* It did not measure TLS 1.2's `getApplicationBufferSize`: the probe's
  handshake loop completes the server engine and leaves the TLS 1.2 client
  engine short of `FINISHED`, so those rows read as the never-negotiated state
  and are excluded from §5 rather than reported. §5's predicate answers 16676
  for a completed TLS 1.2 session too; that is the one row in this record that
  is inference rather than measurement, and it is marked here.


## 9. NOMINATIONS

**NOMINATION 1 — the whole tree. A layout-aware field writer.** 271 measured
sites (§7) write a primitive into a slot the loaded class declares as a
reference, where `gc/src/heap.rs:1674` silently replaces it with `null`. The
fix is not per-site deletion; it is the `dp_layout` / `BbLayout` pattern —
resolve which model the object is wearing, then write the slot that model
declares — applied at the four or five allocation shapes that account for most
of the count (`ArrayList`, `HashMap`, `SocketChannel`, `SSLContext`,
`SSLEngineResult`). Worth pairing with making
`overlay_check_access`'s cross-type arm default-ON in `--jdk-only`: it already
detects exactly this and is gated behind `CRATONVM_DBG_OVERLAY`.

**NOMINATION 2 — `t27_tls.rs` (this file), `getAcceptedIssuers`.** The
hand-rolled 4-field `java/security/cert/X509Certificate` mirror writes
`Long(0)` into `subjectX500Principal` and a `String` into `Certificate.hash`.
`keystore::make_x509_mirror` is the canonical minter, prefers a real
`sun.security.x509.X509CertImpl`, and is already called by this file's
`getPeerCertificates`. Not taken here because it changes the object handed to
every trust-manager caller and this lane cannot run one.

**NOMINATION 3 — `native-builtins/src/phases_late/ssl_security.rs` +
`t27_tls.rs`, jointly. The `SSLSocket` 6-field model (§7.1).** Slots 0/2/3 of
that model are `impl` / `socketLock` / `in` on the real `java.net.Socket`
layout. Slot 0 already has a workaround compiled into the VM's dispatcher
(§7.2). Both files write the model, so neither can move alone.

**NOMINATION 4 — `native-builtins/src/net_phase_e.rs`, RE.6b.** Two items.
(a) `sss_option_call`'s delegate now has a second consumer: `accept()` reads
`getSoTimeout` through `invoke_virtual` (§6). A `pub(crate)` reader would make
that a direct call and remove the Java round-trip, but the Java door is the
safer default and needs nothing. (b) RE.6b's own comment block still says the
`Int` write is "DROPPED by the field-layout guard"; it is coerced to `null` —
see §1, which is the same correction this record makes to G16-1.

**NOMINATION 5 — `t27_tls.rs` (this file). The caller's `InetAddress`.**
`getInetAddress()` rebuilds the address from the recorded literal, so a socket
bound with `InetAddress.getByName("localhost")` reports `/127.0.0.1` where
HotSpot reports `localhost/127.0.0.1` (§3). Retaining the caller's own object
needs a global GC root, which this file has machinery for
(`default_ssl_context_slot`) but not a generic map form.

**NOMINATION 6 — `t27_tls.rs` (this file). `setUseClientMode` on a server
socket.** The flag is recorded and read back honestly, but the listener is not
turned round: `accept()` still performs a server handshake. Making it real means
building a rustls *client* connection over an accepted TCP stream. Disclosed in
the code rather than pretended.

**NOMINATION 7 — `native-builtins/src/tls.rs:1370`, the `--synthetic-jdk`
twin of `getApplicationBufferSize`.** Still the single 16384. §5's two measured
constants (`JSSE_APPLICATION_BUFFER_SIZE_FRESH` / `_NEGOTIATED`) are
`pub(crate)` for exactly this.

**NOMINATION 9 — `native-builtins/src/http_url_connection.rs`, the
per-connection `HostnameVerifier`.** MEASURED on binary **A**:
`RSslLiveSession` now dies at `phase=verifier` with
`SSLPeerUnverifiedException: Certificate for <127.0.0.1> does not match any of
the subject alternative names or the common name: HTTPS hostname wrong, should
be <127.0.0.1>` (`RSslLiveSession.verifier:710`). The vector installs a
verifier that returns `true` unconditionally, and JSSE consults a custom
verifier precisely when its own endpoint identification has FAILED — which is
why the vector uses an IP literal against a cert carrying only a `dNSName`
SAN. `huc_verify_hostname` does invoke the verifier
(`http_url_connection.rs:2578`), and `huc_unverified_peer_message`
(`:2602`) is the shared message for all three ways to fail, so the message
alone does not distinguish "no verifier found" from "verifier declined". The
next lane should establish which branch fires — the likely answer is that the
per-connection `setHostnameVerifier` is not reaching
`huc_verify_hostname`'s lookup. **This is now the front of `RSslLiveSession`.**

**NOMINATION 8 — carried forward, unchanged, from G16-1:** NOM-4
(`http_url_connection.rs:404`'s same-named `register_https_session_accessors`
overwrites five of `net_phase_e`'s six) and NOM-5 (`native-io`,
`sun/nio/ch/Net`: `getReuseAddress()` reads `true` on an accepted `Socket`
where HotSpot reads `false`). Neither is this lane's file and neither was
touched.

**Not nominated, deliberately:** nothing under `regression-suite/`.
`RSslLiveSession` is correct as written.


## 10. Files this lane touched

* `native-builtins/src/t27_tls.rs` — §1.1 (the four writes deleted, the slot
  constants deleted, the layout recorded on `SSS_FIELDS`), §3 (bind identity:
  four registrations, `bind_address`/`bind_display` on the state, the two
  `InetSocketAddress` constructors, `sss_to_string`, `sss_closed_state`), §4
  (seven registrations, `sss_mode_states`, `sss_enabled_suites_table`), §5
  (three named constants, the state-dependent accessor), §6
  (`rustls_server_accept_within`, `AcceptFailure`, `sss_accept_timeout`), and
  nine unit tests:
  `the_ssl_server_socket_lifecycle_is_never_written_into_an_object_slot`,
  `sss_bind_identity_rows_move_together`,
  `sss_abstract_method_error_rows_are_registered_with_their_setters`,
  `t27_does_not_shadow_the_re6b_option_surface`,
  `sss_to_string_matches_the_oracle`,
  `closing_a_server_socket_does_not_unbind_it`,
  `an_unknown_server_socket_reads_closed_and_unbound`,
  `sss_mode_defaults_are_the_measured_hotspot_values`,
  `the_session_buffer_sizes_are_two_states_not_one_constant`.
* `docs/known-issues/jdk-only/G25-1-…md` — this record.

Nothing else. No `INDEX.md` / `README.md` edit; no other Rust file.
