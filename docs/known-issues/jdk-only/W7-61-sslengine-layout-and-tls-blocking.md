# W7-61 — the `SSLEngine` layout row is a false positive, and the four TLS blocking sites need a different fix from the other nineteen

> ## Third pass, 2026-08-12 (lane A3 / P1-C) — both items re-verified, and a THIRD `SSLSocket` defect found and fixed
>
> **Re-verification of what this record claims, against today's tree.** Every
> named symbol from items 1 and 2 is present:
> `native-builtins/src/tls.rs` carries `mod registry_ordering_tests` with all
> three tests (`net_phase_e_wins_create_ssl_engine_on_the_essential_path`,
> `p68_ssl_is_the_last_writer_on_the_ssl_engine_surface`,
> `t27_covers_every_engine_triple_p68_registers_on_the_abstract_class`);
> `servlet::TlsEntry` still carries `raw: Option<TcpStream>` and
> `s2_tls_classify_after_block` / `t27_tls::rustls_classify_after_block` both
> exist; `probes/AsyncCloseProbe.java` and its `.expected.txt` are in the tree.
> `ssleng_alloc` is still in `ssl_security.rs`. **Nothing was built or run on
> this pass either** — this is a source reading, and it does not re-open or
> re-close any verdict above. The Windows half of item 2 stays OPEN.
>
> **The new defect, and why this record did not see it.** Items 1 and 2 are
> about the `SSLEngine` layout and about *waking* a TLS stream. Neither asked
> what class the stream OBJECT is. Under `--jdk-only` that was the whole
> failure:
>
> ```
> OK   P1-C SSLContext from PKCS12 keystore
> OK   P1-C SSLServerSocket binds
> OK   P1-C client handshake completes
> FAIL P1-C connected streams are real types
>   -> java.lang.NoClassDefFoundError: javax/net/ssl/SSLSocketOutputStream
> ```
>
> (orchestrator's loopback TLS witness, real RSA PKCS12 identity, real
> handshake; HotSpot 25 is 6/6 on the identical program.) The handshake
> succeeded and the FIRST stream access died, so no `SSLEngine`, `SSLContext`
> or unconnected-`SSLSocket` check could have reached it — three such checks
> were green on the same run.
>
> `javax/net/ssl/SSLSocketInputStream` and `…OutputStream` are **declared by no
> supported JDK image** — they are listed as such in
> `native-api/src/no_image_receiver.rs::NO_IMAGE_JDK_RECEIVERS`, which is what
> re-tags their eight natives `SyntheticStub`. Strict mode refuses the mint, and
> the refusal is correct; the defect was that `SSLSocket.getInputStream()` /
> `getOutputStream()` are `bridge` rows that survive strict mode and then ask
> for it.
>
> **Fixed by making the receiver real**, in `ssl_security.rs`: the two carriers
> are now `sun/security/ssl/SSLSocketImpl$AppInputStream` and `$AppOutputStream`
> — the exact pair real JSSE returns, so `getClass().getName()` gains HotSpot
> parity instead of naming a class no JDK has ever had. `javap -p` against JDK
> 25 on this host confirms both, their supertypes (`java.io.InputStream` /
> `OutputStream`) and their declared methods. The eight legacy registrations are
> **kept**, because `net_phase_e`'s own layered-socket branch still mints the old
> names and because `scripts/baselines/jdk-only-gated-never-delete.tsv` pins all
> eight rows.
>
> **Three hazards this record's readers should know about, because two of them
> are dispatch rules item 1 analysed and did not connect to this surface:**
>
> 1. `vm_exec.rs` (~21095, and again in the `check_override` chain ~21393) and
>    `invoke.rs` (~3234) each route any receiver whose class name
>    `starts_with("sun/security/ssl/SSLSocketImpl")` to `javax/net/ssl/SSLSocket`
>    's native for a fixed tuple list. `$AppInputStream` and `$AppOutputStream`
>    match that PREFIX, and `("close", "()V")` is in the list — so `in.close()`
>    on a stream would have run the SOCKET close against a stream receiver,
>    writing `Value::Int` over `NEW13_SOCK_TLSID`/`NEW13_SOCK_CLOSED`, which on
>    a real `AppInputStream` are `appDataIsAvailable` (boolean) and `readLock`
>    (a `ReentrantLock` reference). Neutralised inside `ssl_security.rs` by a
>    receiver-class guard at the top of `SSLSocket.close()V` that delegates to
>    `ssl_stream_close`. The durable fix is to narrow the three predicates so
>    they do not catch the nested classes; that is **nominated**, not applied,
>    and the guard is correct either way. `close()V` is the only pair in the
>    list that a Java call site can produce on a stream — the rest are not
>    declared on `InputStream`/`OutputStream`.
> 2. Moving onto a real receiver means every concrete method the real class
>    declares must be shadowed or be safe on an all-null layout.
>    `AppInputStream` declares `skip(long)`, whose body takes `this.readLock`
>    and reads `this.buffer` — both null on a carrier allocated without the real
>    constructor. `skip(J)J` is therefore registered for the first time. This is
>    the general form of the trap and is worth restating whenever a fabricated
>    carrier is retargeted at a real class.
> 3. The private `Int` moved off raw slot 0 to an APPENDED slot
>    (`try_alloc_with_appended_slots`), because slot 0 on the real layout is
>    `oneByte`, a `byte[]`. Readers take it from `object_num_fields(this) - 1`,
>    which is sound only because every such carrier is one this file allocated
>    with `width == 1`; a foreign instance reads a reference there, `as_int()`
>    answers `None`, and the identity-keyed side table answers instead. Chosen
>    over `appended_slot_base_for_class` per call to protect this record's own
>    measured ~265 ns body budget for `read()I`.
>
> **NO CLOSE-PATH CHANGE WAS MADE.** Nothing in item 2 — neither classifier,
> neither `raw`-duplicate shutdown, nor any readiness gate — was touched, added
> to or removed. The `SSLSocket.close()` guard above is a receiver-type
> discrimination, not close-awareness: it changes which of two existing close
> bodies runs, and neither body's blocking behaviour changed. The Windows half
> of item 2 is untouched and still open, and the "socket readiness is not stream
> readiness" deadlock warned about in "The Windows half, re-derived" was
> deliberately not gone near.
>
> **Also correcting a stale verdict in a neighbouring record.**
> `W7-17-vm-internal-door-sweep.md`'s table row for these two classes reads
> *"behaviour carriers; door correct — none"*, with `strict?` = **no**. That was
> a true statement about the strict corpus AS IT THEN REACHED, not about the
> classes: the corpus had no connected-TLS vector. The orchestrator's witness is
> that vector, and it makes the row `strict? YES, fatal` — the same shape W7-17
> itself assigned to `CratonVM$HttpServerLoop`. That file is not this lane's;
> the correction is **nominated**.

