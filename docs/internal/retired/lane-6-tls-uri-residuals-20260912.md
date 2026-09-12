# Lane 6 — the §6 residuals: `URI`, `URL`, and the TLS bookkeeping

**Wave record, 2026-09-12.** Closes items 2, 3, 4 and 5 of
[`lane-6-net-residuals-20260911.md`](lane-6-net-residuals-20260911.md) §6, and
answers items 1 and 6 with measurement rather than leaving them as assertions.

**This wave retires nothing.** No triple leaves any keep list, no kind moves,
and the census denominator does not change. Every number below is a
correctness measurement against HotSpot 25.0.4+7.

---

## 1. What §6 listed, and what happened to each

| §6 item | verdict |
|---|---|
| 1. the cipher-suite and protocol lists | **measured; one real defect inside it, fixed** — see §7 |
| 2. `SSLContext` / `KeyManagerFactory` initialisation state | **fixed** — rows 64, 65, 103, 106 |
| 3. `SSLServerSocketFactory.createServerSocket` | **fixed** — rows 87, 88 |
| 4. `getURL()` after a followed redirect | **fixed** — `L6HttpLoopbackSweep` row 57 |
| 5. the `java.net.URI` and `java.net.URL` families | **fixed** — 40 and 7 rows, **both to zero** |
| 6. the literal screen's placement | **measured; nothing to align to** — see §8 |

Numbers below are `diff` lines, two per differing row, base against trial.
**Nothing got worse.**

---

## 2. Item 5a — `URI.hashCode` was 31 of `L6UriSweep`'s 40 rows

The shape was already the JDK's: `hashIgnoringCase(scheme)`, then fragment,
then the opaque/hierarchical split, then `1949 * port`. The two helpers it was
built out of were not.

```rust
// before — one 31-fold continued across the whole URI, over BYTES
fn hash_str(h: i32, s: Option<&str>) -> i32 {
    s.bytes().fold(h, |acc, b| acc.wrapping_mul(31).wrapping_add(b as i32))
}
```

`java.net.URI.hash(int, String)` is **`hash * 127 + s.hashCode()`** — a fresh
string hash MIXED INTO the accumulator, not a continuation of it — and it
switches to `normalizedHash` for any component containing a `%`, so two URIs
differing only in the case of an escape triplet hash alike. `hashIgnoringCase`
really is a continued 31-fold, which is why one of the two helpers looked
right: they are genuinely different functions and the file had one shape for
both.

Both also folded `str::bytes()`. `java.lang.String.hashCode` folds UTF-16 code
units, so a component carrying a non-ASCII character hashed differently for
that reason as well.

**The transcription was checked before it was written**, by running the
candidate against `new URI(spec).hashCode()` for all 37 specs the probe
constructs: `37 specs, 0 mismatches`. `a:b/c` was hand-verified to the
arithmetic — `hashIgnoringCase(0,"a") = 97`, then
`97*127 + "b/c".hashCode() = 12319 + 95734 = 108053`, which is what HotSpot
prints.

## 3. Item 5b — the other nine `URI` rows

**`URI.create(String)` had its own transcription of the constructor's
refusals.** Four of the constructor's seven checks, in a different order, with
one catch-all message (`Illegal character in URI at index 9`) where the
constructor names the component — and no `cause`. The JDK's `create` is four
lines:

```java
try { return new URI(str); }
catch (URISyntaxException x) { throw new IllegalArgumentException(x.getMessage(), x); }
```

Both halves of the word TRANSLATED were missing. The ladder is now one
function, `net_uri_inet::uri_parse_fail`, called by the constructor and by
`create`; `create` builds the real `URISyntaxException` and wraps it as the
JDK does, so the reason and the index survive for a caller that catches the
unchecked wrapper. **4 rows.**

