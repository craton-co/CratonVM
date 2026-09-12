# Lane 6 — the §9 residuals: a carrier with no constructor

**Date:** 2026-09-12
**Branch:** `claude/l6-http-residuals-20260912`
**Base:** `dev` at `46d7b7211`
**Predecessor:** [`lane-6-tls-uri-residuals-20260912.md`](lane-6-tls-uri-residuals-20260912.md),
whose §9 is this wave's whole scope.

**39 differing rows closed across four probes, nothing worse.**

```text
  L6HttpLogicSweep      14 diff lines -> 0
  L6SocketSweep         40            -> 0
  L6HttpLoopbackSweep   16            -> 6
  L6TlsParamSweep       36            -> 22
  L6UriSweep / L6UrlSweep / L6InetSweep / L6X500Sweep   0, unmoved
  L6JcaSweep            34, unmoved (no §9 item names it)
```

---

## 1. What §9 listed, and what happened to each

| §9 item | rows | outcome |
|---|---|---|
| `HttpURLConnection` natives hijack a user SUBCLASS | 87, 88, 101, 115, 117 | fixed — §6 |
| two registrars own the streaming-mode setters | 64, 65, 66, 155 | **the diagnosis was wrong**; the real cause fixed — §3 |
| `getRequestMethod()` and the wire disagree | 67, 69 | fixed — §4 |
| measured and left: `L6SocketSweep`'s 20 rows | all 20 | fixed — §7 |
| measured and left: `L6TlsParamSweep` 66, 83, 86, 89, 94 | 5 | fixed — §8 |
| measured and left: `L6TlsParamSweep` 74 | 1 | fixed — §8 |
| measured and left: `L6TlsParamSweep` 69 | 1 | left, for a NEW reason — §9 |
| measured and left: `L6HttpLoopbackSweep` 37, 42 | 2 | left, with the mechanism — §9 |
| `L6HttpLoopbackSweep` 65 | 1 | left, with the mechanism — §9 |
| `L6TlsParamSweep` 60, 62, 63, 70, 71, 72, 77, 78, 92, 93 | 10 | §6 item 1's decision stands (the LISTS) |

---

## 2. The first command was `--dump-native-registry`, and it refuted §9

§9's second bullet said two registrars own `setFixedLengthStreamingMode` /
`setChunkedStreamingMode` and named `phases_early`'s `p54_*` pair as the winner,
keeping its state in synthetic slots that alias real fields on a real carrier.
It also said the way to settle it was one command. It was, and the answer was
different:

```text
$ cratonvm --jdk-only --dump-native-registry reg.json -cp /tmp Nop
$ jq '.natives[] | select(.class=="java/net/HttpURLConnection")' reg.json

  connect()V            by http_url_connection.rs:5514   overwrote=bridge
  getContentLength()I   by http_url_connection.rs:5564   overwrote=bridge
  getLastModified()J    by net_phase_e.rs:12097          overwrote=None
  ...  20 triples, and NONE of them from phases_early
```

`register_phase54_net_extras` is reached only from `register_synthetic_overrides`,
which is `#[cfg(feature = "synthetic-jdk")]`. **In `--jdk-only` mode it does not
exist.** Its `p54_*` bodies, its aliased slots and its two spurious refusals are
real, and they are real in a mode the residual rows were not measured in. The
comment inside that file which says it "runs AFTER
`http_url_connection::register_http_url_connection_real`" is true of the
synthetic build and irrelevant to this one.

The same dump gave the finding the wave is built on. `java/net/HttpURLConnection`
carries **twenty** registrations where `sun/net/www/protocol/http/HttpURLConnection`
carries thirty-one. The thirteen missing ones are a retirement table —
`retired_shadow.rs`'s 2026-09-11 lane-L6 block — and the JDK's own bytecode is
what runs for them.

That is the whole wave: **retiring a native onto a carrier this VM ALLOCATES
hands the JDK's bytecode an object whose constructor never ran.**

---

## 3. Item 2 — zero is a value, not an absence

`HttpURLConnection` declares

```java
    protected int  chunkLength            = -1;
    protected int  fixedContentLength     = -1;
    protected long fixedContentLengthLong = -1;
    protected int  responseCode           = -1;
```

and `-1` is each one's "not set" sentinel. `URL.openConnection()` allocates its
carrier with `try_alloc_concurrent_synthetic` and returns it; no `<init>` runs,
so all four arrive as 0 — which reads as SET.