**Branch:** `fix/sslengine-layout-and-tls-blocking-20260812`, based on dev
`2d2467c81`.

**Nothing in the Rust half of this record was built or run.** This session had
no build. Every "before" is a reading of the tree; every "after" is a claim
about source. What WAS executed is the Java probe, against HotSpot 25.0.3 on
this Windows 11 host, and every number attributed to the oracle below is a
transcript, not an inference.

Two inputs, both on branches that are **not on dev**:
`W7-49-slot-index-recensus.md` (`fix/w44-slot-index-sweep-20260812`) and
`W7-53-blocking-close-family.md` (`fix/blocking-close-awareness-family-20260812`).
Read the merge note at the end before landing this.

---

## Item 1 — `SSLEngine` 7 wide over a class declaring 2

### The verdict

**Not live.** W7-49 §7 lists
`javax/net/ssl/SSLEngine` as `7 vs 2, 27 receiver-slot accesses at indices 2–6,
LIVE`, and says the repair was not attempted because it "threads a base through
27 access sites plus two sibling registrars… a last-write-wins question this
lane could not settle without a build."

The question is settleable without a build, and the answer inverts the premise.
On the Compatible/strict boot path the 7-wide allocation **cannot happen** and
the 7-slot map **has no receiver**. On the synthetic path the map is live — and
there a renumber would break the one mode that uses it.

**So: no renumber.** What landed instead is the ordering written down where the
map is, one stale sentence corrected, and three tests that fail if the ordering
moves.

### How the ordering was determined

Not by indentation. `native-builtins/src/phases_late/ssl_security.rs` contains
nested `fn` items inside `register_p68_ssl` whose bodies are dedented and whose
closing braces sit at **column 0** (lines 2611 and 2626). A nearest-preceding-
column-0-`fn` scan therefore reports line 4575 as *outside* `register_p68_ssl`,
which is wrong. Two methods were run and reconciled:

1. a comment/string/char/raw-string-aware brace-depth scan from each `fn`'s
   opening brace, reporting where depth returns to 0 — this gives the true
   spans; and
2. a column-0 scan, kept only to expose where the two disagree.

They disagree in exactly two places and both disagreements are informative:

| site | column-0 scan says | brace scan says | truth |
|---|---|---|---|
| `ssl_security.rs:4575` | outside `register_p68_ssl` | inside `register_p68_ssl` (1734..5135) | brace scan — the misindented nested `fn`s fooled the other |
| `lib.rs:23713` | inside `register_synthetic_overrides` | inside `register_essential_natives_with_shims` | column-0 scan — a naive whole-file depth counter had drifted over 17k lines; a proper span scan from `register_synthetic_overrides`'s own brace (21210..23967) confirms it |

