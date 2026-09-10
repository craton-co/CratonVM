# Lane 6 — networking, TLS and `java.security`: 124 shadows retired, 842 adjudicated — RETIRED 2026-09-10

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
| **Retired** | **124 triples** — `java/net/` (114) and `javax/security/auth/x500/X500Principal` (10). |
| **Adjudicated, not retired** | **842**, every one with a verdict and a measurement (§5). |
| **Probe rows this closes** | **297** of the 496 differing rows the lane's eight new probes find against HotSpot. |
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

## 2. The retirement: 124 triples, and how each earned its row

`RETIRED_SHADOW_L6_TRIPLES` in `native-api/src/retired_shadow.rs`. Two prefixes
admitted, both the narrow spelling: `java/net/` and
`javax/security/auth/x500/`.

Armed on those two prefixes, against HotSpot as oracle, over the **whole
125-probe tree** — not the families' own probes, which are the narrowest
instrument in the building:

```text
  L6UriSweep         80 diff lines -> 0      ( 7,179 yields)
  L6UrlSweep         14            -> 0      (10,555 yields)
  L6X500Sweep       276            -> 0      ( 4,669 yields)
  L6InetSweep       380            -> 160    ( 1,156 yields)
  L6HttpLogicSweep   64            -> 60     ( 2,913 yields)
  every other probe in the tree            delta exactly 0
```

Three probes go to **zero**. That is the shape §1.4's remedy is supposed to
have: the JDK's own RFC-3986 parser, its own RFC-2253 parser and its own
`URLStreamHandler` are more exact than any of the hand-written natives in front
of them, and yielding is not a workaround for them but the point.

### The four preconditions, and what each removed

Applied per triple against a dump from a run of **the very probes whose
improvement is cited above** — never against a corpus census, which is a
different workload. (That distinction is not pedantry: it is the
`FileChannelImpl.open` mistake recorded on `RETIRED_SHADOW_PHASE2_TRIPLES`, a
whole build spent retiring the one triple the corpus had dispatched and the
probe never touched.)

```text
  owns the slot + effective kind Bridge   -34 not-owner, -55 already retagged
  bucket A or B (something to yield to)   -91 C/D/F
  dispatched by the instrument (inv > 0) -127 never reached
  registrar not held by lane T             -4 the throwable ctor table
```

### The refusals are not inert

A refusal is a retirement **only when nothing already owns the triple**.
`NativeMethodRegistry::register_inner` refuses a `SyntheticStub` under
`--jdk-only` without inserting it; `JdkOnlyViolation::SyntheticNativeRegistered`
carries a `survivor`, and a non-null one means an earlier registration is still
in the slot and still serving — strict mode then runs that older native, every
probe reads exactly as before, and the wave is a no-op that looks like a clean
result.

Measured on the trial binary, over a `--jdk-only-report` of the probe tree:

<!-- REFUSALS-PLACEHOLDER -->

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

114 rows under `java/net/`:

```text
  30  java/net/DatagramSocket        16  java/net/URL
  29  java/net/URI                   14  java/net/HttpURLConnection
   8  java/net/Inet4Address           7  java/net/Inet6Address
   6  java/net/MulticastSocket        3  java/net/InetAddress
   1  java/net/ProxySelector
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

## 5. The 842 that stay, every one with a verdict

Adjudicated by `scripts`-free analysis of one
`--dump-native-registry --explain-jdk-only` dump plus the eight per-probe dumps,
over all **1,414** registrations under the lane's nine prefixes. Every row gets
exactly one verdict; "the rest" is not a classification.

| rows | verdict |
|---:|---|
| 557 | **BLOCKED** — measured to break the rustls TLS / JCA stack (§3.1) |
| 176 | **C** — declared abstract or on an interface; no door dispatches it |
| **124** | **RETIRED** |
| 104 | **UNOBSERVED** — no probe in this tree dispatched it (§5.1) |
| 86 | **BLOCKED** — the arm was VACUOUS; the dial was never asked (§3) |
| 84 | **LOSER** — another registration owns the slot; the edit would be inert |
| 73 | **KIND** — already `SyntheticStub`, outside the mechanism |
| 67 | **KEEP** — super-forwarder for a null `delegate` (§3.2) |
| 40 | **D** — the image method is `ACC_NATIVE`; `Bridge` is CORRECT per §1.5 |
| 33 | **F** — class present, method absent, matches nothing |
| 33 | **KIND** — already `Intrinsic`, exempt at every door |
| 28 | **LANE-T** — the cross-lane throwable registrar and its siblings |
| 9 | **E** — no such class in the JDK image |

The goal population reconciles exactly: `124 + 557 + 86 + 67 + 104 + 28 = 966`.

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
* *"`URLStreamHandler` NPE, 2 vectors, yours right now."* — **Gone.** No corpus
  vector fails. `grep -rl URLStreamHandler` finds no test asserting it. The
  entry was already stale when the page was written the same day.
* *"`URI` (30 rows) — the best mechanical wave here."* — **Correct, and it was.**
  80 diff lines to zero, and the 30 rows include the `<init>` that 970 of the
  probe's dispatches go through.
* *"`InetAddress` deserves care; ask only structural questions."* — **Correct,
  and it was load-bearing.** The probe asks no DNS question, and the finding is
  precisely that *CratonVM* does.
* *"Retire the TLS stack base-class-first."* — **Overtaken.** The ordering advice
  presumes the stack is retirable at all; §3.1 and §3.2 say it is not, for two
  independent reasons.
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
