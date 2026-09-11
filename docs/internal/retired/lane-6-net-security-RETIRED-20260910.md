# Lane 6 — networking, TLS and `java.security`: 15 shadows retired of 966, and the six builds it took to find out which — RETIRED 2026-09-11

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
| **Retired** | **15 triples** — `java/net/HttpURLConnection` (14) and `ProxySelector.getDefault` (1). Verified at 132/132 against a control built from the same tree. |
| **Adjudicated, not retired** | **951**, every one with a verdict and a measurement (§5). **109 of them are rows the probe tree cleared and the corpus refused** — §2 is the ladder, and it is the most useful part of this page. |
| **Probe rows this closes** | **22** of the 496 differing rows the lane's eight new probes find against HotSpot. The other 474 are measured, catalogued in §1, and left to the work §8 names. |
| **Where** | Azure host 2 (`20.80.105.49`), branch `claude/l6-net-security-20260910` off `origin/dev`, merged with `dev` at `0d18c01bd`, JDK 25.0.4+7 image, HotSpot 25.0.4+7 as oracle. Host load ranged 14-190 and the filesystem hit 100% twice; every number here is a correctness delta, never a timing. |

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

## 2. The ladder: six builds, and what each one measured out

**This is the finding of the wave.** The probe tree cleared 124 rows. The
corpus refused 109 of them, one class at a time, across five more builds. Every
refusal has a named mechanism, and they are all the same species.

| # | table | corpus, paired and alone | what it measured out |
|---|---|---|---|
| 1 | **124** rows, 10 classes | trial 126/132, ctrl 132/132 | `URL` (16), `DatagramSocket` (30), `MulticastSocket` (6), `InetAddress` (3), `Inet4Address` (8), `Inet6Address` (7) |
| 2 | **54**, 4 classes | trial 130/132, ctrl 132/132 | `URI` (29) — `RJdkBridge1` |
| 3 | **25**, 3 classes | `RSslLiveSession` 3/3 red | nothing — `HttpURLConnection` dropped on a guess, and it was innocent |
| 4 | **11**, 2 classes | `RSslLiveSession` 3/3 red | nothing — the guess again |
| 5 | **10**, `X500Principal` alone | `RSslLiveSession` 3/3 red | `X500Principal` (10) |
| 6 | **15**, `HttpURLConnection` + `ProxySelector` | **trial 132/132, ctrl 132/132** | — |

Builds 3 and 4 are in the table because they are the cost of guessing. After
build 2 the remaining failure was `RSslLiveSession`, an HTTPS vector, and
`HttpURLConnection` was the obvious suspect; it was dropped twice before build
5 tested `X500Principal` on its own and found it sufficient. **Two builds and
about ninety minutes bought nothing, and the class they blamed went back in at
build 6 and is one of the two this lane retires.**

### 2.1 Precondition 3 is checked one frame too high

Every one of the six refusals is the same species:

```text
  RJdkServices                NPE: URLStreamHandler.openConnection, "this.handler" is null
  RServiceLoaderDoubleSource     (same)
  RJdkDefineClass             NPE: URLStreamHandler.getDefaultPort,  "this.handler" is null
  RJdkNet                     UnsatisfiedLinkError sun/nio/ch/DatagramChannelImpl.receive0
  RJdkNet   (InetAddress)     UnsatisfiedLinkError java/net/Inet6AddressImpl.lookupAllHostAddr
  RNetIfaceScope              AssertionError: every scoped IPv6 address must round-trip, 2 did not
  RJdkBridge1                 URL.toURI().getPath() must carry the lone surrogate, got U+FFFD
  RSslLiveSession             CK client.responseCode = 200 unclassified
```

**Precondition 3 asks whether the IMAGE METHOD carries `Code` to yield to. All
124 rows passed it. It does not ask what that code then CALLS.**

* `java.net.URL`'s methods are one line each — `handler.openConnection(this)`,
  `handler.getDefaultPort()`, `handler.equals(this, u)`. `handler` is written
  only by the real constructor, and this VM **mints** `URL` objects in
  `classloader.rs` without running it.