**A second `#` is illegal.** `uri_first_char_fault` had `'#'` in its
unconditional legal set as "fragment delimiter". The JDK takes everything
after the FIRST one as the fragment and scans it against a set that does not
contain `#`, so `new URI("http://h/p#a#b")` is `Illegal character in fragment
at index 12`. **2 rows.**

**`compareTo(null)` is an NPE, not an `IllegalArgumentException`.** The JDK has
no null check: `compareTo`'s first statement dereferences the argument, and the
helpful NPE names the field — `Cannot read field "scheme" because "that" is
null`. An `IllegalArgumentException` is a different type on the wire and a
`catch (NullPointerException)` walks past it. **1 row.**

**The exception's `input` is not the argument.** `new URI("  http://h/p  ")`
reports `http://h/p`. This is not trimming in the parser — the INDEX still
refers to the untrimmed string — it is `jdk.internal.util.Exceptions.trim`,
which every `URISyntaxException` the parser raises passes through:

```java
throw new URISyntaxException(formatMsg("%s", filterNonSocketInfo(input)), reason, p);
// formatMsg ends in trim(): "remove leading, trailing and duplicated space characters"
```

Only U+0020. A tab survives, which is how it was told apart from
`String.strip()` before the source was read; this host's JDK 25 `src.zip`
confirms both. **1 row.**

## 4. Item 5c — the seven `URL` rows