The retired `setFixedLengthStreamingMode(int)` refuses when `chunkLength != -1`;
the retired `setChunkedStreamingMode(int)` refuses when `fixedContentLength != -1`.
So both refused, on every fresh connection, each naming the mode the other had
supposedly set. That is rows 64, 65 and 66 of `L6HttpLoopbackSweep` and row 155
of `L6HttpLogicSweep`, and it is exactly the symptom §9 described — with a
different cause, in a different file, in the other build mode.

`http_url_connection::huc_write_declared_field_defaults` writes the declared
initial state — the four sentinels plus `method`, `doInput`, `doOutput`,
`allowUserInteraction`, `useCaches`, `instanceFollowRedirects`,
`ifModifiedSince`, `connected` — **by name**, never by slot, and is called from
two places:

* `net_phase_e`'s mint site, which had six of them inline; and
* `huc_init`, because a user subclass reaching `super(u)` runs THIS native
  instead of the constructor and has the identical hole. `L6HttpLogicSweep`
  row 22 is that carrier: `setFixedLengthStreamingMode(-1L)` has to reach the
  argument check, and it only does if `chunkLength` is genuinely `-1`.

One list, because two lists is how the six inline writes came to be six rather
than twelve. The mint site's source-witness test used to assert
`set_field_by_name(conn, …)` appeared at least four times; it now asserts `url`
is written by name and the shared list is called, which is the same claim about
slots with the drift removed.

### The streaming state has to cross back

The JDK's bytecode writes those three fields; `perform` reads
`RealReq::streaming`. `real_streaming_mode` reads the fields first and falls
back to the side table, which is what the `sun.*` carriers (whose setters ARE
ours) still use.

Arming the fixed-length path then exposed the next layer: the live stream saw
**0 bytes** of a 4-byte body. In real-JDK mode `getOutputStream()` hands back a
genuine `java.io.ByteArrayOutputStream` and its writes are the JDK's own
bytecode, which dispatches no `BaosEvent` — so `live.written` is 0 for every
fixed-length request. The head is already on the wire with the right
`Content-Length` and the body is in the BAOS, so the response path now pushes
the remainder rather than failing a request whose body this VM is holding.

---

## 4. Item 3 — two readers of one method

`setRequestMethod` and `getRequestMethod` are in the same retirement table. On
the minted carrier the JDK's bytecode owns the `method` FIELD and this VM's
`RealReq.method` never sees the call. The wire kept reading the side table:

```text
  setRequestMethod("PUT")    field=PUT   table=""      wire sent POST
  setDoOutput(true) only     field=GET   table=POST    getRequestMethod() said GET
```

— rows 69 and 67, the same disagreement in opposite directions, which is what
made it read as two defects.

`real_method` is the one reader, field first. The two writers keep both copies
in step: `huc_set_request_method` (live on the `sun.*` carriers) writes the
field as well as the table, and `getOutputStream`'s GET→POST promotion — which
is a JDK behaviour, not ours — writes the field too, or `getRequestMethod()`
keeps answering GET for a request that goes out as a POST.

---

## 5. `connect()` left `connected` at 0

Row 73: `connect(); setDoOutput(true)` succeeded. `URLConnection.connect()` sets
`connected = true`, and the inherited bytecode for `setDoOutput`, `setDoInput`,
`setUseCaches` and `setRequestProperty` — all retired — reads that field to
refuse a late change. This VM defers the actual request from `connect()` to
`getResponseCode()`/`getInputStream()`, which is right, and the deferral had
been allowed to make the call invisible, which is not: a deferred connect is
still a connect.

The response path already wrote the field (a fix from the previous wave, for
rows 41 and 71). `connect()` is the door it did not cover.

---

## 6. Item 1 — a native on an abstract class claims a user subclass

`register_one(r, "java/net/HttpURLConnection")` exists so applications that use
the abstract base class through reflection work. Dispatch probes the RECEIVER's
class chain, so a test double that extends `HttpURLConnection` gets those
natives too — and they answer from this VM's own connection state, which the
double never populated. `L6HttpLogicSweep`'s `Fixture` overrides
`getHeaderField(String)` and hands out thirteen headers; `getContentLength()`
and `getContentLengthLong()` tried to open a socket to `fixture.invalid`,
`getLastModified()` answered 0, and `setDoInput`/`setUseCaches` accepted a
change after `connect()` that the JDK refuses.