* `javax.security.auth.x500.X500Principal` has exactly one declared instance
  field, `transient X500Name thisX500Name`, and
  `native-builtins/src/jca/x500.rs` documents in its own header that this VM
  **repurposes that slot to hold a String**. Any JDK bytecode on the class
  dereferences a String as an `X500Name`. `RSslLiveSession` reaches it through
  the TLS peer certificate's `getSubjectX500Principal()`.
* `java.net.URI` is the same shape one level out: `URL.toURI()` allocates a
  real `java.net.URI` carrier and publishes fields into it rather than running
  its constructor, and the native's own comment (`G75-1 N1`) records why — a
  string that has been through `read_string` cannot hold an unpaired surrogate.
* `InetAddress.getByName` yields to bytecode that calls
  `Inet6AddressImpl.lookupAllHostAddr`, and `DatagramSocket` to bytecode that
  routes through `sun.nio.ch.DatagramChannelImpl.receive0`. Both are
  `ACC_NATIVE` and neither is implemented here, so the retirement trades a
  shadow for an `UnsatisfiedLinkError` — the exact failure precondition 3
  exists to prevent, one frame deeper than it is checked.

**So precondition 3 is really: the image method carries `Code`, AND that
code's own callees are satisfiable in this VM.** Four of the six are a field
only a real constructor writes, which is `Class.getModule`'s situation from
lane-0 §7 — and that is not a coincidence. A VM that allocates a JDK carrier
without constructing it has, for every such class, a shadow that cannot be
retired until the carrier is built properly.

### 2.2 The probe tree cannot substitute for the corpus, and it is not close

Both instruments cleared all 124 rows:

* armed on the two prefixes with `CRATONVM_ENFORCE_NATIVE_SHADOW`, **all 125
  probes got no worse** and five got dramatically better;
* **built**, and run as a two-binary A/B against a control from the same merged
  tree, the same five improved — `L6UriSweep` 80 → 0, `L6X500Sweep` 276 → 0,
  `L6HttpLogicSweep` 64 → 18, `L6InetSweep` 380 → 368, `L6SocketSweep` 42 → 38
  — and **zero probes regressed**.

The corpus then refused 109 of those rows. The probe tree is 3,183 rows of
contract edges and it could not see any of it, because the defects are not in
the families' own behaviour: they are in what the VM's OTHER natives hand to
the retired bytecode. **A retirement's blast radius is its class's users**, and
a probe tree's users are the probes.

### 2.3 The dial is a lead in both directions, not a verdict

Three separate times in this wave the enforcement dial and the built table
disagreed:

* **optimistic** — `URL`, `DatagramSocket` and the `Inet*` family scored clean
  on every probe under the dial and broke six corpus vectors when built;
* **pessimistic, wrong vector** — arming `javax/security/auth/x500/` failed
  `RJdkX509Intercept`, which passes on every binary ever built here. The dial
  named the right class for the wrong reason and would have been dismissed as
  an artefact on that basis;
* **silent** — the dial said nothing about `RSslLiveSession`, which is the
  vector that actually refuses `X500Principal`.

The dial DECLINES at dispatch and arms a PREFIX; `retired_shadow.rs` re-tags at
REGISTRATION and is per-triple. They are different mechanisms and they do not
agree often enough to substitute.

### 2.4 A vector run by hand is not the vector the suite runs

This one cost a wrong commit, so it is written down. `RJdkBridge1` and
`RJdkX509Intercept` were both dismissed as dial artefacts on the strength of

```bash
cratonvm --java-home "$JDK" --jdk-only -cp build:modules-overlay:resources RJdkBridge1
```

passing on the very binary the harness fails it on. `run.sh` builds the
classpath, sets the flags and applies the cross-VM diff; a vector can pass by
hand and fail under it. The isolation that settles a vector is the one the
suite's own `known-flaky.txt` prescribes:

```bash
ONLY=<Vector> CV=... JDK=... CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh
```

run three times on each binary. Every verdict in the ladder above is from that
form, and `RSslLiveSession` — which had looked like a load flake, passing 11 of
11 by hand — came back FAIL FAIL FAIL on trial and pass pass pass on control.

### 2.5 The four preconditions, and what each removed

Applied per triple against a dump from a run of **the very probes whose
improvement is cited** — never against a corpus census, which is a different
workload. (That is the `FileChannelImpl.open` mistake recorded on
`RETIRED_SHADOW_PHASE2_TRIPLES`.)