**`URL.toURI()` published without parsing.** It built a `java.net.URI` carrier
around `toString()`'s text and wrote its components — so
`new URL("http://h/a b").toURI()` handed back a URI whose own constructor
rejects its text. `toURI()` is `new URI(toString())`, so it owes that
constructor's refusals; it now runs `uri_parse_fail` first. Only the CHECK is
shared — the publish stays, because the `jar:`/`nested:` fallback it exists for
is load-bearing for Spring and Tomcat (the retirement record's item 3).
**3 rows.**

**`http://h:80/p` equals `http://h/p`.** `URLStreamHandler.sameFile` and
`hashCode` both substitute the protocol's default port for an absent one. The
canonical key `URL.equals`/`sameFile`/`hashCode` share wrote the literal port
text, so the two were different keys. One table, `url_default_port`, now shared
with `URL.getDefaultPort()` — which was the only copy of it. **3 rows.**

**`new URL("http://h:-1/p").toExternalForm()` answered `h:-1`.**
`field5_is_full_url` decides whether slot 5 holds a cached full URL or a bare
authority, and its authority test was "everything after the colon is a digit".
`-1` is not, so `h:-1` was read as scheme `h` and the bare authority was handed
back as the whole URL. `rest.parse::<i64>()` is the test that was meant.
**1 row.**

---

## 5. Item 2 — an initialised flag, and the slot that could not hold it

`SSLContext.getInstance("TLS").getSocketFactory()` is
`IllegalStateException: SSLContext is not initialized` on HotSpot
(`SSLContextImpl.checkInitialized`). This VM handed back a factory whose key
and trust managers were never installed — so a caller whose `init()` threw and
was swallowed got a working object and no signal.

**The first attempt did not fire, and the trial binary is what said so.** Both
live `getInstance` registrations already write `Int(0)` into slot 1 and `init`
writes `Int(1)`, so the gate looked one `matches!` away. Rows 64 and 65 were
unchanged. Slot 1 of a REAL `javax.net.ssl.SSLContext` is the `contextSpi`
REFERENCE, and `try_alloc_concurrent_synthetic` upsizes to the real layout in
real-JDK mode: an `Int` written there is not an `Int` when it is read back. The
two existing writes are equally inert. The state moved to a side table of
contexts minted-but-not-initialised — default ALLOW, so a context this crate
did not mint is never refused.

**And the second attempt fixed only row 64.** `createSSLEngine` is registered
in `net_phase_e` inside a `for desc in [..]` loop, which hides it from the grep
that finds every other `"createSSLEngine"` in the tree — so the gate went onto
the two registrations in `phases_late::ssl_security` that do not win. Three
builds for one guard, and each time the tell was the same: base and trial
printed the same words.

`KeyManagerFactory`/`TrustManagerFactory` got the same treatment with a
different mechanism — neither class has a spare slot (slot 1 of both is the
real `factorySpi`, which `jsse_factory_is_ours` reads), so the flag is a set
keyed the way this module already keys its two id tables, marked at each
`init`'s success return and consulted by `getKeyManagers`/`getTrustManagers`.
The messages are the JDK's own: `KeyManagerFactoryImpl is not initialized`.

## 6. Item 3 — a TLS factory handing out a plaintext `ServerSocket`

`javax.net.ssl.SSLServerSocketFactory` inherits the no-arg
`createServerSocket()` from `javax.net.ServerSocketFactory`, and `phases_early`
registers a native THERE that answers `new java.net.ServerSocket()`. Dispatch
asks the registry about the RECEIVER's class chain, so an
`SSLServerSocketFactory` receiver reached it.

The visible half is a `ClassCastException` on a cast HotSpot does not have to
make. The invisible half is worse: a caller who did not cast got a **plaintext
listener from a factory whose name says TLS**.

`SSLServerSocketFactoryImpl.createServerSocket()` is
`new SSLServerSocketImpl(context)` — an SSLServerSocket with no listener behind
it, whose parameters can be set and read before anything binds. That is what
this now records, and it is the first UNBOUND socket this file has ever had, so
`SslServerSocketState` grew a `bound` field: `isBound()` could no longer be
"we have a record of it".

`bind()` on such a socket is refused with a named `SocketException` rather than
brought up. `create_ssl_server_socket` resolves its TLS identity from the
FACTORY it was called on, and a socket keeps no rooted reference to one, so a
listener could only be built on the process-wide identity — and the one thing
this file must never do is stand up a listener without the client verifier a
caller asked for. A loud refusal replaces a silent plaintext listener.

### Why there was anything to find

`native-builtins/src/tls_deny.rs` exists for exactly this defect. Its module
doc opens with the mechanism — the plaintext base registrations call
`deny_plaintext_fallback` first, and a TLS-factory receiver reaching them is
refused — and it carries two allowlists, a test that checks them against the
live registry, and eleven unit tests of the refusal itself. The no-arg
overload was even IN the unbridged list, with a paragraph explaining that
`create_ssl_server_socket` binds and builds in one step so there is no unbound
socket to hand back.

**`deny_plaintext_fallback` had no caller outside its own test module.** All
nine plaintext base registrations in `phases_early` went straight to their
bodies. The net was never hung, so the one overload it was written to catch
went on returning a plaintext `java.net.ServerSocket` for as long as the
module existed — and the allowlist that said so read as a decision rather than
as the symptom it was.

The nine call sites are now real. With both unbridged sets empty this is
behaviourally inert today, which is the point: every descriptor the plaintext
base registers is bridged on the TLS class, so dispatch never reaches the base
for one, and the guard is there for the NEXT overload rather than this one.
A premise in a comment is not a compile-time link.

## 7. Item 1 — the lists, asked of the VM about ITSELF

§6 is right that the suite and protocol lists cannot match HotSpot's and that
faking them would be a lie. But *"the list is different"* and *"the VM gives
two different answers to the same question"* are different claims, and only the
second is measurable without settling the first. Four rows were appended to
`L6TlsParamSweep` — at the END, so every existing row keeps its number — each
asking one JSSE whether it agrees with itself. **All four are `true` on HotSpot
by construction.**

Three were `false` here, and two of those are now fixed:

* `SSLServerSocketFactory.getDefaultCipherSuites()` answered **three**
  hard-coded TLS 1.3 names where `SSLSocketFactory` and `SSLContext` answer all
  fifteen of `SUPPORTED_CIPHER_SUITE_NAMES` — the file's own declared single
  source of truth.
* `SSLEngineImpl.getEnabledCipherSuites()` defaulted to the same three, twenty
  lines below its own `getSupportedCipherSuites()` answering fifteen. This is
  E42 exactly, one class over: HotSpot has no enabled/supported distinction on
  a fresh engine (measured, 31 == 31), and netty's `JdkSslContext` intersects
  its configured list with an engine's — so every TLS 1.2 suite this VM can
  actually negotiate was silently dropped from that intersection.

The third is left open, deliberately: `SSLSocket.getSupportedProtocols()`
answers `[TLSv1.2, TLSv1.3]` and `SSLEngine`/`SSLContext` answer
`[TLSv1.1, TLSv1.2, TLSv1.3]`. Both sides carry a written rationale — the
engine's says TLSv1.1 must be advertised so Tomcat's configuration
intersection does not destroy an explicit `TLSv1.1+TLSv1.2` policy; the
socket's says this VM offers only the two. Row 118 now measures the
disagreement instead of leaving it to the next reader to notice.

**The fifteen-name list is not fiction.** The CBC suites are implemented
(`t27_tls_cbc`) and the two DHE names are negotiated as their ECDHE analogue;
that was checked before considering whether to narrow it.

## 8. Item 6 — the literal screen, and what is actually behind that door

§6 records that `Inet*AddressImpl.lookupAllHostAddr` is served by
`inet_address.rs::resolve_addrs`, which has no literal screen, and that no
probe row reaches it. Both halves are true. The conclusion drawn from them —
"a caller reaching that native directly still gets `getaddrinfo`'s more
permissive answer" — understates what is there. Measured on both VMs with
`--add-opens java.base/java.net=ALL-UNNAMED`:

```text
                                  HotSpot                CratonVM
  Inet6AddressImpl("0x7f.0.0.1")  UnsatisfiedLinkError   1 [127.0.0.1]
  Inet4AddressImpl("localhost")   UnsatisfiedLinkError   1 [127.0.0.1]
```

Ten rows, all ten `UnsatisfiedLinkError` on HotSpot: the JNI method is not
bound for an impl a caller constructed reflectively. **There is nothing to
align to.** The class is package-private and needs `--add-opens` to reach at
all, and what HotSpot does when you do reach it is refuse. §6's decision
stands, now on a measurement rather than on an assumption.

---

## 9. What this wave did not do

The `--jdk-only` residual rows that are NOT §6 items, with what is known about
each. Three of them are one family.

**`java.net.HttpURLConnection`'s natives hijack a user SUBCLASS.**
`register_one(r, "java/net/HttpURLConnection")` exists so apps that use the
abstract base class directly via reflection work, and dispatch probes the
RECEIVER's class chain — so a test double that extends `HttpURLConnection` and
overrides `getHeaderField(String)` has its `getContentLength()`,
`getLastModified()`, `setDoInput()` and `setUseCaches()` answered by natives
that never consult its overrides. `L6HttpLogicSweep`'s `Fixture` is exactly
that shape: rows 87, 88, 101, 115, 117 — `getContentLength()` tried to open a
socket to `fixture.invalid`. The discriminator is cheap (this VM mints exactly
four carrier classes and a subclass is none of them), but the fallback has to
be "run the JDK's own bytecode for the method the JVM resolved", which is ~31
wrappers through a macro like `https_super_forwarders!`. Sized, not built.

**Two registrars own `setFixedLengthStreamingMode` / `setChunkedStreamingMode`,
and the second keeps its state in aliased slots.** `phases_early`'s `p54_*`
pair registers the same three triples on `java/net/HttpURLConnection` and
decides "is the other mode set?" by reading synthetic slot indices — on a real
carrier those name other fields. Both refusals fire spuriously, and
symmetrically: `setFixedLengthStreamingMode` reports `Chunked encoding
streaming mode set` and `setChunkedStreamingMode` reports `Fixed length
streaming mode set`, each on a connection where nobody set either. Rows 64, 65
and 66 of `L6HttpLoopbackSweep` and row 155 of `L6HttpLogicSweep`. That file's
own comment already says such a check "cannot be made safely from a native
registered on the shared class" — about slot 9, two functions away.
`--dump-native-registry` settles which registrar wins in one command; that is
the next wave's first move, not a guess this one should make.

**`getRequestMethod()` and the wire disagree about `doOutput`.** Row 67 sent
POST and reported GET; row 69 set `PUT` and POST went on the wire. The JDK's
rule is one line of `getOutputStream()` — `if (method.equals("GET")) method =
"POST"` — and this VM applies the promotion in one place and picks the wire
method in another.

**Rows measured and left:** `L6TlsParamSweep` 66 and 89 (a fresh `SSLEngine`'s
`getUseClientMode()` is `false` on HotSpot and `true` here), 69
(`SSLSocketFactory.getDefault().getClass()` — HotSpot's is
`SSLSocketFactoryImpl`; minting the JDK's own Impl hands every inherited call
to bytecode this VM does not implement, which is why it was not done), 74 (the
same for `DefaultServerSocketFactory`), 83, 86, 94; `L6HttpLogicSweep` 22;
`L6HttpLoopbackSweep` 37, 42, 73; and `L6SocketSweep`'s 20 rows, which no §6
item names and this wave did not open.

---

## 10. The gates

Two runs. The first found three tests this wave moved; the second is the tree
that lands.

**Run 1 — isolation, on the pre-merge tree.** Base and trial built from one
worktree at `bdb02d94e`, so they differ only by this wave:

```text
  A/B, 9 L6 probes            0 worse, 5 better, 62 rows closed
      L6UriSweep              80 diff lines -> 0
      L6UrlSweep              14            -> 0
      L6TlsParamSweep         58            -> 36
      L6HttpLogicSweep        18            -> 14
      L6HttpLoopbackSweep     20            -> 16
      L6JcaSweep / L6InetSweep / L6X500Sweep / L6SocketSweep   unmoved
```

Three trial binaries, not one, and each time the tell was the same: base and
trial printed the same words. Twice for the `SSLContext` gate (§5) and once
for `URL.toURI` — the fix that was measured, not the fix that was reasoned
about, is the one that moved a row.

**Run 2 — the merged tree**, which is what lands. `dev` moved 60+ commits
under this wave:

```text
  --jdk-only corpus          134 passed, 0 failed
  SUITE=all                  134 passed, 0 failed
  SUITE=core                  93 passed, 0 failed
  gate: types                 green
  gate: native-api            green
  gate: native-builtins       one target failed  (see below)
  gate: ... --features management     one target failed  (the same one)
  gate: ... --features synthetic-jdk  one target failed  (the same one)
  A/B re-run against the merged binary   reproduces every number above
```

### The one red, and why it is not this wave's

`raw_lock_constructions_do_not_grow` reports **429 against a baseline of 428**
in all three `native-builtins` configurations. That is the red this wave's own
BASE commit is about: `bdb02d94e`, "the second dev-owned red on this merge,
named to the line", records 429 on pristine `origin/dev`.

It was re-derived here rather than taken on trust, because this wave really
did add two locks and had to prove it had removed them. The test's census is a
source scan, so it can be run against two revisions with no build at all: the
same algorithm over `git archive bdb02d94e` and over the landing tree gives
**429 both times, with an empty per-file diff**. The two side tables this wave
adds are `OrderedPlMutex` at `LockLevel::Scratch` and are counted as ordered,
not raw.

The test caps its site list at 40 alphabetically, so it names neither the
culprit nor the innocent — which is why the per-file diff, not the site list,
is the instrument.

### One flaky row, recorded

`L6SocketSweep` scored 40 diff lines on the base binary in one run and 42 in
another, same binary, same probe. Nothing in this wave touches it. The trial's
40 in the second run is therefore not a fix and is not counted above; the row
that moves needs finding before anyone reads a delta there.
