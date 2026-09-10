# Lane 6 — networking, TLS, and `java.security`

**Scope: 819 §1.4 shadows over 90 classes, from 663 registration sites.**
Prefixes: `java/net/`, `sun/net/`, `javax/net/`, `jdk/internal/net/`,
`java/security/`, `sun/security/`, `javax/crypto/`, `javax/security/`,
`jdk/net/`.

Read [`lane-0-integration-and-gates.md`](lane-0-integration-and-gates.md) §2-§6 first. Method, preconditions and
landing protocol: [`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md).

---

> **Lane T closed 2026-09-10.** Its throwable-family rows are RETIRED
> (`RETIRED_SHADOW_LT_TRIPLES`, 906 triples over 62 classes), so a triple this
> page defers to lane T is either already retired or classified as blocked —
> check the table before treating it as unowned. Record: [the lane T record](../../internal/jdk-only/lane-t-the-throwable-family-retired-and-the-two-defects-the-arm-had-to-find-first-20260910.md).

## 1. Shape of the lane

```text
  67  sun/net/www/protocol/https/HttpsURLConnectionImpl   30  java/net/URI
  39  javax/net/ssl/HttpsURLConnection                    23  javax/net/ssl/SSLSocket
  35  java/net/DatagramSocket                             22  java/security/Signature
  35  javax/crypto/Cipher                                 21  java/security/Provider
  35  sun/security/ssl/SSLEngineImpl                      18  java/net/InetAddress
  31  sun/net/www/protocol/http/HttpURLConnection
  30  java/net/HttpURLConnection
```

Two large single-lane registrars concentrate the work:
`native-builtins/src/http_url_connection.rs:5390` (26 rows, one class) and the
`keystore.rs:2520-2612` family (about a dozen call sites, 8 rows each) — the
latter is **cross-lane with the frozen/unowned set**, so check lane T and L0 §2
before touching it.

## 2. Read this first: much of this lane is blocked behind L7

Security and networking reach the JDK through **service loading**, and service
loading is currently broken in a way no retirement in this lane can fix:

- `java/security/Provider` (21) and the `Signature`/`Cipher` lookup path resolve
  algorithms through `ServiceLoader`.
- `ServiceLoader.checkCaller` was failing with *"module java.base does not
  declare `uses`"* because `Class.getName()` answered the internal slash form
  when the native yielded. **That is fixed** — L0 tagged `getName` a reviewed
  `Intrinsic`, taking `ClassNameSweep` from 24 diffs of 24 to 2. Re-confirm on
  your tree.
- What remains is L7's: `jdk/internal/loader/BuiltinClassLoader` **fails to
  link**, which blocks ten corpus vectors and every path that needs the builtin
  loader hierarchy. No field publish reaches it.
- A separate recorded finding: **`loadInstalled()` answered 0 for every
  service** and was bypassed *without throwing*, because every module was in the
  app loader's catalog and the probe asked a different lookup than the code
  used.

**So price your provider/algorithm rows against L7's progress, not against
today's failures.** Start with the rows that do not route through service
loading — `URI`, `InetAddress`, `DatagramSocket` — and re-price the rest after
each L7 landing.

## 3. Two vectors in the corpus are yours right now

- **`URLStreamHandler` NPE, 2 vectors.** In your prefix, small, and independent
  of the loader work. A good first target because it is a real corpus movement
  rather than a census movement.
- The CLDR locale-provider failures are **L1's** territory, not yours, even
  though they surface through a provider lookup. Hand them over rather than
  duplicating the diagnosis.

## 4. `URI` (30 rows) — pure parsing, and the best mechanical wave here

No VM-filled state, no service loading, no I/O. Retiring it exercises the JDK's
own RFC-3986 parser, which is far more exact than any hand-written native. Probe
the shapes where implementations diverge, and print exception **messages**:

- opaque vs hierarchical, empty authority, `file:///`, IPv6 literals in
  brackets, percent-encoding in each component, `resolve`/`relativize`
  round-trips, `normalize` on `..` above root, and the
  `equals`/`hashCode`/`compareTo` case rules (scheme and host are
  case-insensitive; path is not).

`java/net/InetAddress` (18) is the opposite and deserves care: its answers
depend on the **host's** resolver, so a row that prints a resolved address is
not reproducible. Ask only structural questions — `isLoopbackAddress`,
`getByAddress` round-trips, textual-form parsing — and never a DNS lookup.

## 5. TLS and HTTPS: 200+ rows, and the ordering that makes them tractable

`HttpsURLConnectionImpl` (67), `javax/net/ssl/HttpsURLConnection` (39),
`SSLEngineImpl` (35) and `SSLSocket` (23) are a stack, not four classes. Bucket
B dominates: `HttpsURLConnectionImpl` inherits most of its surface from
`HttpURLConnection`, which inherits from `URLConnection`.

Consequence: **retire from the base class upward.** Retiring
`HttpsURLConnectionImpl.getInputStream` while `HttpURLConnection.connect` is
still a native leaves the JDK's bytecode calling into a native that has a
different idea of the connection state.

Do not attempt a live TLS handshake as your instrument. Probe the parts that are
pure logic — header parsing and case-insensitive header maps, redirect-limit
counting, `getHeaderFieldKey` ordering, `setRequestProperty` validation,
`SSLParameters` get/set round-trips, cipher-suite list filtering — and leave the
handshake to the regression corpus, which already covers it.

## 6. Traps

- **A blanket null from a shadowing native picks the fallback path**, and
  fallbacks differ between JDK 21 and 25. Split a composite call into its
  sub-questions before concluding anything.
- **Check `rc` before believing a harness label.** In this area a classifier
  read 976 SIGSEGVs as OOM-kills, because it could not see its own VM's crash
  report.
- **`javax/net/`, `javax/crypto/`, `javax/security/` may not be in
  `RETIRED_SHADOW_PREFIXES` yet.** L0's skeleton commit adds every lane's
  prefixes; verify yours is present, because an entry outside every prefix
  silently answers "not retired" and is invisible in a workload.
- Several of your classes are also reached by **L4's** channel work
  (`SocketChannelImpl`, `DatagramChannelImpl`) and by **lane T's**
  `concrete_receiver.rs:185`. Confirm holds before starting.

## 7. The increment loop

1. Funnel from a dump: owns slot, kind `Bridge`, image `Code`, `invocations > 0`
   in **your** instrument's run.
2. Probe + HotSpot oracle, on this host, with the oracle configured like the VM
   under test. No build needed.
3. Fill `RETIRED_SHADOW_L6_TRIPLES`, sorted and unique.
4. Build token (L0 §5); one build per wave.
5. `N refusals, 0 survivors`.
6. Probe-tree A/B, `--jdk-only` corpus, `SUITE=all` at `TIMEOUT=600`, `all`-arm
   count.
7. Full gate set. Kind-map rows. Commit. Do not push.

## 8. Done

Every bucket-A/B row in the prefix set is retired, classified as C/D/E/F, a
reviewed `Intrinsic` with its probe, or blocked with the blocker named — with
the service-loading-dependent rows explicitly separated from the rest and
re-priced after L7's last landing, and the TLS stack retired base-class-first.