**The discriminator is one lookup.** This VM mints six connection carrier
classes and a subclass is none of them; a JDK class is never a user subclass
whatever its name, so the boot/extension loader ids are excluded too. That
second half is not decoration — `java/net/URLConnection`'s registrations are
inherited by every connection impl in the image, including
`sun.net.www.protocol.file.FileURLConnection` and
`sun.net.www.protocol.jrt.JavaRuntimeURLConnection`, and several of those
carriers are this VM's own.

**The fallback has to be bytecode.** `invoke_virtual_bytecode_only` resolves from
the receiver, so `getContentLength()` reaches `URLConnection.getContentLength`,
which calls `getContentLengthLong()`, which this guard forwards again, which
calls `getHeaderFieldLong`, which calls `getHeaderField(String)` — and THAT
resolves to the subclass's override. Every step is the JDK's own algorithm;
nothing here re-implements `getHeaderFieldDate`'s three date formats. Row 101's
`Last-Modified` is in RFC 850 form, which the native's
`parse_rfc1123_date_millis` cannot read and `URLConnection`'s own body can.

§9 sized this at "~31 macro-generated wrappers" and that is what it took:
`subclass_aware_bodies!` generates one `fn` per triple, because
`NativeCallback` is a bare `fn` pointer and a wrapper has to know its own name
and descriptor at runtime. All 29 non-`<init>` triples are wrapped, not the five
a probe caught. `<init>` is deliberately excluded: a constructor is never
dispatched virtually, and forwarding by receiver would resolve back to the
subclass's own `<init>` and recurse.

### The macro generated the `r.register` calls too, and blinded two gates

First cut, the macro emitted the registrations as well — 29 lines of
`$r.register($cls, $name, $desc, $fname);` instead of 29 literal ones. The gate
that caught it is `the_drift_baseline_has_no_stale_rows`, red in all three
`native-builtins` configurations with:

```text
STALE BASELINE — 11 recorded drift pair(s) no longer drift.
  register_phase54_net_extras
      java/net/HttpURLConnection.getContentLengthLong()J
      java/net/HttpURLConnection.getErrorStream()Ljava/io/InputStream;
      …
```

Those eleven pairs drift exactly as much as they did before. `registrar_drift.rs`
and `registrar_reachability.rs` find registrations by SCANNING this source —
they expand `for x in [..]` loops, and their own doc says so, but they cannot
expand a macro. Losing the shipping half of a pair reads to the scanner as "the
drift is gone", and the ratchet's instruction is to regenerate the baseline and
explain. Regenerating would have been the wrong move: the correct fix is to put
the 31 `r.register` lines back in the source where a scanner can read them, and
let the macro define the bodies alone.

This is the third time this species has been recorded here
(`a-helper-refactor-can-blind-the-source-scanning-drift-gate`,
`the-drift-scanner-is-blind-to-register-with-kind`). What is new is the shape of
the RED: not "you added drift", but "the drift you had is gone" — a stale-row
complaint from a gate that had simply stopped being able to see one side.

---

## 7. `L6SocketSweep` — 20 rows, and a registrar that was not the live one

Every one of these is the JDK's own argument validation, absent:

```text
  setSoTimeout(-1)         "negative SO_TIMEOUT"     -> "timeout < 0"
  setSendBufferSize(0)     "negative send buffer"    -> "Invalid send size"
  setTrafficClass(256)     "tc is not in range"      -> "Invalid IP_TOS value"
  connect(addr, 65536)     accepted                  -> "port out of range:65536"
  connect(null, 9)         accepted                  -> "Address can't be null"
  connect(unresolved)      IOException/getaddrinfo   -> SocketException "Unresolved address"
  send(wrong peer)         accepted                  -> "Connected and packet address differ"
  send(null)               "null object argument"    -> the JDK's helpful NPE for `synchronized (p)`
  bind() twice             accepted, and REBOUND     -> SocketException "Already bound"
  getOption(null)          UOE naming ""             -> messageless NPE
  setOption(TCP_NODELAY)   "DatagramSocket.setOption: …" -> "'TCP_NODELAY' not supported"
  closed socket            kept its port, IOExceptions -> -1, and SocketExceptions
  setTimeToLive(-1)        accepted (and became 0)   -> "Invalid TTL/hop value"
  joinGroup(non-multicast) accepted                  -> SocketException "Not a multicast address"
  joinGroup(null, null)    accepted                  -> IAE "Unsupported address type"
```

Two of those are worth naming separately.