The reconciled spans: `register_essential_natives_with_shims` = lib.rs
6878..20732; `register_synthetic_overrides` = lib.rs 21210..23967;
`register_p68_ssl` = ssl_security.rs 1734..5135. Every registration call named
below sits at **brace depth 1** in its enclosing function — unconditional, not
inside an `if`.

### The ordering

**Compatible / strict** (`use_synthetic_jdk == false`, and the default
`cratonvm-cli` where `synthetic-jdk` is compiled out). Both boot arms —
vm_init.rs:1701 and vm_init.rs:2239 — enter the same function:

| lib.rs | call | what it does to this surface |
|---|---|---|
| 18191 | `register_p68_ssl` | `SSLContext.createSSLEngine` ×2 → `ssleng_alloc` (**requests 7**); 21 triples on `javax/net/ssl/SSLEngine` under a 7-slot map |
| 18214 | `net_phase_e::register_phase_e_networking` → `register_re6_ssl_context` | **re-registers both `createSSLEngine` descriptors**, allocating `sun/security/ssl/SSLEngineImpl` |
| 18252 | `t27_tls::register_sslengine_real` → `register_engine_impl_natives` | 32 triples on `sun/security/ssl/SSLEngineImpl`, keyed by an object-identity side table, no slot indices |

`ssleng_alloc` has exactly two callers (ssl_security.rs:2001 and 2010) and both
are those `createSSLEngine` registrations. Both are overwritten 23 lines later.
**The 7-wide allocation is dead in Compatible mode**, and lib.rs's own comment
at 18192 says that ordering is deliberate and load-bearing (it exists so p68's
"fake, non-cryptographic" engine cannot pre-empt the rustls-backed one).

**Synthetic** (vm_init.rs:1580, `register_builtins` = `register_essential_natives`
then `register_synthetic_overrides`):

| lib.rs | call | what it does |
|---|---|---|
| 23713 | `register_tls_natives` | `register_ssl_context` (createSSLEngine → `alloc_ssl_engine`, **requests 14**) + `register_ssl_engine` (19 triples on a 14-slot map) |
| 23716 | `register_phase68_natives` → `register_p68_ssl` **again** | the 7-slot map and `ssleng_alloc` win back |

lib.rs:23708 states this explicitly: *"Registered BEFORE phase68 so that
register_p68_ssl's … implementations take precedence via last-writer-wins."*
**p68 is the last writer in BOTH modes.** A repair belongs there or nowhere —
and `tls.rs`'s own comment at ~830 ("Shadowed by phases_late.rs::register_p68_ssl
… which registers the same triple later and wins") is correct and was the
clearest single pointer in the tree.

### Why the 7-slot map has no receiver in Compatible mode

Three facts, each read from the dispatch code rather than assumed:

* `invoke.rs:1051` — for a non-special, non-array call, `invoke_class` is the
  **receiver's runtime class name**, read from `args[0]`'s `class_id`. Step 1
  (`native_override.rs::resolve_step1_native`, called at invoke.rs:3199) looks
  the triple up under that name, not under the call site's constant-pool owner.
* `invoke.rs:3271` — the hierarchy walk that would otherwise reach an abstract
  superclass runs with `walk_native_hierarchy == false` for virtual and special
  calls, and returns `None` immediately when the receiver's own class declares
  the method. So a library subclass of `SSLEngine` (Netty's `JdkSslEngine`, a
  Conscrypt or BouncyCastle engine) runs its own bytecode.
* `javax/net/ssl/SSLEngine` is **abstract**. A concrete subclass must implement
  every abstract method, so no instantiable class inherits one. The only
  receiver that can land on this map is an object CratonVM allocated **on the
  abstract class itself** — which, with `ssleng_alloc` dead, nothing does.

And the one receiver that does exist, `sun/security/ssl/SSLEngineImpl`, is
covered outright: `register_engine_impl_natives` registers **all 21** of p68's
(name, descriptor) pairs on `SSLEngineImpl` plus eleven more, so the walk never
starts.

`HEADER_SIZE` was checked and is not a factor, as W7-49 said of the crates
generally: every site here addresses by slot index (`set_field(obj, i, …)`), and
there is no hand-rolled header arithmetic anywhere in this surface.

### The real defect this uncovered, left NAMED

In **synthetic mode** the two maps coexist on one object, and that is a live
two-layouts-on-one-class condition — the shape that made `java.lang.Process` a
bug. `register_p68_ssl` wins its 21 triples; the **8 tls.rs triples p68 does not
re-register keep the 14-slot map on a 7-wide object**:

| tls.rs triple that survives | slot it addresses | what that slot is on a 7-wide p68 engine |
|---|---|---|
| `<init>()V` (`init_ssl_engine_fields`) | writes 0–13 | slots 7–13 are past the end |
| `getPeerHost()Ljava/lang/String;` | 5 | p68's `handshake_status` (an `Int`) |
| `getPeerPort()I` | 6 | p68's `session` (a reference) |
| `getApplicationProtocol()Ljava/lang/String;` | 7 | past the end |
| `setSSLParameters` / `getSSLParameters` | 8–13 | past the end |
| `wrap([BB;BB)` / `unwrap(BB;[BB)` | 2, 3, 1 | inbound/outbound-done vs want-client-auth, enabled-protocols, need-client-auth |

Not repaired here. Giving the surface one owner moves `ssl_security.rs` and
`tls.rs` in one step and re-bases the `vm/src/vm/tests.rs` fixture that pins the
7-slot map by hand (`shared.mem.heap.alloc_object(ClassId::new(0), 8)` with a
comment naming `ssleng_alloc`'s layout). That needs a build. Repairing one side
only would move the disagreement rather than close it — the same reason W7-49 §5
gave for not renumbering `AsynchronousSocketChannel` from the wrong crate, and
the standard this lane was asked to hold to.

Two smaller disagreements found in passing and not touched:
`javax/net/ssl/SSLEngineResult` is allocated with **2** fields by
`tls.rs::alloc_ssl_engine_result` and with **4** by both
`ssl_security.rs`'s wrap/unwrap and `t27_tls::alloc_engine_result`; and
`SSLEngineResult$HandshakeStatus` with 1 by tls.rs and 2 by p68.

### What landed

* `native-builtins/src/phases_late/ssl_security.rs` — the ordering table above,
  in place at the head of the SSLEngine block, with the reason a renumber must
  not be attempted from either side.
* Same file — the sentence *"ssleng_alloc allocates every SSLEngine directly on
  this abstract class (never a concrete subclass), so an unregistered method
  here always throws AbstractMethodError on any real-JDK caller"* corrected. It
  was true when written, is false since `register_re6_ssl_context` started
  winning, and is what made W7-49 read the site as LIVE. A comment outliving its
  defect, doing damage.
* `native-builtins/src/net_phase_e.rs` — `register_re6_ssl_context` is now
  `pub(crate)` so the ratchet can name the exact registrar instead of the
  umbrella.
* `native-builtins/src/tls.rs` — `mod registry_ordering_tests`, three tests that
  compare **registered `fn`-pointer identity**, not behaviour:
  `net_phase_e_wins_create_ssl_engine_on_the_essential_path`,
  `p68_ssl_is_the_last_writer_on_the_ssl_engine_surface`, and
  `t27_covers_every_engine_triple_p68_registers_on_the_abstract_class`.

**Touches Compatible mode:** the `pub(crate)` and the comments do not change any
registration, any slot index or any dispatch. Nothing in item 1 changes runtime
behaviour in any mode. That is the point: the site did not need a behaviour
change, it needed the reachability question answered.

### The probe

`probes/SlotIndexRecensusProbe.java` is W7-49's pattern and the right place for
a CratonVM-arm reading here; this lane did not add a row to it because the file
is not on dev either (it is on `fix/w44-slot-index-sweep-20260812`) and the row
this lane would add is a **green**, not a red: it asserts the site is not live.
When that branch lands, the row to add is three lines and is specified here so
it can be written without redoing the analysis:

```java
// W7-61: in Compatible mode the engine must be a real SSLEngineImpl, not a
// bare abstract SSLEngine, because that is what makes ssleng_alloc's 7-slot
// map unreachable. Read the two declared fields back through the JDK's own
// reflective accessor, NEVER through getPeerHost()/getPeerPort() — tls.rs
// registers natives on both, so those read slots 5 and 6 of CratonVM's map
// and would agree with a corrupt object.
SSLEngine e = SSLContext.getDefault().createSSLEngine();
System.out.println("engineClass=" + e.getClass().getName());   // want: sun.security.ssl.SSLEngineImpl
e.setUseClientMode(true);
Field f = SSLEngine.class.getDeclaredField("peerHost");        // slot 0, a String
f.setAccessible(true);
System.out.println("peerHost=" + f.get(e));                    // want: null, NOT 1
Field g = SSLEngine.class.getDeclaredField("peerPort");        // slot 1, an int
g.setAccessible(true);
System.out.println("peerPort=" + g.get(e));                    // want: -1 or 0, NOT 0-from-a-clobber
```

`Field.get` resolves the slot from the **loaded class's** layout, so it is a
read the natives cannot satisfy. `getClass().getName()` is printed beside the
predicate deliberately: if it ever says `javax.net.ssl.SSLEngine`, the ordering
moved and the two field reads are the ones that will show the damage.

---

## Item 2 — the four TLS blocking sites

### What the sites are

`servlet.rs::s2_tls_read_direct` and `s2_tls_write` (native-tls), and
`t27_tls.rs::rustls_stream_read` and `rustls_stream_write` (rustls). All four
have one shape:

1. clone an `Arc<Mutex<Stream>>` out of a process-wide registry;
2. release the registry lock (correct, and already documented at each site);
3. lock the per-stream mutex and block.

And both closes have the mirror shape: remove the registry entry, then
`try_lock` the stream — which fails, because the parked reader holds it — and
give up, on the stated grounds that *"the entry is already unregistered, so
dropping our handle is sufficient — the socket closes when the last `Arc`
goes."*

That sentence is false in exactly the case it was written for. **The parked
reader is holding an `Arc`.** The last `Arc` does not go, the socket is not
closed, no wakeup is delivered, and the reader waits forever.

### Why the other nineteen sites' loop must not be transplanted

W7-53's mechanism parks in `poll` on a bounded slice, re-asks the registry after
the poll, and **abandons the wait** with `ErrorKind::Interrupted` once the slot
is gone. Abandoning is the part TLS cannot take. A TLS record is assembled from
an unbounded number of underlying `recv` calls; a reader that returns between
two of them hands its caller a fragment and leaves the stream desynchronised
for good, which is worse than the hang.

That objection is specific to *abandoning*, and this is the observation the fix
turns on: **it does not apply to classifying a call that has already returned.**
At that instant the record layer is at rest — either a whole record has been
delivered or the call has failed on its own.

So the fix is two halves, and neither is the 19-site loop:

**1. WAKE — make `close()` end the byte stream, from outside the stream mutex.**
`servlet::TlsEntry` already carries `raw: Option<TcpStream>`, a `try_clone`d
duplicate whose own doc comment says it exists so an fd-level operation can run
without waiting on `stream`'s mutex — and `s2_tls_close` never used it. It does
now. `t27_tls`'s two entry structs gain the same field, cloned at all four
construction sites (`TlsServerStream::tcp()` reaches the socket through the
enum's three variants). `shutdown(Shutdown::Both)` on the duplicate takes no
mutex, frees no handle a worker is mid-syscall on (so it is not the
use-after-close `pipe.rs` was fixed for), and does not cut a record in half — it
ends the connection, which the record layer must already handle.

**2. CLASSIFY — after the blocking call returns, re-ask the registry.**
`servlet::s2_tls_classify_after_block` and `t27_tls::rustls_classify_after_block`.
If the id has left the registry, report `ErrorKind::Interrupted` ("socket
closed") rather than EOF or a peer error. Asked *after* on purpose, exactly as
W7-53 argues: a close landing while parked is seen on the very next instruction,
and a close that raced a readiness edge still wins — HotSpot fails an I/O a
concurrent `close()` beat rather than handing back bytes on a socket Java has
already closed. Bytes that genuinely arrived before the close are still
delivered; the next call reports the close.

`ErrorKind::Interrupted` is unambiguous at these four sites for the same reason
`net_phase_e::re1_socket_closed_err` is at its own: nothing else in the path
produces it. Every real EINTR is reissued below this level (`EintrIo`,
`http_url_connection::read_eof_tolerant`), and the only producer of this kind is
the classifier, which fires only on a state no successful I/O can be in. The
registry is removed from by `s2_tls_close` and `rustls_stream_close` and by
nothing else — grepped, three sites total — so there are no false positives.

### Platform, and the half that stays open

Stated as a contract rather than a measurement; the host is Windows and **no
Linux arm was run**.

* **Unix** — `shutdown(SHUT_RDWR)` on the duplicate wakes a parked `recv` with
  EOF, and the classifier then reports the close instead of a spurious
  end-of-stream. Both halves deliver.
* **Windows** — Winsock has **no `shutdown` that aborts a pending blocking
  call**; only `closesocket` does, and closing a handle a worker is inside a
  syscall on is the use-after-close this family already paid for once. So for a
  reader **already parked**, the Windows arm is a no-op and **that half of the
  row stays OPEN**. It is written down at both close sites and here rather than
  quietly counted, for exactly the reason W7-53 left the Windows pipe sink write
  open: a mechanism that compiles, looks like the others, and cannot deliver the
  wakeup is what removes a site from a census while leaving the defect. The
  correct Windows fix is the one that record already names — overlapped I/O with
  a bounded `GetOverlappedResultEx` — a change to how the socket is created, not
  landable on inspection.

  A close that has *already* landed is still observed on Windows, because the
  classification runs on every return.

### The Windows half, re-derived 2026-08-12 — what closes it, and the trap that nearly did not get named

Both halves above are **verified present in the tree** on this pass
(`servlet::s2_tls_close` and `t27_tls::rustls_stream_close` shut down
`entry.raw`; `s2_tls_classify_after_block` and `rustls_classify_after_block` run
on every return of all four sites). The Windows arm is still open and stays open.
Three things were established about *how* it closes, and they belong here because
this record is where the Windows half lives:

1. **The mechanism is not a `shutdown` and not `closesocket`.** It is the one the
   nineteen fixed sites use: never enter the blocking call until a bounded
   `poll`/`WSAPoll` on the registry-held duplicate says the socket is ready, and
   re-ask the registry after each slice. Winsock's lack of an aborting `shutdown`
   is then irrelevant — the thread is parked in `WSAPoll` with a 25 ms bound, not
   in `recv`.
2. **A socket-readiness gate in front of a TLS read is a DEADLOCK unless it is
   screened by the assembler first.** TLS decrypts a whole record at a time, so a
   caller that asked for less than the assembler holds gets the rest with no
   socket I/O at all — `rustls-0.23.42`'s `Stream::prepare_read` touches the
   transport only `while conn.wants_read()`, and `wants_read()` is false while
   `received_plaintext` is non-empty. The screens exist (`conn.wants_read()` for
   rustls, `native_tls::TlsStream::buffered_read_size()` in
   `native-tls-0.2.18`), and they must be asked **under the stream mutex, before
   it is released for the poll**. W7-53-blocking-close-family.md's "Third pass"
   section carries the full derivation, the ordering, and the
   `Option<TcpStream>` → `Option<Arc<TcpStream>>` change that removes the
   per-read `try_clone` its own design assumed.
3. **The two WRITE sites must still get nothing**, and that is not a scoping
   decision — it is the measurement in "The two WRITE sites get no close-aware
   loop" above. A close-aware TLS write would be a behaviour HotSpot does not
   have.

So the Windows half is now specified rather than merely named, and it is still
**not applied**: the whole gain is on the platform with no build, the blast radius
is every TLS read in the VM, the `LegacyDsa` screen is `#[cfg(unix)]` and cannot
be written from this host, and the row is masked by a 30 s timeout rather than
hanging. The pilot to take first is `rustls_stream_read`'s client arm, which has
no `cfg` arms at all.

### The two WRITE sites get no close-aware loop, and that is now measured

This was going to be an argument. It is a measurement instead.

On **HotSpot 25.0.3 / Windows 11, 2026-08-12**: `SSLSocket.close()` issued from
another thread while a writer is parked inside `getOutputStream().write()`
**does not return**. JSSE serialises `duplexCloseOutput()` behind the write it
would have to interrupt. The probe reports it as
`tlsWrite parked=true ms=4000 outcome=CLOSE_BLOCKED`.

So there is **no reference behaviour in which a close wakes a parked TLS write**.
A close-aware write loop at `s2_tls_write` / `rustls_stream_write` would not be
HotSpot parity; it would be a behaviour HotSpot does not have. Both keep the
classification (which costs nothing and reports a close that already landed) and
neither gets a loop.

This also forced a harness change: `close()` and the row cleanup ran inline in
`AsyncCloseProbe`, which silently assumed `close()` returns. Against a parked
TLS write it does not, and the probe became the hang it measures. Both now run
on bounded daemon threads. The 13 pre-existing rows are unaffected — their
closers return immediately — and their transcript is unchanged apart from
sub-millisecond timings.

### The instrument

`probes/AsyncCloseProbe.java` and `probes/AsyncCloseProbe.expected.txt`, taken
from `fix/blocking-close-awareness-family-20260812` and **extended**, not
duplicated. Four rows added, and the two properties the brief requires are both
present and both executed:

* **`-Dprobe.skipClose=tlsRead`** — executes the failure path.
  `tlsRead parked=true ms=4000 outcome=TIMEOUT … FAIL`, exit 1, process leaves.
* **`tlsSelfTestNoPark`** — a TLS-specific calibration row that passes by *not*
  parking. The plain `selfTestNoPark` proves the guard can say `false` on a
  plain socket and says nothing about TLS, where a read can fail to park for a
  reason that exists only in the record layer (a whole record already decrypted
  in the engine's buffer). Without it, `parked=true` on `tlsRead` rests on a
  check nobody has seen refuse a TLS read.

And one more, against the vacuous shape the brief names:

* **`tlsReadIntegrity`** — asserting a TLS read returned *something* is not a
  test that it returned a complete record. This row writes a 200 KiB payload
  (many records; the maximum plaintext fragment is 16 KiB) from the peer and
  reads it back **byte-for-byte**, reporting `CORRUPT@<offset>` at the first
  wrong byte. A close-aware loop bolted on at the wrong level — one that wakes a
  reader mid-record and hands back a partial buffer — corrupts here while
  `tlsRead` stays green. It is the row that would catch the failure mode W7-53
  refused to risk.

The TLS pair is real: a self-signed PKCS12 (CN=localhost, SAN
`dns:localhost` + `ip:127.0.0.1`, RSA 2048, 36500 days) generated with JDK 25's
own `keytool` and embedded base64 in the probe. Generated rather than
runtime-built because making a certificate at runtime needs `sun.security.x509`
internals, and a probe that needs `--add-exports` is a probe that gets skipped.
The client uses a trust-all `X509TrustManager` — these rows are about
close-awareness, not PKI, and supplying an application trust manager is also
what switches endpoint identification off, which keeps the rows from depending
on how the host resolves `localhost`.

**Measured 2026-08-12, HotSpot 25.0.3, Windows 11: 17 pass, 0 fail, 0
inconclusive, exit 0.** Full transcript in the `.expected.txt`.
**There is no CratonVM arm and none should be inferred. No binary was built.**

---

## What the orchestrator's build must show

### Item 1 — compile and three tests

```
cargo test -p cratonvm-native-builtins --lib registry_ordering_tests
```

* `net_phase_e_wins_create_ssl_engine_on_the_essential_path` — GREEN.
  If it is RED, `register_p68_ssl` is winning `createSSLEngine` on the essential
  path, `ssleng_alloc` is live again, and the 7-over-2 allocation is real. Every
  reachability verdict in item 1 is then void.
* `p68_ssl_is_the_last_writer_on_the_ssl_engine_surface` — GREEN.
  If RED, tls.rs's 14-slot map is authoritative in synthetic mode and p68's is
  dead code, which inverts the "do not renumber" conclusion.
* `t27_covers_every_engine_triple_p68_registers_on_the_abstract_class` — GREEN.
  If RED for a `sun/security/ssl/SSLEngineImpl` assertion, a triple on the
  Compatible engine now falls through to the hierarchy walk and can reach p68's
  7-slot map on the abstract superclass, which writes an `Int` into `peerHost`.
  That is the corruption path, and a RED here is the first sign of it.
  If RED for a `javax/net/ssl/SSLEngine` assertion, p68's surface changed and
  the list needs updating — not deleting.

These are new tests; a compile failure in `registry_ordering_tests` is the one
outcome that means the ratchet does not exist. Note `crate::t27_tls::
register_sslengine_real` is `pub fn` and `register_phase68_natives` /
`register_p68_ssl` / `register_re6_ssl_context` are `pub(crate)`, all reachable
from `tls.rs`.

### Item 2 — compile, existing tests, and the probe

```
cargo test -p cratonvm-native-builtins --lib
cargo test -p cratonvm-native-io --lib net::tests
cargo test -p cratonvm-native-io --lib socket_channel::tests
```

must stay green — nothing in `native-io` was touched, and the three sibling
readers W7-53 did not touch are covered there.

The compile is where the `raw` field additions land. Four construction sites
were edited (`t27_tls.rs`, the `TlsClientStreamEntry` ×2 and
`TlsServerStreamEntry` ×2 literals); a "missing field `raw`" error means one was
missed. `TlsServerStream::tcp()` reaches the socket through `StreamOwned::sock`,
`native_tls::TlsStream::get_ref()` and `openssl::ssl::SslStream::get_ref()` —
the third is `#[cfg(unix)]` and is **not compilable on this host**, in principle
as well as in practice.

Then, on a built binary:

```
javac -d <out> probes/AsyncCloseProbe.java
java                     -cp <out> AsyncCloseProbe   # CONTROL — run first, every time
cratonvm.exe             -cp <out> AsyncCloseProbe   # Compatible
cratonvm.exe --jdk-only  -cp <out> AsyncCloseProbe   # strict
```

Read the four TLS rows as follows:

| row | Windows CratonVM | Linux CratonVM | what a different reading means |
|---|---|---|---|
| `tlsSelfTestNoPark` | `parked=false` PASS | same | `parked=true` ⇒ the TLS park guard is broken and **no TLS row means anything**, however green |
| `tlsReadIntegrity` | `returned:204800` PASS | same | `CORRUPT@<n>` ⇒ a record was cut; that is the failure the whole design avoids. A short `returned:<n>` ⇒ truncation at a record boundary |
| `tlsRead` | **`TIMEOUT` FAIL is EXPECTED** — the open Windows half | PASS with an exception | a PASS on Windows means Winsock delivered a wakeup this record says it cannot; re-check which native serviced the call (`CRATONVM_DBG_TLS_SRV=1`) before believing it |
| `tlsWrite` | `CLOSE_BLOCKED` PASS | `CLOSE_BLOCKED` or an exception, both PASS | **`TIMEOUT` is the defect**: the close returned and the writer is still parked. That is the reading a half-repair produces |

Two readings that would falsify the design rather than a row:

* `outcome=returned:-1` on `tlsRead` (Linux) — the reader woke on the EOF and
  the classifier did **not** retype it. The registry re-ask is not running, or
  is running before the call rather than after.
* Any `SocketTimeoutException` on a TLS socket with no timeout set — nothing
  here derives a deadline, so this would mean the classification is firing on a
  live id.

---

## Flags

None added. No `CRATONVM_*` variable was introduced, so no
`types/src/flag_groups.rs` / `types/tests/flag-surface.txt` /
`docs/flag-tokens.md` / `docs/config/flag-inventory.md` work is implied.
`probe.skipClose`, `probe.settleMs`, `probe.wakeMs` and `probe.enterMs` are Java
system properties the probe reads itself.

## Compatible mode

Per change, since `--real-jdk` is contractually frozen:

| change | touches Compatible? | justification |
|---|---|---|
| item 1's comments, `pub(crate)`, ordering tests | **no behaviour change in any mode** | nothing registers, dispatches or indexes differently |
| `s2_tls_close` / `rustls_stream_close` shutting down the registry-held duplicate | **yes** | genuine HotSpot-parity fix. JDK 25 `Socket.close()` is unconditional — a thread blocked in an I/O operation *will* throw — and these four sites did not deliver it at all |
| the two classifiers | **yes** | same clause: the wakeup must be reported as a close, not as EOF. Today a woken reader reports end-of-stream, which is a wrong answer, not merely a missing exception |
| `AsyncCloseProbe`'s bounded closer/cleanup | probe only | no VM code |

One parity gap left open deliberately: the classifier's `Interrupted` reaches
Java as `IOException: socket closed` (every caller in `ssl_security.rs` maps any
`Err` to `RuntimeError::IOException`). HotSpot raises `SocketException`, a
subtype, so `catch (IOException)` — the overwhelmingly common form — is
unaffected. Retyping it means touching four call sites in `ssl_security.rs` the
way `net_phase_e::re1_socket_read_stream` already does for the plain-socket
surface, and is the named follow-up.

## Tests

No existing test was weakened. The 13 pre-existing `AsyncCloseProbe` rows keep
their `want` strings, their verdicts and their transcript. Added: three Rust
ordering tests in `native-builtins/src/tls.rs`, and four probe rows.

## Merge note — this branch depends on two unlanded siblings

`probes/AsyncCloseProbe.java` and `probes/AsyncCloseProbe.expected.txt` were
taken from `fix/blocking-close-awareness-family-20260812` (commit `404a0d42d`)
with `git checkout <ref> -- <path>` and extended, because the brief asked for
that probe to be extended rather than duplicated and it is not on dev.

If that branch merges to dev first, git will report an **add/add conflict** on
both files. Resolve it by taking **their** version and re-applying this lane's
additions, which are deliberately contiguous and delimited so this is
mechanical:

1. the TLS imports, appended to the import block;
2. the block headed `// TLS rows (W7-61)`, immediately above `main`;
3. the four `wrap(...)` registrations at the end of the row array in `main`;
4. the three harness edits inside `check(...)` — the daemon-thread closer, the
   `CLOSE_BLOCKED` branch, and the daemon-thread cleanup in the `finally`.

`W7-49-slot-index-recensus.md` and `W7-53-blocking-close-family.md` are cited
throughout and live on `fix/w44-slot-index-sweep-20260812` and
`fix/blocking-close-awareness-family-20260812` respectively. Both should land
before or with this record; neither file is touched here.

## What this lane could not resolve

1. **The synthetic-mode two-map condition on `javax/net/ssl/SSLEngine`.**
   Specified above, site by site and slot by slot. It needs one owner for the
   surface, which moves two files and re-bases a `vm/src/vm/tests.rs` fixture.
   A build lane's work.
2. **The Windows half of the TLS read wakeup.** Named, with the design that
   would close it, and expected to read `tlsRead TIMEOUT` until then.
   **Re-derived and specified 2026-08-12** — see "The Windows half, re-derived"
   above and W7-53-blocking-close-family.md's "Third pass": the design that was
   written down would have *deadlocked* on buffered plaintext, the screen that
   makes it sound is named per stack, and the per-read `try_clone` it assumed is
   replaced by an `Arc`. Still not applied, and the reasons are stated there
   rather than reduced to "no build".
3. **Any Linux reading at all.** The Unix arms added here are not compilable on
   this host, and the platform claims are contracts rather than observations.
4. **`SSLEngineResult` / `HandshakeStatus` width disagreement** (2 vs 4, 1 vs 2
   across three files). Found in passing, unmeasured, not touched.