```text
  owns the slot + effective kind Bridge   -34 not-owner, -55 already retagged
  bucket A or B (something to yield to)   -91 C/D/F
  dispatched by the instrument (inv > 0) -127 never reached
  registrar not held by lane T             -4 the throwable ctor table
  the corpus tolerates it                -109 the ladder above
```

### 2.6 The refusals are not inert

A refusal is a retirement **only when nothing already owns the triple**.
`JdkOnlyViolation::SyntheticNativeRegistered` carries a `survivor`, and a
non-null one means an earlier registration is still serving — strict mode runs
that older native, every probe reads exactly as before, and the wave is a
no-op that looks like a clean result.

Measured on the final trial binary, over a `--jdk-only-report` of three probes
that dispatch these classes:

```text
  15 distinct triples refused, 0 with a survivor
```

15 of 15. Every row of the table appears in the refusal set, and none of them
left an older registration serving. A row present in the table and absent from
the refusals would be a row the retag never reached — the silent half of this
mechanism, and the thing a green probe run cannot distinguish from success.

## 3. The prefixes that never got as far as a build

§2 is about the candidates the probe tree cleared and the corpus refused. This
section is about the ones that never became candidates: each was armed **alone**
on the same 125-probe tree, and the dial arm was enough to stop it.

| arm | probes worse | verdict |
|---|---:|---|
| `java/net/` | 0 | went to the ladder in §2 |
| `javax/security/auth/x500/` | 0 | went to the ladder in §2, and was refused at build 5 |
| `javax/net/` | 2 | refused here — `L6TlsParamSweep` 66 → 94 diffs |
| `java/security/`, `sun/security/`, `javax/crypto/`, `javax/security/` | 8 | refused here — `SecuritySurfaceSweep` 0 → 2,594 |
| `sun/net/` | 0 | refused here — **123 of 125 probes VACUOUS** |
| `jdk/net/`, `jdk/internal/net/` | 0 | refused here — **123 of 125 probes VACUOUS** |

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

Note what this section could NOT do, which §2 is the answer to: a clean dial arm
is not a candidate cleared. `java/net/` and `javax/security/auth/x500/` both
scored 0 probes worse here, and between them they lost 109 rows to the corpus
over the next five builds.

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

15 rows, all under `java/net/`:

```text
  14  java/net/HttpURLConnection    <init>(URL), addRequestProperty,
                                    getHeaderFieldDate, getInstanceFollowRedirects,
                                    getRequestMethod, getRequestProperties,
                                    getRequestProperty, setChunkedStreamingMode,
                                    setConnectTimeout, setDoOutput,
                                    setFixedLengthStreamingMode, setReadTimeout,
                                    setRequestMethod, setRequestProperty
   1  java/net/ProxySelector        getDefault
```

`HttpURLConnection` is 14 of its 30 rows. The other 16 are the
connection-state family — `getInputStream`, `getResponseCode`,
`getHeaderField(s)`, `getContentLength` — which no probe in this tree
dispatched; §5.1.

What it buys, on the final pair of binaries — the whole 125-probe tree, plain
`--jdk-only`, no dial:

```text
  L6HttpLogicSweep   64 diff lines -> 20     (164 rows, 2,913 yields)
  every other probe in the tree             delta exactly 0, except the
                                            VtHandoffProbe noise floor (§7)
```

22 differing rows closed. The rows are the perimeter this campaign predicts and
the happy path never reaches: `setRequestMethod` accepted `CONNECT` and
silently upper-cased `get`; `setRequestProperty` did not REPLACE what
`addRequestProperty` had appended, leaving `[3, 4]` where the JDK leaves `[4]`;
`getRequestProperties` handed back a MODIFIABLE map, and so did its value
lists; `setFixedLengthStreamingMode` and `setRequestProperty` were accepted
after `connect()` where the JDK throws `IllegalStateException`;
`setConnectTimeout(-1)` and `setReadTimeout(-1)` threw the right type with the
wrong message; and the RFC-850 and asctime `Date` header formats parsed as
`-1`.