**`bind()` on a bound socket did not just accept — it rebound.** The body had an
`sd.fd >= 0` arm that called `udp_rebind`, which moved a live socket to another
port and reported success. The JDK's answer is `SocketException("Already
bound")`, and `isBound()` here already said true. The arm is gone rather than
made unreachable.

**The multicast fixes went to the wrong file first.** They were written into
`phases_late/net_channels.rs`, which registers thirteen `java/net/MulticastSocket`
triples, and the trial binary printed the same words as the control. The dump
said the live registrar is `native-io/src/net.rs`. Same tell as the three
non-firing guards of the previous wave, and the same cost: one build.
`net_channels.rs`'s copies were left untouched rather than fixed in parallel —
[two producers of one carrier class is a failure family], and adding a second
correct copy would have made this file the third.

The type matters as much as the text. `java.io.IOException` walks straight past
`catch (SocketException e)`, and every "closed"/"already bound" refusal the JDK
raises is the subclass; `ds_require_not_closed` and `ds_require_open_socket`
exist so the distinction is not restated at fifteen call sites.

---

## 8. The TLS leftovers

**Rows 66 and 89 — an engine's role flag in a String slot.** `setUseClientMode`
wrote `ctx.set_field(this, 0, …)`, and slot 0 of the real
`sun.security.ssl.SSLEngineImpl` this VM allocates is
`javax.net.ssl.SSLEngine.peerHost`, a REFERENCE. Same species as the
`SSLContext` slot-1 flag the previous wave found, one class over. There were
THREE copies of the pair — `phases_late::ssl_security` and `tls.rs` on
`javax/net/ssl/SSLEngine`, and `t27_tls` on the Impl — and the Impl's is the
live one, which cost a build to learn. All three now read
`EngineState`.

The state is split in two on purpose. `is_client` is what the handshake does and
still defaults to `true`, because this VM's engine callers are overwhelmingly
clients and flipping that default would turn every client that never calls the
setter into a server. `client_mode_set` is whether the caller ever configured
it, and `getUseClientMode()` reports the CONFIGURED role — which for a fresh
engine is `false`, as on HotSpot.

**Row 94 — the `TLS_` prefix is not a cipher-suite name.** `is_cipher_suite_name`
accepted anything with a `TLS_`/`SSL_` prefix, so `TLS_NO_SUCH_SUITE` was
stored and then silently widened by `cipher_provider_for`, which falls back to
the full provider when nothing maps: the application ran unrestricted believing
it had restricted. Every real JSSE suite name is a TLS 1.3 name (all five of
which are in `SUPPORTED_CIPHER_SUITE_NAMES`), a pre-1.3 name spelling out the
key exchange and cipher either side of `_WITH_`, or an `_SCSV` signalling value.
That structural test rejects a typo without narrowing the set to what this
engine can negotiate — the property the old predicate was widened to protect.
`tls.rs`'s registration, which is the last writer on the
`javax/net/ssl/SSLEngine` surface, had no validation at all.

**Row 118 — three protocol lists, one VM.** `SSLSocket.getSupportedProtocols()`
said `[TLSv1.3, TLSv1.2]`, `SSLEngine`'s said `[TLSv1.3, TLSv1.2, TLSv1.1]`, and
`SSLContext.getSupportedSSLParameters()` agreed with the engine. The row asks
the VM whether it agrees with itself and it read `false`. Now
`t27_tls::SUPPORTED_PROTOCOL_NAMES`, the same "one list, one spelling" rule
`SUPPORTED_CIPHER_SUITE_NAMES` already carries — and the TLSv1.1 rationale
(Tomcat INTERSECTS a connector's configured list with the advertised set and
silently drops the rest) is stated once, at the list, instead of at one of the
three copies.

**Row 86 — a handshake with no peer.** `startHandshake()` on
`SSLSocketFactory.getDefault().createSocket()` returned normally, telling the
caller a handshake had completed on a socket that has no peer.

**Row 83 — a lossy `SSLParameters` round-trip.** `setSSLParameters` read the
cipher list out of the object and discarded the rest, so
`setSSLParameters(p); getSSLParameters()` lost the client-auth flags and the
endpoint-identification algorithm. `SSLParameters` is CONFIGURATION and the JDK
round-trips all of it.

