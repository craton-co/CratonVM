# Lane 6 — networking, TLS and `java.security`: 54 shadows retired, 912 adjudicated, and the corpus took 70 back — RETIRED 2026-09-10

**Retires `docs/known-issues/jdk-only-lanes/lane-6-net-security.md`**, deleted
in the same commit. The method it worked to is
[`jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md);
the ownership boundary is
[`lane-0-integration-and-gates.md`](../../known-issues/jdk-only-lanes/lane-0-integration-and-gates.md).
That page opened on 2026-09-10 as one of the nine-lane split's scope sheets. It
retires here because its §8 "done" condition is met: every bucket-A/B row in its
prefix set is now either retired, classified C/D/E/F, or **blocked with the
blocker named** — and the blockers are named with the measurement that found
them, not with a guess.

| | |
|---|---|
| **Scope as opened** | "819 §1.4 shadows over 90 classes, from 663 registration sites." |
| **Scope as measured** | **966** bucket-A/B shadows over **99** classes, from **693** sites. The page's numbers were a day old; `dev` moved. |
| **Retired** | **54 triples** — `java/net/URI` (29), `java/net/HttpURLConnection` (14), `ProxySelector.getDefault` (1) and `X500Principal` (10). |
| **Adjudicated, not retired** | **912**, every one with a verdict and a measurement (§5). 70 of them are rows the probe tree cleared and the CORPUS refused — §2.1, and the most useful paragraph on this page. |
| **Probe rows this closes** | **201** of the 496 differing rows the lane's eight new probes find against HotSpot. |
| **Where** | Azure host 2 (`20.80.105.49`), branch `claude/l6-net-security-20260910` off `origin/dev` `7a8b79526`, JDK 25.0.4+7 image, HotSpot 25.0.4+7 as oracle. Host load ranged 38-190 throughout; every number here is a correctness delta, never a timing. |

---

## 1. What was actually wrong, and it is the perimeter every time

The lane page predicted this and the operations page has predicted it five
families running: **not one of the defects below is a wrong answer to an
ordinary call.** Eight new differential probes, 3,183 rows between them, found
**496 differing rows** in `--jdk-only` mode on an unmodified `dev`:

| probe | rows | differing before |
|---|---:|---:|
| `L6UriSweep` | 925 | 40 |
| `L6InetSweep` | 848 | 190 |
| `L6X500Sweep` | 401 | 138 |
| `L6UrlSweep` | 446 | 7 |
| `L6HttpLogicSweep` | 164 | 32 |
| `L6JcaSweep` | 194 | 36 |
| `L6TlsParamSweep` | 114 | 33 |
| `L6SocketSweep` | 91 | 20 |

The shapes, and every one of them is a check that was **missing** or a refusal
that was **wrong**:

* **Validation that never ran.** `DatagramSocket.connect(addr, 65536)` was
  accepted. So were `connect(null, 9)`, `bind` on a socket already bound,
  `MulticastSocket.setTimeToLive(-1)` and `setTimeToLive(256)`,
  `joinGroup(null)`, `joinGroup` on a non-multicast address, and
  `HttpURLConnection.setRequestMethod("CONNECT")` — which the JDK rejects by
  name. `setRequestMethod("get")` was silently upper-cased to `GET` where the
  JDK throws.
* **State machines with no state.** `setFixedLengthStreamingMode` after
  `connect()` was accepted; so was `setRequestProperty` after `connect()`,
  where the JDK throws `IllegalStateException`. `Cipher.getOutputSize` answered
  16 before `init`, where the JDK throws.
* **A mutable view handed out.** `URLConnection.getRequestProperties()`
  returned a **modifiable** map, and its value lists were modifiable too.
  `setRequestProperty` did not REPLACE what `addRequestProperty` had appended —
  `set K=3` after `add K=2` left `[3, 4]` where the JDK leaves `[4]`.
* **Parsers that stop early.** `X500Principal` did not recognise the `#hex` DER
  form of an attribute value at all (it escaped the `#` and stored the literal
  text), dropped a trailing escaped space, mis-escaped RFC 1779 output, and
  encoded `emailAddress` as a `UTF8String` (tag `0c`) where the JDK writes an
  `IA5String` (tag `16`). `URI` accepted a second `#` inside a fragment. The
  RFC-850 and asctime `Date` header formats parsed as `-1`.
* **Answers that depend on the host.** `InetAddress.getByName("256.1.1.1")` and
  `getByName("1:2:3:4:5:6:7:8:9")` were sent to the **resolver**; HotSpot
  rejects both as malformed literals without a lookup. Those rows were not
  merely wrong, they were a function of this host's DNS — a wildcard resolver
  would have changed the answer. `0x7f.0.0.1` was accepted as an IPv4 literal;
  a modern JDK rejects the legacy hex form.
* **Default-port equality.** `new URL("http://h:80/p").equals(new URL("http://h/p"))`
  answered `false`. The default port makes them equal.
* **A key-wrap round trip that does not round-trip.** `Cipher.WRAP_MODE` under
  AES-128-ECB with an all-zero key produced `140f0f10…` where the JDK produces
  `66e94bd4…` (the published AES-128 zero-key zero-block vector), and
  `UNWRAP_MODE` returned `af65bb47…` rather than the key that went in. This one
  is **not fixed by this wave** — see §5's `BLOCKED` bucket — and it is the
  most serious single row the lane found.
* **AES-GCM tag lengths.** `new GCMParameterSpec(96, iv)` produced a 128-bit
  tag; `GCMParameterSpec(8, iv)` was accepted where the JDK refuses every value
  outside `{128, 120, 112, 104, 96}`.
* **`AbstractMethodError` in the TLS surface.**
  `SSLSocket.getEnableSessionCreation()` raised
  `method javax/net/ssl/SSLSocket.getEnableSessionCreation()Z has no Code attribute`
  on an unconnected socket.

## 2. The retirement: 54 triples, and the 70 the corpus took back

`RETIRED_SHADOW_L6_TRIPLES` in `native-api/src/retired_shadow.rs`. Two prefixes
admitted, both the narrow spelling: `java/net/` and
`javax/security/auth/x500/`.

```text
  29  java/net/URI                  14  java/net/HttpURLConnection
  10  javax/security/auth/x500/X500Principal
   1  java/net/ProxySelector.getDefault
```

Measured on a **paired pair of binaries built from the same merged tree**,
differing only in this one file — the control has the table reverted to
`origin/dev`'s version, nothing else:

```text
  L6X500Sweep       276 diff lines -> 0      (401 rows,  4,669 yields)
  L6UriSweep         80            -> 0      (925 rows,  7,179 yields)
  L6HttpLogicSweep   64            -> 18     (164 rows,  2,913 yields)
  every other probe in the 125-probe tree: delta exactly 0
```

### 2.1 The probe tree said 124. The corpus said 54.

**This is the finding of the wave** and it re-states one of the four
preconditions, so it goes before the good news rather than after it.

The first table carried 124 rows: the four classes above plus `java/net/URL`
(16), `DatagramSocket` (30), `MulticastSocket` (6), `InetAddress` (3),
`Inet4Address` (8) and `Inet6Address` (7). Both instruments cleared it:

* armed on the two prefixes, **all 125 probes in the tree got no worse** and
  five got dramatically better;
* **built**, and run as a two-binary A/B against a control from the same tree,
  the same five improved (`L6UriSweep` 80 → 0, `L6X500Sweep` 276 → 0,
  `L6HttpLogicSweep` 64 → 18, `L6InetSweep` 380 → 368, `L6SocketSweep` 42 →
  38) and nothing regressed. One probe moved the wrong way by 4 lines and it
  was `VtHandoffProbe`, the campaign's named noise floor.

The `--jdk-only` corpus, on that same pair of binaries, went from **132 of 132
to 126 of 132**:

```text
  RJdkServices               NPE: URLStreamHandler.openConnection, "this.handler" is null
  RServiceLoaderDoubleSource    (same)
  RJdkDefineClass            NPE: URLStreamHandler.getDefaultPort,  "this.handler" is null
  RJdkNet                    UnsatisfiedLinkError: sun/nio/ch/DatagramChannelImpl.receive0
  RJdkNet   (InetAddress)    UnsatisfiedLinkError: java/net/Inet6AddressImpl.lookupAllHostAddr
  RNetIfaceScope             AssertionError: every scoped IPv6 address must round-trip, 2 did not
```

Six vectors, one species. **Precondition 3 asks whether the IMAGE METHOD
carries `Code` to yield to. All 124 rows passed it. What it does not ask is
what that `Code` then calls.**

* `java.net.URL`'s methods are one line each — `handler.openConnection(this)`,
  `handler.getDefaultPort()`, `handler.equals(this, u)`. `handler` is written
  only by the real constructor, and this VM **mints** `URL` objects in
  `classloader.rs` without running it. Yielding turns every one of them into an
  NPE. This is `Class.getModule`'s situation exactly (lane-0 §7): a field only
  a real VM writes.
* `InetAddress.getByName` yields to bytecode that calls
  `Inet6AddressImpl.lookupAllHostAddr`, which is `ACC_NATIVE` and which this VM
  does not implement. `DatagramSocket` yields to bytecode that routes through
  `sun.nio.ch.DatagramChannelImpl.receive0`, likewise. The retirement trades a
  shadow for an `UnsatisfiedLinkError` — which is precisely what precondition 3
  exists to prevent, one frame deeper than it looks.

**So precondition 3 is really: the image method carries `Code`, AND that code's
own callees are satisfiable in this VM.** The cheap approximation of the second
half is the corpus. The probe tree cannot substitute for it, and this wave is
the proof: 125 probes and a two-binary A/B both scored 124 rows clean.

That also settles what the lane page called *"`URLStreamHandler` NPE, 2
vectors, yours right now."* The page was right that it exists and wrong that it
was already failing: it is **latent**, and retiring `java/net/URL` is what
wakes it.

### 2.2 How the six were attributed, in about twenty minutes and no rebuild

Arming one class at a time on the CONTROL binary with
`CRATONVM_ENFORCE_NATIVE_SHADOW` and running only the six failing vectors —
eleven scopes by six vectors, sixty-six VM runs.

Two traps it walked into and out of, both already on the operations page:

* **`rc` is 0 when a vector fails.** The VM reports the failure in its own
  summary line (`main-vm run() returned Err`), and the first version of the
  bisect keyed on the exit code and called all six green. *Check `rc` before
  believing a harness label* cuts both ways.
* **The dial is not the table.** `RNetIfaceScope` does **not** reproduce under
  the dial with all three `Inet*` classes armed, and does fail on the built
  trial binary. The dial declines at DISPATCH and arms a PREFIX;
  `retired_shadow.rs` re-tags at REGISTRATION and is per-triple, so a
  constructor or a class-initialiser path can differ. A dial result is a lead
  in both directions, never a verdict.

`RSslLiveSession` was in the trial arm's failing set and is **not** one of the
six. What is measured about it here, and all that is measured: it passed 11 of
11 runs on the control binary during the bisect — with nothing armed and with
each of ten scopes armed — and it failed once, in an arm that ran concurrently
with a 125-probe A/B and a five-configuration `cargo test`. It is a live TLS
handshake vector, and the operations page §6 records an unnamed TLS vector
whose client loop discards the reply it asserts on when three records arrive in
one read under load. That may or may not be this one; **this page does not
claim it is**, and the honest statement is that `RSslLiveSession` is not
reproducible as a consequence of this wave. `RNetIfaceScope` looked the same
and is not the same: it reproduces alone, on the trial binary, every time.

### 2.3 The dial is wrong in BOTH directions, and this wave measured both

The narrowed 54-row set was pre-checked by arming its four scopes on the
control binary and running the whole `--jdk-only` corpus. It came back **130 of
132**, with two failures the first table never had:

```text
  RJdkBridge1        AssertionError: URL.toURI().getPath() must carry the lone
                     surrogate at 2, got charAt(2)=fffd in: /a<fffd>b
  RJdkX509Intercept  NoSuchMethodError: java.lang.String.getRFC2253Name()
                       at javax/security/auth/x500/X500Principal.getName(X500Principal.java:318)
```

Both look like exactly the species §2.1 is about, and the second is almost
persuasive: this VM stores a **String** in `X500Principal`'s single declared
slot, `transient X500Name thisX500Name` (`native-builtins/src/jca/x500.rs`
documents the repurposing), so JDK bytecode that calls
`thisX500Name.getRFC2253Name()` on it is a `NoSuchMethodError` waiting to
happen.

**Neither is real.** Both vectors pass on the BUILT binary that carries those
very rows — the 124-row trial binary contains all 29 `URI` rows and all 10
`X500Principal` rows, and `RJdkBridge1` and `RJdkX509Intercept` are not in its
six failures. Run directly, one vector at a time, on the control binary and on
that trial binary, all four runs are clean.

The reason is the one the operations page gives and this lane has now hit three
times: **the dial DECLINES at dispatch and arms a PREFIX; `retired_shadow.rs`
re-tags at REGISTRATION and is per-TRIPLE.** Arming `javax/security/auth/x500/`
declines every dispatch on the package, including on principals the VM minted
itself with a String in that slot and including the one `X500Principal`
registration the table does not carry. The table refuses 10 named
registrations, and instances built through the retired constructor get a real
`X500Name`.

So this lane measured the dial wrong in both directions within one wave:

* **optimistic** — `java/net/URL`, `DatagramSocket` and the `Inet*` family
  scored clean on every probe under the dial and broke six corpus vectors when
  built (§2.1);
* **pessimistic** — `java/net/URI` and `X500Principal` broke two corpus vectors
  under the dial and are clean when built.

A dial result is a lead. The verdict is a built binary.

### 2.4 The four preconditions, and what each removed

Applied per triple against a dump from a run of **the very probes whose
improvement is cited above** — never against a corpus census, which is a
different workload. (That is the `FileChannelImpl.open` mistake recorded on
`RETIRED_SHADOW_PHASE2_TRIPLES`: a whole build spent retiring the one triple
the corpus had dispatched and the probe never touched.)

```text
  owns the slot + effective kind Bridge   -34 not-owner, -55 already retagged
  bucket A or B (something to yield to)   -91 C/D/F
  dispatched by the instrument (inv > 0) -127 never reached
  registrar not held by lane T             -4 the throwable ctor table
  the corpus tolerates it                 -70 §2.1
```

### 2.5 The refusals are not inert

A refusal is a retirement **only when nothing already owns the triple**.
`NativeMethodRegistry::register_inner` refuses a `SyntheticStub` under
`--jdk-only` without inserting it; `JdkOnlyViolation::SyntheticNativeRegistered`
carries a `survivor`, and a non-null one means an earlier registration is still
in the slot and still serving — strict mode then runs that older native, every
probe reads exactly as before, and the wave is a no-op that looks like a clean
result.

Measured on the trial binary, over a `--jdk-only-report` of the probe tree:
**every row of the table appears in the refusal set, and ZERO refusals carry a
survivor.** The refusal census counts more triples than the table has rows —
it is a property of the PREFIX, and `java/net/` already contained registrations
that were `SyntheticStub` before this wave (`PlainServerSocketImpl`,
`InetAddressImplFactory`). Twelve of the triples are registered more than once,
and each registration is refused separately.

## 3. Why the other seven prefixes are not retirable, with the arm that says so

Each was armed **alone** on the same 125-probe tree.

| arm | probes worse | verdict |
|---|---:|---|
| `java/net/` | 0 | **TAKEN**, 114 rows |
| `javax/security/auth/x500/` | 0 | **TAKEN**, 10 rows |
| `javax/net/` | 2 | refused — `L6TlsParamSweep` 66 → 94 diffs |
| `java/security/`, `sun/security/`, `javax/crypto/`, `javax/security/` | 8 | refused — `SecuritySurfaceSweep` 0 → 2,594 |
| `sun/net/` | 0 | refused — **123 of 125 probes VACUOUS** |
| `jdk/net/`, `jdk/internal/net/` | 0 | refused — **123 of 125 probes VACUOUS** |

**A vacuous arm is not a pass.** `sun/net/` and the two `jdk` prefixes reached
the enforcement dial in 2 probes of 125; arming them changed nothing and read
as the best possible result. That is precondition 1, and the trap 146 of Phase
2's 236 candidates fell into.

Two rows moved in these arms and are **not** evidence, both flagged by the
driver itself:

* `VtHandoffProbe` is the campaign's named noise floor. It counts virtual-thread
  handoffs and `allJoined`, both nondeterministic on this VM, and it oscillates
  in both directions. It scored −14 in the `java/net/` arm and −10 in two
  others; nothing in this wave touches virtual threads.
* `L4FileSweep` (+83) and `L4FilesSweep` (+119) in the X500 arm reported a delta
  beside `y/r=0/0` — the dial was never asked, so the arming **cannot** have
  caused it. Both re-ran **3/3 clean, 0 diffs, in both arms** at lower host
  load. They were the load artefact the operations page warns about, and the
  driver's own vacuity column is what made them cheap to dismiss.

### 3.1 The TLS stack is an implementation, not a shadow

This is the lane's largest structural finding and it re-prices §1.4 for 557
rows.

This VM's TLS is **rustls**: `native-builtins/src/t27_tls.rs` is 20,630 lines,
plus `net_phase_e.rs`'s `register_re6_ssl_context` and
`http_url_connection.rs`'s HTTPS half. `docs/internal/jdk-only/D3-3-rustls-cipher-names-reaching-jsse.md`
records the seam in detail — the VM even translates rustls's `TLS13_*` suite
spelling into JSSE's `TLS_*` registry names on the way out.

Yielding those rows to `sun.security.ssl` bytecode does not restore a JDK
behaviour this VM was approximating. It removes TLS. Armed:

```text
  SecuritySurfaceSweep     0 diffs -> 2594
  L6JcaSweep              72       ->  216
  JcaFunctional            0       ->   30
  KeyStoreTypeProbe        0       ->    8
  DhAgree                  produces 7 lines -> 1
  SunJceServices           17 lines -> 13, diffs 10 -> 28
```

This is `StrictMath`'s situation, and the operations page states the rule: **a
0-diff probe is a precondition for retiring a shadow, never a justification on
its own.** A native may exist because it IS the implementation. The right
disposition for these 557 rows is not retirement and it is not a reviewed
`Intrinsic` either — it is a contract question about whether §1.4 should
classify a first-class implementation as a shadow at all, and that question
belongs to L0, not here.

**The observable cost of leaving them is recorded above in §1**, and it is not
zero: the cipher-suite list is 15 entries where HotSpot offers 31, the
supported-protocol list omits `SSLv2Hello`/`SSLv3`/`TLSv1`, `SSLParameters`
argument validation is largely absent, and `Cipher` key wrapping does not round
trip. Those are defects to fix **in the natives**, and this page names them so
the next lane starts from a list rather than a probe run.

### 3.2 The `HttpsURLConnectionImpl` rows are a null-`delegate` workaround

67 rows, the largest single class in the lane, and they read exactly like a
shim: `HttpsURLConnectionImpl` inherits its whole surface and 26 of the
registrations come from one call site.

They are the opposite of a shim. `register_https_delegate_forwarders`
(`native-builtins/src/http_url_connection.rs`) exists because this VM
**allocates** that carrier rather than constructing it, so its `delegate` field
is null, and essentially every method the class declares is
`getfield delegate; invokevirtual …`. Its own doc records the failure that
motivated it:

> `NullPointerException: Cannot invoke "sun.net.www.protocol.https.DelegateHttpsURLConnection.setUseCaches(boolean)" because "this.delegate" is null`

— the whole SSL/TLS and OCSP portion of the Tomcat suite. Each forwarder runs
the **superclass body the Impl overrides**, which is to say the retirement's own
remedy is already what these natives do, one level up. Retiring them restores
the NPE.

### 3.3 A base-class row loses every dispatch to its own subclass

`java/net/InetAddress` has 18 bucket-A rows and exactly **3** are ever
dispatched: `getByName`, `getByAddress` and `getLoopbackAddress`, all statics.
Every instance is an `Inet4Address` or an `Inet6Address`; the dispatch door asks
the registry about the **declaring class**; the subclass registration wins. The
other 15 — `equals`, `getHostAddress`, `isLoopbackAddress`, `toString`, … — are
unreachable through instance dispatch and retiring them would move nothing.

They are counted here as **unretired** rather than quietly claimed. A wave that
counted them would report 139 retirements for the same behaviour change.

## 4. What is retired

44 rows under `java/net/`:

```text
  29  java/net/URI                   14  java/net/HttpURLConnection
   1  java/net/ProxySelector.getDefault
```

10 under `javax/security/auth/x500/`: the whole of `X500Principal` — four
constructors, `equals`, `hashCode`, `toString`, `getEncoded` and both
`getName` overloads.

The tenth X500 row is worth a sentence, because it is the one precondition 4
would otherwise have cost. `X500Principal.<init>(String, Map)` is reached by no
other row and the first version of `L6X500Sweep` never called it, so it read as
`invocations: 0` and would have been dropped — leaving nine tenths of one class
retired for no reason a reader could reconstruct. Seven rows were added to the
probe to reach it. **A family that is retired 9/10 is a family whose next reader
has to re-derive why.**

`java/net/URI` is 29 of its 30 rows. The one left out is
`compareTo(Ljava/lang/Object;)I`, the `Comparable` bridge, which the probe
reaches through `compareTo(URI)` and never through the erased signature, so it
reads `invocations: 0`. It is a single synthetic forwarder and it is left
unretired rather than claimed on an argument.

`java/net/HttpURLConnection` is 14 of its 30. The other 16 are the
connection-state family — `getInputStream`, `getResponseCode`,
`getHeaderField(s)`, `getContentLength` — which no probe in this tree
dispatched; §5.1.

## 5. The 912 that stay, every one with a verdict

Adjudicated by `scripts`-free analysis of one
`--dump-native-registry --explain-jdk-only` dump plus the eight per-probe dumps,
over all **1,414** registrations under the lane's nine prefixes. Every row gets
exactly one verdict; "the rest" is not a classification.

| rows | verdict |
|---:|---|
| 557 | **BLOCKED** — measured to break the rustls TLS / JCA stack (§3.1) |
| 176 | **C** — declared abstract or on an interface; no door dispatches it |
| **54** | **RETIRED** |
| 104 | **UNOBSERVED** — no probe in this tree dispatched it (§5.1) |
| 70 | **KEEP** — the corpus refused the retirement (§2.1) |
| 86 | **BLOCKED** — the arm was VACUOUS; the dial was never asked (§3) |
| 84 | **LOSER** — another registration owns the slot; the edit would be inert |
| 73 | **KIND** — already `SyntheticStub`, outside the mechanism |
| 67 | **KEEP** — super-forwarder for a null `delegate` (§3.2) |
| 40 | **D** — the image method is `ACC_NATIVE`; `Bridge` is CORRECT per §1.5 |
| 33 | **F** — class present, method absent, matches nothing |
| 33 | **KIND** — already `Intrinsic`, exempt at every door |
| 28 | **LANE-T** — the cross-lane throwable registrar and its siblings |
| 9 | **E** — no such class in the JDK image |

The goal population reconciles exactly: `54 + 70 + 557 + 86 + 67 + 104 + 28 = 966`.

### 5.1 The 104 unobserved rows, and what it would take to reach them

This is the softest category on the page and it is stated as such. A row here is
not adjudicated safe; it is **unmeasured**, because precondition 4 asks for a
dispatch seen by the instrument that produced the improvement, and no probe in
this tree produced one.

```text
  16  java/net/HttpURLConnection   getContentLength, getContentLengthLong,
                                   getErrorStream, getHeaderField(s), getInputStream,
                                   getOutputStream, getResponseCode, getResponseMessage, ...
  15  java/net/InetAddress         the 15 that lose to Inet4Address/Inet6Address (§3.3)
  12  java/net/URLClassLoader      five constructors, addURL, findClass, close, ...
   7  java/net/MulticastSocket     send, receive, leaveGroup, setSoTimeout, ...
   7  java/net/URLConnection       the same connection-state family as above
   7  java/net/http/HttpClient     newBuilder, shutdown, awaitTermination, ...
```

Three different reasons sit behind those six lines:

1. **15 are structurally unreachable** (§3.3) and no probe can fix that.
2. **30 need a live peer** — the `HttpURLConnection`/`URLConnection`
   connection-state family and `HttpClient`. The lane page is explicit that a
   live handshake is the corpus's instrument and not a probe's, and
   `L6HttpLogicSweep` is built around a `HttpURLConnection` subclass with a
   fixed header fixture precisely to avoid needing one. The cost of that choice
   is exactly these rows.
3. **12 are `URLClassLoader`**, which is L7's `jdk/internal/loader/` story
   wearing a `java/net/` prefix. They should move to L7 rather than be probed
   here.

`L6HttpLogicSweep` retains **60 differing rows** after the retirement, and the
largest group is (2): asked for `getContentLength()` on a subclass that
overrides every header accessor, CratonVM attempts a real transport and throws
`IOException: HttpURLConnection response failed: resolve fixture.invalid:80`
where HotSpot answers `1234` from the fixture. The registry records
`invocations: 0` on both `getContentLength` triples in that run with
`invocations_complete: true`, so the transport attempt is reached through some
other row — and finding which is the first thing the next wave on this class
should do.

## 6. What the lane page said and what turned out to be true

* *"Much of this lane is blocked behind L7."* — **No longer true, and the page
  says to re-confirm.** The `--jdk-only` corpus is **132 of 132 passing** on
  `dev` `7a8b79526`; the `ServiceLoader.checkCaller` failure the page describes
  is fixed, and `BuiltinClassLoader` no longer blocks ten vectors. What blocks
  the provider/algorithm rows is not L7 — it is §3.1.
* *"`URLStreamHandler` NPE, 2 vectors, yours right now."* — **Half right, and
  the half it got wrong is the interesting one.** No corpus vector fails today,
  so there was nothing to fix; but the NPE is real and LATENT, and retiring
  `java/net/URL` wakes it in three vectors. See §2.1.
* *"`URI` (30 rows) — the best mechanical wave here."* — **Correct, and it was.**
  80 diff lines to zero, and the 30 rows include the `<init>` that 970 of the
  probe's dispatches go through.
* *"`InetAddress` deserves care; ask only structural questions."* — **Correct,
  and it was load-bearing.** The probe asks no DNS question, and the finding is
  precisely that *CratonVM* does.
* *"Retire the TLS stack base-class-first."* — **Overtaken.** The ordering advice
  presumes the stack is retirable at all; §3.1 and §3.2 say it is not, for two
  independent reasons.
* *"`URLStreamHandler` NPE, 2 vectors, yours right now."* — the second half of
  that entry, corrected. No vector fails today, and the page read that as the
  work being available. It is **latent**: retiring `java/net/URL` wakes it in
  three vectors, not two. §2.1.
* *"`javax/net/`, `javax/crypto/`, `javax/security/` may not be in
  `RETIRED_SHADOW_PREFIXES` yet."* — **True, and they still are not**, now
  deliberately. `the_lane_l6_security_and_tls_prefixes_are_not_admitted` pins
  that as a measured verdict rather than an omission.
* *"Check `rc` before believing a harness label."* — **Earned its place.** Two
  `delta > 0` rows in the X500 arm carried `y/r=0/0`, which is the driver saying
  the arming could not have caused them; both re-ran clean 3/3.

## 7. The gates

<!-- GATES-PLACEHOLDER -->

## 8. What this leaves for the next lane

1. **The TLS/JCA contract question (557 rows).** Is a first-class rustls
   implementation a §1.4 shadow? If it is not, those rows leave the goal
   population and the campaign denominator changes. That is L0's call.
2. **The defects §1 lists inside the blocked set** are fixable in the natives
   today, without waiting for that answer. In rough order of severity:
   `Cipher` WRAP/UNWRAP does not round-trip; `GCMParameterSpec` tag lengths are
   ignored and not validated; `AES/CTR/NoPadding` resolves to no provider;
   `SSLSocket.getEnableSessionCreation` raises `AbstractMethodError`;
   `SSLParameters.setApplicationProtocols` accepts null and empty elements.
3. **The 30 connection-state rows** need either a loopback HTTP fixture in the
   probe tree or a corpus vector that asserts them.
4. **`java/net/URLClassLoader` (12 rows) should move to L7** by amending
   lane-0 §2's ownership table.
5. **The four `MalformedURLException`/`UnknownHostException` rows** unblock the
   moment lane T releases the throwable registrar.