One other probe moved, `HibfixVarHandleProbe` at −2 of 16, on a
`VarHandle` surface nothing in this wave touches. It is reported and not
claimed.

## 5. The 951 that stay, every one with a verdict

Adjudicated by `scripts`-free analysis of one
`--dump-native-registry --explain-jdk-only` dump plus the eight per-probe dumps,
over all **1,414** registrations under the lane's nine prefixes. Every row gets
exactly one verdict; "the rest" is not a classification.

| rows | verdict |
|---:|---|
| 557 | **BLOCKED** — measured to break the rustls TLS / JCA stack (§3.1) |
| 176 | **C** — declared abstract or on an interface; no door dispatches it |
| **15** | **RETIRED** |
| 104 | **UNOBSERVED** — no probe in this tree dispatched it (§5.1) |
| 109 | **KEEP** — the corpus refused the retirement (§2, the ladder) |
| 86 | **BLOCKED** — the arm was VACUOUS; the dial was never asked (§3) |
| 84 | **LOSER** — another registration owns the slot; the edit would be inert |
| 73 | **KIND** — already `SyntheticStub`, outside the mechanism |
| 67 | **KEEP** — super-forwarder for a null `delegate` (§3.2) |
| 40 | **D** — the image method is `ACC_NATIVE`; `Bridge` is CORRECT per §1.5 |
| 33 | **F** — class present, method absent, matches nothing |
| 33 | **KIND** — already `Intrinsic`, exempt at every door |
| 28 | **LANE-T** — the cross-lane throwable registrar and its siblings |
| 9 | **E** — no such class in the JDK image |

The goal population reconciles exactly: `15 + 109 + 557 + 86 + 67 + 104 + 28 = 966`.

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
  `dev`; the `ServiceLoader.checkCaller` failure the page describes is fixed,
  and `BuiltinClassLoader` no longer blocks ten vectors. What blocks the
  provider/algorithm rows is not L7 — it is §3.1.
* *"`URLStreamHandler` NPE, 2 vectors, yours right now."* — **Half right, and
  the half it got wrong is the interesting one.** No corpus vector fails today,
  so there was nothing to fix. But the NPE is real and **latent**: retiring
  `java/net/URL` wakes it in three vectors, not two. §2.1.
* *"`URI` (30 rows) — pure parsing, and the best mechanical wave here."* —
  **Right about the parsing and wrong about the wave.** `URI` IS pure parsing:
  yielding takes `L6UriSweep` from 80 differing lines to zero, the cleanest
  result in the lane. It is still not retirable, because `URL.toURI()` hands
  the JDK's `URI` a carrier it allocated and published fields into rather than
  constructed, and `RJdkBridge1` measures the difference on an unpaired
  surrogate. The blocker is not in `URI`.
* *"`InetAddress` deserves care; ask only structural questions."* — **Correct,
  and it was load-bearing.** The probe asks no DNS question, and the finding is
  precisely that *CratonVM* does: `getByName("256.1.1.1")` goes to the
  resolver, where HotSpot rejects it as a malformed literal without a lookup.
* *"Retire the TLS stack base-class-first."* — **Overtaken.** The ordering
  advice presumes the stack is retirable at all; §3.1 and §3.2 say it is not,
  for two independent reasons.
* *"`javax/net/`, `javax/crypto/`, `javax/security/` may not be in
  `RETIRED_SHADOW_PREFIXES` yet."* — **True, and they still are not**, now
  deliberately. `the_lane_l6_security_and_tls_prefixes_are_not_admitted` pins
  that as a measured verdict rather than an omission.
* *"Check `rc` before believing a harness label."* — **Earned its place twice.**
  `rc` is 0 when a corpus vector fails: the tell is the VM's own
  `main-vm run() returned Err` line, and the first bisect keyed on the exit
  code and called all six failures green.
* *"819 shadows over 90 classes, from 663 registration sites."* — the lane is
  **966 over 99 classes from 693 sites**. The page's numbers were a day old.

## 7. The gates

All of it on the MERGED tree, with a control binary built from the same tree
and differing only in `native-api/src/retired_shadow.rs`.

### The three arms

```text
  --jdk-only corpus    trial 133/133     ctrl 133/133    (paired, each arm ALONE)
  SUITE=all            133/133
  SUITE=core            93/93
```