**Row 74 — `ServerSocketFactory.getDefault()` returned an instance of the
ABSTRACT base class.** HotSpot's is `javax.net.DefaultServerSocketFactory`, a
class the JDK does instantiate. Minting the concrete class is safe here because
`register_plain_server_socket_factory` registers the four `createServerSocket`
bodies on it as well — which is also what keeps
`tls_deny::deny_plaintext_fallback` in the path, since
`DefaultServerSocketFactory`'s own bytecode would build the same plain
`java.net.ServerSocket` without consulting the guard at all.

---

## 9. What this wave did not do

**`L6TlsParamSweep` row 69 — `SSLSocketFactory.getDefault()`'s class.** §9 gave
the reason as "minting the JDK's own Impl hands every inherited call to bytecode
this VM does not implement". That reason is weaker than it looked — the fix
used for row 74 (mint the concrete class, register its declared methods) would
work — but the cost is different: `javax/net/ssl/SSLSocketFactory` is minted at
**five** sites across four files, and `sun.security.ssl.SSLSocketFactoryImpl`
declares six `createSocket` overloads plus the two cipher-suite getters, all of
which would need registering on the new name before the first one is switched.
One row, five mint sites, one binary each if it goes wrong.

**`L6TlsParamSweep` 60, 62, 63, 70, 71, 72, 77, 78, 92, 93 — the LISTS.** §6
item 1's decision stands: this VM advertises the fifteen cipher suites and the
three protocols it has, HotSpot advertises thirty-one and six, and the fix for
that is a TLS stack, not a longer literal. Row 77 and row 93 moved from two
protocols to three on the way past — not because the list grew, but because the
VM stopped disagreeing with itself about it.

**`L6HttpLoopbackSweep` row 42 — `getRequestProperty` after connect.** This is a
statement about which CLASS the carrier is, not about a native. HotSpot's
carrier is `sun.net.www.protocol.http.HttpURLConnection`, which overrides
`getRequestProperty` and returns the header; ours is `java.net.HttpURLConnection`,
whose inherited `URLConnection.getRequestProperty` begins with `checkConnected()`
and throws. `getRequestProperty` is in the retirement table, so the JDK's
bytecode is exactly what runs — correctly, for the class the receiver actually
is. Closing it means minting the `sun.*` class from `URL.openConnection()`,
which changes the receiver of thirty-one registrations at once.

**`L6HttpLoopbackSweep` row 37 — two `getInputStream()` calls.** HotSpot returns
the same stream and the second read throws because the first consumer closed a
network stream. This VM builds a fresh `ByteArrayInputStream` per call from a
buffered body; caching the object is easy, but `ByteArrayInputStream.close()` is
a no-op in the JDK and this VM does not serve it, so there is nothing to make
the second read fail. It needs a stream class this VM owns.

**`L6HttpLoopbackSweep` row 65 — "too many bytes written".** The JDK throws it
from `FixedLengthOutputStream.write`, at the write. This VM's request body is a
real `java.io.ByteArrayOutputStream` whose writes are the JDK's own bytecode
(see §3), so there is no write to intercept — the overrun is only visible when
the response path counts the bytes, which is after the probe's lambda has
returned. Same missing hook as row 37, from the other end.

**`L6JcaSweep`'s 34 diff lines.** No §9 item names them and this wave did not
open the probe.

---

## 10. The gates

Measured on the merged tree, `6c8ad94d4`, release binary built at 17:05:55Z
from that same revision (the mtime and the rev are printed together because a
failed build does not fail the arms that follow it).

### The cargo set — six configurations, all green

| config | result |
|---|---|
| `cargo test -p cratonvm-types --no-fail-fast` | 0 |
| `cargo test -p cratonvm-native-builtins --tests --no-fail-fast` | 0 |
| `… --features management --tests` | 0 |
| `… --features synthetic-jdk --tests` | 0 |
| `cargo test -p cratonvm-native-api --tests` | 0 |
| `cargo test -p cratonvm-native-io --tests` | 0 |

Two of these were red on the first two attempts and both reds were this
wave's own. They are recorded because each was a wrong first diagnosis.

**`the_drift_baseline_has_no_stale_rows`** — "11 recorded drift pair(s) no
longer drift", every pair `register_phase54_net_extras` × `java/net/HttpURLConnection`.
The cause was §6's macro: emitting the thirty-one `r.register` calls from
`subclass_aware_registrations!` deleted the shipping half of each recorded pair
as far as `registrar_drift.rs` is concerned, because that gate reads the SOURCE
and cannot expand a macro. Regenerating the baseline would have "fixed" it and
thrown away eleven real records. The macro now generates BODIES only and the
registrations stayed literal.