(133, not the 132 quoted elsewhere on this page: the corpus gained a vector
between the wave's first acceptance and its last. Both arms ran the same list.)

### The probe tree, two binaries

```text
  measured 125 probes
  moved AWAY from HotSpot:   0
  moved TOWARD HotSpot:      1      L6HttpLogicSweep, 64 diff lines -> 20
  one side did not finish:   0
```

An earlier pass of the same A/B, on the previous merge base, scored
`VtHandoffProbe` at **+4** — and the pass before that scored it at **−14**, on
tables that share not one row. It is the campaign's named noise floor: it
counts virtual-thread handoffs and `allJoined`, both nondeterministic on this
VM, and the operations page records six successive A/Bs scoring its sibling at
`0, -2, 0, 0, +2, +2`. On the final base it did not move at all. Nothing in
this wave touches virtual threads, and no version of this wave ever claimed
that row either way.

### The gate set of `jdk-only-lane-operations.md` §5

```text
  cargo test -p cratonvm-types                                    rc=0
  cargo test -p cratonvm-native-api --tests                       rc=0
  cargo test -p cratonvm-native-builtins --tests                  rc=0
  cargo test -p cratonvm-native-builtins --features management --tests      rc=0
  cargo test -p cratonvm-native-builtins --features synthetic-jdk --tests   rc=0
```

Two gates moved and both were run to ground rather than re-frozen on sight.

**`the_tables_const_lists_every_table_the_predicate_consults`** failed: the
predicate consults seven tables and `RETIRED_SHADOW_TABLES` listed six. Lane 1
hit exactly this one merge earlier and left the note in the const. Two waves in
a row is not a one-off: a lane adding a table writes the predicate arm in one
hunk and the const row in another, `git merge` cannot know they belong
together, and the guard is the only thing that does.

**`synthetic_stub_count_does_not_regress`** moved by **+17**, and the account
is three numbers, taken by substitution on the merged tree with the control
pinned to the merge's own **second parent** rather than to `origin/dev`:

```text
  CTRL  (retired_shadow.rs from HEAD^2)   all three arms GREEN
  TRIAL (this branch)                     2345 / 2356 / 2345
  baseline before                         2328 / 2339 / 2328
```

`CTRL` green is the load-bearing half: the whole +17 is this lane's, and none
of it is drift the lane would otherwise be absorbing — the mistake this file's
own header records three of, at 3/3/12. Re-frozen to 2345 / 2356 / 2345 with
that account written into `stub_ratchet.rs`.

**+17 for 15 triples is not an error.** The unit of the stub count is a
REGISTRATION and two of the fifteen are registered twice, each re-tagged
separately. The `--jdk-only-report` census for the same prefixes reports **15
distinct triples refused, 0 with a survivor** — the same population counted the
other way, and the check that says no refusal left an older native serving.

`registrar_drift` is GREEN on both arms: this wave moves no registration site.

### One earlier red that was not a red, and why it looked like one

An earlier pass reported `NullArgMsgProbe` at +2 on a
`CopyOnWriteArrayList.addAll(null)` row — a `java/util/concurrent/` class this
lane does not touch. It was an artefact of the CONTROL, not of the trial: the
control's `retired_shadow.rs` came from `git show origin/dev:`, and
`origin/dev` is a **moving ref** that had picked up lane 5's 98-row table
between the merge and the build. The control was retiring `addAll`; the trial,
built from an older merge, was not. **A paired control has to be the merge's
own other side**, and `git rev-parse HEAD^2` is what makes it one. With the
control pinned, the row is gone.

## 8. What this leaves for the next lane

Every KEEP in this lane except the TLS/JCA question is **one nameable change
away from being a candidate again**, and this wave found the change and the
vector that tests it. Ordered by rows unblocked per unit of work.

1. **`java/net/URL` (16 rows) — give the carrier a `handler`, or stop minting
   it.** Every JDK method on `URL` is one line through `this.handler`, and this
   VM mints `URL` objects in `native-builtins/src/classloader.rs` without
   running the constructor that sets it. Tests: `RJdkServices`,
   `RServiceLoaderDoubleSource`, `RJdkDefineClass`.
2. **`javax/security/auth/x500/X500Principal` (10 rows) — put a real
   `X500Name` in `thisX500Name`.** `native-builtins/src/jca/x500.rs` documents
   the repurposing of that slot to a String, and every JDK method on the class
   dereferences it. The sites that mint principals without the real constructor
   are in `phases_late/ssl_security.rs` and `http_url_connection.rs`. Retiring
   this is worth **138 differing probe rows**, the single largest correctness
   win the lane measured. Test: `RSslLiveSession`.
3. **`java/net/URI` (29 rows) — make `URL.toURI()` construct rather than
   publish.** `net_phase_e.rs`'s `toURI` allocates a `java.net.URI` and writes
   its fields, because a string that has been through `read_string` cannot hold
   an unpaired surrogate (its own `G75-1 N1` comment). Invoking
   `URI.<init>(String)` with the original String OBJECT preserves the unit and
   populates the carrier properly — but the fallback for `jar:`/`nested:` URLs
   has to survive, and that path is load-bearing for Spring and Tomcat. Worth
   **40 differing probe rows**. Test: `RJdkBridge1`.
4. **`java/net/InetAddress` + `Inet4Address` + `Inet6Address` (18 rows) —
   implement `java/net/Inet6AddressImpl.lookupAllHostAddr`.** That one
   `ACC_NATIVE` method is what the JDK's `getByName` bytecode calls. Tests:
   `RJdkNet`, `RNetIfaceScope`.
5. **`java/net/DatagramSocket` + `MulticastSocket` (36 rows) — implement
   `sun/nio/ch/DatagramChannelImpl.receive0`.** Same shape one level further
   down the NIO stack, and it is **L4's** prefix rather than this lane's, so
   the two lanes have to agree before either moves. Test: `RJdkNet`.
6. **The TLS/JCA contract question (557 rows).** Is a first-class rustls
   implementation a §1.4 shadow at all? If it is not, those rows leave the goal
   population and the campaign denominator changes. That is L0's call, and
   nothing there should be retired until it is answered.
7. **The defects §1 lists inside that blocked set are fixable in the natives
   today**, without waiting for (6). In rough order of severity: `Cipher`
   WRAP/UNWRAP does not round-trip (it produces neither the JDK's ciphertext
   nor the original key); `GCMParameterSpec` tag lengths are ignored and not
   validated; `AES/CTR/NoPadding` resolves to no provider;
   `SSLSocket.getEnableSessionCreation` raises `AbstractMethodError`;
   `SSLParameters.setApplicationProtocols` accepts null and empty elements.
   Each has a row in `L6JcaSweep` or `L6TlsParamSweep` that goes green when it
   is fixed.
8. **The 30 connection-state rows** (`HttpURLConnection`/`URLConnection`
   `getInputStream`, `getResponseCode`, `getHeaderField(s)`,
   `getContentLength`) need either a loopback HTTP fixture in the probe tree or
   a corpus vector that asserts them.
9. **`java/net/URLClassLoader` (12 rows) should move to L7** by amending
   lane-0 §2's ownership table: it is the loader story wearing a `java/net/`
   prefix.
10. **The four `MalformedURLException`/`UnknownHostException` rows** unblock
    the moment lane T releases the throwable registrar.

### Three things to carry into every lane, not just this one

**Precondition 3 is checked one frame too high.** "The image method carries
`Code`" was true for all 124 rows and false about 109 of them, because what
that code CALLS was not satisfiable here: a field only a real constructor
writes, or an `ACC_NATIVE` method nobody implemented. Until the funnel can ask
the deeper question, the corpus is the check that catches it.

**The probe tree cannot substitute for the corpus.** 3,183 rows of contract
edges, 125 probes, and a two-binary A/B all scored 124 rows clean. The corpus
refused 109. The defects were never in the retired families' own behaviour;
they were in what the VM's other natives hand to the retired bytecode.

**Narrow by measurement, not by suspicion.** Two of the six builds here were
spent dropping `java/net/HttpURLConnection` because it was the plausible
culprit for an HTTPS vector. It was innocent, it went back in at the last
build, and it is one of the two classes this lane retires. When a build costs
twenty-five minutes, the cheapest next experiment is the one that ISOLATES a
suspect, not the one that removes it.