**`raw_lock_constructions_do_not_grow`** — 430 against a baseline of 428, in
all three `native-builtins` configurations. The note this wave inherited said
this was dev's own pre-existing red at 429. That was worth two minutes of
checking and it was wrong in both directions.

The gate is a source scan, so it can be scored at two revisions with no build
by replicating its census (`native-builtins/src/**/*.rs`, skipping `#[cfg(test)]`
regions and comment lines, `OrderedPl*` counted before `Mutex::new`):

```
this branch   raw=429  ordered=191
origin/dev    raw=428  ordered=190   <- GREEN, not 429
delta         native-builtins/src/phases_late/jar_manifest.rs  5 vs 4
```

So `dev` was green and the `+1` was in a file this wave never opened. Two
separate causes, both mine to clear:

* the `SSLParameters` round-trip fix (§8) added one `parking_lot::Mutex` for
  the endpoint-identification table. It is now an `OrderedPlMutex` at
  `LockLevel::Scratch`, which the two acquisition sites justify: each does one
  map operation on a key computed above the acquisition. The getter did not
  compute it above the acquisition until this change — it evaluated
  `ctx.identity_hash_code` inside the lock expression, which is a `ctx` call
  under a guard in the one crate whose natives re-enter the VM.
* the branch was carrying a **stale merge base**. `1c6656f8f` — the lock-ratchet
  lane's own conversion of the jar manifest-sections cache — landed on `dev`
  after this branch last merged. Merging `dev` and re-censusing gives 428,
  exactly the baseline.

The second is the reason the cheap census is worth running after every merge
and not only after every edit: a file this wave never touched moved its number.

### The three script gates

| gate | result | whose |
|---|---|---|
| `jdk-only-census.sh` | 0 | — |
| `jdk-only-refusal-survivors.sh` | 1 | **not this wave's** |
| `bridge-ratchet.sh` | 1 (REFUSED) | **not this wave's** |

`jdk-only-refusal-survivors.sh` reports five rows and every one is a `-`, which
is the direction that means *closed*: `java/util/logging/Handler` `getLevel` /
`setLevel` and `java/util/logging/LogRecord` `getLevel` / `getMessage` /
`getSequenceNumber`, all `intrinsic`. A `+` row would be a new instance of the
species and would be this wave's problem; a `-` row is a baseline nobody
re-froze after the closure. `f9d75ee18` ("campaign-wide survivors are 5 not 0")
names the same five. This branch's diff mentions `java/util/logging` **zero**
times across its seven files, so it cannot have moved them; the re-freeze
belongs to whoever closed them.

`bridge-ratchet.sh` scores its compatible leg (schema 5, 12852 rows) and then
REFUSES the `--jdk-only` leg: *"no committed baseline for 25/linux/jdk-only
(have: 25/linux)"*. The strict registry has never been measured on this image.
A refusal is deliberately not a pass, and it is also not a red this wave can
clear by pushing: creating that baseline freezes 7367 bridge-rows-with-no-target
that several lanes are actively moving, so it wants its own change with its own
`--note`, not a side effect of a net/security wave.

### The three arms

| arm | result |
|---|---|
| `SUITE=all` | **136 passed, 0 failed** |
| `SUITE=core` | **95 passed, 0 failed** |
| `CRATONVM_ARGS=--jdk-only` | 135 passed, 1 failed |

The one failure is `RArrayStoreLibrary`, and it is not a behaviour difference:

```
HARNESS ERROR [G3] RArrayStoreLibrary: publishes no check count, and is not in
regression-suite/harness-uncounted.txt.
```

The vector arrived on `dev` with `16b07c71e` ("fix(natives): Arrays.fill named
a COMPONENT where HotSpot names the VALUE") without emitting its count. This
branch changes **zero** files under `regression-suite/` or `apps/`, so the
vector it runs is byte-identical to dev's. Left for that lane, which is also
the only one that knows what N should be.

### The probes

Two binaries from one worktree, base `46d7b7211`, trial the wave:

| probe | base | trial |
|---|---|---|
| `L6HttpLogicSweep` | 14 | **0** |
| `L6SocketSweep` | 40 | **0** |
| `L6HttpLoopbackSweep` | 16 | 6 |
| `L6TlsParamSweep` | 36 | 22 |
| `L6UriSweep` / `L6UrlSweep` / `L6InetSweep` / `L6X500Sweep` | 0 | 0 |

78 diff lines closed, nothing worse. `L6JcaSweep` is unmoved and §9 records why.
