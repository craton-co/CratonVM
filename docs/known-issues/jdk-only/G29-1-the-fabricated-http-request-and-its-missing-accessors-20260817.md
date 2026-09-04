# G29-1 — the fabricated HttpRequest and its missing accessors: Mechanism A, instances 4 and 5

**Status:** DIAGNOSED-MEASURED / FIXED-IN-SOURCE, **AFTER NOT MEASURED**.
Every "before" number below is MEASURED on a real binary against a real
oracle. The fix is written and formatted but **has not been built**, so its
"after" is PREDICTED — §8 says so in the plainest terms available, because this
directory's standing rule is that a prediction must never be dressed as a
result.

**Provenance.** Binary: `C:/craton/target-fcheck/release/cratonvm.exe`, mtime
`2026-08-17 01:41:07 -0300`, unchanged for the whole of this lane (re-checked at
02:08 and again at the end — see §8). Oracle: HotSpot 25.0.3+9-LTS at
`C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`. Vectors from
`C:/craton/cvm-mergecheck/regression-suite/build`. Probes written for this lane,
in `scratchpad/`: `HttpProbe.java` (the seven accessors × twenty builder
shapes, sixteen builder refusals, the `HttpHeaders` reader surface, the
`HttpClient` control), `HttpProbe2.java` (header ordering, header count,
`copy()`, `HEAD()`, five `BodyPublishers` content lengths, scheme validation),
`HttpProbe3.java` (the same rows on BOTH VMs — the differential this record's
tables are cut from).

Files changed: `native-builtins/src/net_phase_e.rs`,
`native-builtins/src/http_client.rs`. Nothing else.

---

> **VERIFIED AGAINST A BINARY 2026-09-04. The predicted "after" is measured, and
> it is this record's own numbers to the digit.** Status was
> *"DIAGNOSED-MEASURED / FIXED-IN-SOURCE, **AFTER NOT MEASURED**"* — the fix
> *"written and formatted but has not been built"*.
>
> §1 measured the before-state precisely: `RJdkOptionalShape` died at
> `httpmint` with `AbstractMethodError: java/net/http/HttpRequest.version()` and
> produced **16** `CK` lines against HotSpot's `checks=1418`.
>
> ```text
>                        CK lines   verdict                                differing
> HotSpot 25                 29     PASS RJdkOptionalShape (1418 checks)       —
> CratonVM --jdk-only        29     PASS RJdkOptionalShape (1418 checks)       0
> CratonVM compatible        29     PASS RJdkOptionalShape (1418 checks)       0
>
> AbstractMethodError occurrences in the --jdk-only transcript:  0
> ```
>
> **§1's two named landmarks both land exactly.** The record says the gap is the
> whole `httpmint` family and that HotSpot's last family line is
> `httpmint=268`; ours now reads `CK RJdkOptionalShape httpmint=268`, and the
> total is `checks=1418` — the two figures §1 tabulated as the target.
>
> **§5.1's slot-map change is in the tree as described**, and the hazard it was
> written against is closed by construction rather than by comment:
>
> ```text
> RE5_REQUEST_NUM_FIELDS = 8                    net_phase_e.rs:13288  (was a bare 0..5)
> RE5_REQUEST_EXPECT_CONTINUE = 5, BODY_PUBLISHER = 7   three new named slots
> for slot in 0..RE5_REQUEST_NUM_FIELDS         net_phase_e.rs:14528  copy driven by the constant
> cargo test -p cratonvm-native-builtins net_phase_e     83 passed; 0 failed
> ```
>
> **What this does NOT verify.** §2's oracle — *"the seven accessors across
> every builder shape"*, twenty builder shapes and sixteen builder refusals —
> was measured with `scratchpad/` probes (`HttpProbe.java`, `HttpProbe2.java`)
> that did not survive their session. `RJdkOptionalShape` exercises the
> accessors its `httpmint` family reaches, which is not the same population; a
> builder shape the vector never constructs is neither confirmed nor refuted
> here. The `HttpHeaders` reader surface and the `BodyPublishers`
> content-length rows are in that unmeasured remainder.

## 0. The headline

`G13-1` measured `RJdkOptionalShape`'s failure and named the mechanism:

> **Mechanism A** — a CratonVM native fabricates an instance of an abstract
> class or interface, and a method invoked on it has no native registration.

It counted `HttpRequest` at **3 of 7**. That count is right for
`java.net.http.HttpRequest` itself and it is the smaller half of the story.
Sweeping the whole surface this file mints — which is what the assignment asked
for and what a one-row fix would have missed — the census is:

Counts are ABSTRACT instance methods (`javap -p`, JDK 25.0.3+9-LTS) — the ones
for which "no native" means `AbstractMethodError` rather than a JDK body —
except the `HttpHeaders` row, which is a real concrete class and is counted by
its public readers.

| minted type | kind | abstract instance methods | registered before | after |
|---|---|---|---|---|
| `java/net/http/HttpRequest` | **abstract class** | 7 | **3** | 7 |
| `java/net/http/HttpRequest$Builder` | **interface** | 14 (+1 default, `HEAD`) | **11** | 14 (+`HEAD`) |
| `java/net/http/HttpRequest$BodyPublisher` | **interface** | 1 | **0** | 1 |
| `java/net/http/HttpHeaders` | final class, real bytecode | 4 public readers (+`toString`) | **1** | 5 |
| `java/net/http/HttpClient` | abstract class | 12 | **12** | 12 (untouched) |
| `java/net/http/HttpClient$Builder` | interface | 11 (+1 default, `localAddress`) | **11** | 11 (untouched) |

(`HttpClient` also declares six CONCRETE instance methods —
`newWebSocketBuilder`, `shutdown`, `shutdownNow`, `close`, `isTerminated`,
`awaitTermination`. Five of the six have natives here; `newWebSocketBuilder`
does not, and does not need one to avoid an `AbstractMethodError` because it has
a body. Same for `HttpRequest.equals`/`hashCode`, which are `final` and
concrete, and which work once the seven accessors they call exist.)

`BodyPublisher` is **0 of 1** — the same ratio at its limit that `PathMatcher
.matches` had, in the same file family, and it is the assertion
`RJdkOptionalShape` reaches immediately after the four this record was assigned.
So Mechanism A now has **five** measured instances, not four, and the fifth was
sitting one call downstream of the fourth.

`HttpClient` is the control the assignment named, and the control holds: **12 of
12** abstract instance methods have a row, and the seven the vector asks about
are measured correct on both VMs (§3's last row: the enum side is sound too).
The difference between the two families is not design. It is that
`HttpClient`'s registrar finished and `HttpRequest`'s stopped after three.

---

## 1. The vector, MEASURED before

```bash
export JAVA_HOME="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
CV="C:/craton/target-fcheck/release/cratonvm.exe"
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CV" --java-home "$JAVA_HOME" --jdk-only \
    -cp C:/craton/cvm-mergecheck/regression-suite/build RJdkOptionalShape
```

| | HotSpot | CratonVM `--jdk-only`, before |
|---|---|---|
| last family line | `httpmint=268` | `process=172` |
| total | `checks=1418`, `PASS` | dies, 16 `CK` lines |
| death | — | `AbstractMethodError: method java/net/http/HttpRequest.version()Ljava/util/Optional; has no Code attribute` at `RJdkOptionalShape.httpmint(RJdkOptionalShape.java:874)` |

`RJdkOptionalShape` accepts `--list` and `--only=<family>`, and both work on
CratonVM:

```text
CK RJdkOptionalShape family=core / prim / stream / version / misc / process / httpmint
```

`httpmint` is the last family and the only red one; the other six are already
green (`core=123`, `prim=100`, `stream=363`, `version=262`, `misc=130`,
`process=172` — identical on both VMs). So the whole remaining gap between
`checks=16` and `checks=1418` is this one family, and this one family is
`java.net.http`.

---

## 2. The oracle, MEASURED — the seven accessors across every builder shape

`HttpProbe.java`, HotSpot 25.0.3+9-LTS. `empty` is `Optional.empty`;
`present:x` is a present Optional holding `x`; a literal `null` would have
printed as `null` and never does. Labels are ASCII throughout.

| built by | `method` | `timeout` | `version` | `bodyPublisher` | `expectContinue` | `headers().map()` |
|---|---|---|---|---|---|---|
| `newBuilder(u)` | GET | empty | empty | empty | false | `{}` |
| `newBuilder().uri(u)` | GET | empty | empty | empty | false | `{}` |
| `.GET()` | GET | empty | empty | **empty** | false | `{}` |
| `.DELETE()` | DELETE | empty | empty | **empty** | false | `{}` |
| `.HEAD()` | HEAD | empty | empty | **empty** | false | `{}` |
| `.POST(noBody())` | POST | empty | empty | present:len=0 | false | `{}` |
| `.POST(ofString("hello"))` | POST | empty | empty | present:len=5 | false | `{}` |
| `.PUT(ofString("abc"))` | PUT | empty | empty | present:len=3 | false | `{}` |
| `.method("PATCH", ofString("pp"))` | PATCH | empty | empty | present:len=2 | false | `{}` |
| `.method("GET", noBody())` | GET | empty | empty | **present:len=0** | false | `{}` |
| `.timeout(5s)` | GET | present:PT5S | empty | empty | false | `{}` |
| `.expectContinue(true)` | GET | empty | empty | empty | **true** | `{}` |
| `.expectContinue(false)` | GET | empty | empty | empty | false | `{}` |
| `.version(HTTP_1_1)` | GET | empty | present:HTTP_1_1 | empty | false | `{}` |
| `.version(HTTP_2)` | GET | empty | present:HTTP_2 | empty | false | `{}` |
| `.header("Accept","text/plain")` | GET | empty | empty | empty | false | `{Accept=[text/plain]}` |
| `.header("Accept","a").header("Accept","b")` | GET | empty | empty | empty | false | `{Accept=[a, b]}` |
| `.header("Accept","a").header("X-Foo","b")` | GET | empty | empty | empty | false | `{Accept=[a], X-Foo=[b]}` |
| `.header("Accept","a").setHeader("Accept","z")` | GET | empty | empty | empty | false | `{Accept=[z]}` |
| `.headers("A","1","B","2")` | GET | empty | empty | empty | false | `{A=[1], B=[2]}` |
| `full` (POST+timeout+expect+version+header) | POST | present:PT3S | present:HTTP_2 | present:len=4 | true | `{Accept=[text/plain]}` |

Two rows in that table are load-bearing and easy to get backwards:

* **`.GET()` clears the publisher; `.method("GET", noBody())` does not.** They
  are not the same request. `HttpRequestBuilderImpl` overrides `GET()`/
  `DELETE()`/`HEAD()` to pass no publisher at all, which is why `HEAD()` — a
  *default* interface method whose bytecode reads `method("HEAD", noBody())` —
  still answers `bodyPublisher() == empty` on the oracle.
* `getClass()` is `jdk.internal.net.http.ImmutableHttpRequest` on HotSpot and
  `java.net.http.HttpRequest` here, in every row. That divergence is the
  mechanism itself and is **not** fixed by this record; see §7.

### 2.1 The refusals, MEASURED — value or exception class plus exact message

Transcribed from the probe's output, not derived:

| call | HotSpot |
|---|---|
| `HttpRequest.newBuilder((URI) null)` | `NullPointerException: uri must be non-null` |
| `newBuilder().uri(null)` | `NullPointerException: uri must be non-null` |
| `newBuilder(URI.create("ftp://example.com/x")).build()` | `IllegalArgumentException: invalid URI scheme ftp` |
| `newBuilder(URI.create("mailto:a@b")).build()` | `IllegalArgumentException: invalid URI scheme mailto` |
| `newBuilder(URI.create("/relative")).build()` | `IllegalArgumentException: URI with undefined scheme` |
| `newBuilder().build()` | `IllegalStateException: uri is null` |
| `newBuilder(URI.create("HTTP://h/x")).build().uri()` | `HTTP://h/x` — scheme test is case-INsensitive |
| `header(null, "v")` | `NullPointerException: name` |
| `header("k", null)` | `NullPointerException: value` |
| `header("", "v")` | `IllegalArgumentException: invalid header name: ""` |
| `timeout(Duration.ofSeconds(-1))` | `IllegalArgumentException: Invalid duration: PT-1S` |
| `timeout(Duration.ZERO)` | `IllegalArgumentException: Invalid duration: PT0S` |
| `timeout(null)` | `NullPointerException` (message **null**) |
| `method("", noBody())` | `IllegalArgumentException: illegal method <empty string>` |
| `method(null, noBody())` | `NullPointerException` (message null) |
| `method("POST", null)` | `NullPointerException` (message null) |
| `POST(null)` | `NullPointerException` (message null) |
| `version(null)` | `NullPointerException` (message null) |
| `headers("A")` | `IllegalArgumentException: wrong number, 1, of parameters` |

Note the split that a "close enough" refusal would erase: `header(null, v)`
carries the message `name`, `header(k, null)` carries `value`, and the
publisher/version/timeout/method setters carry **no message at all**
(`Objects.requireNonNull(x)` with one argument). `getMessage()` returning
`"name"` versus returning `null` is observable from Java, so the fix models the
difference rather than approximating it.

### 2.2 `HttpHeaders`, MEASURED — because `headers()` is one of the missing four

Headers `{Accept: text/plain, Accept: text/html, X-Num: 42}`:

| call | HotSpot |
|---|---|
| `map()` | `{Accept=[text/plain, text/html], X-Num=[42]}` |
| `map().getClass()` | `java.util.Collections$UnmodifiableMap` |
| `map().put(...)` | `UnsupportedOperationException` (message null) |
| `firstValue("Accept")` | `present:text/plain` |
| `firstValue("accept")` | `present:text/plain` — case-INsensitive |
| `firstValue("Nope")` | `empty` |
| `firstValue(null)` | `NullPointerException` (message null) |
| `allValues("Accept")` | `[text/plain, text/html]` |
| `allValues("Nope")` | `[]` — empty list, never null |
| `firstValueAsLong("X-Num")` | `present:42` |
| `firstValueAsLong("N")` where `N: -7` | `present:-7` |
| `firstValueAsLong("Nope")` | `empty` |
| `firstValueAsLong("Accept")` | `NumberFormatException: For input string: "text/plain"` |
| `toString()` | `java.net.http.HttpHeaders@15505eda { {Accept=[text/plain, text/html], X-Num=[42]} }` |
| `getClass()` | `java.net.http.HttpHeaders` |

And the ordering discriminator, which the previous `map()` got wrong and no
existing probe could see because every earlier example was already sorted:

```text
header("Z-Last","1").header("a-mid","2").header("A-First","3")
  HotSpot map()  = {A-First=[3], a-mid=[2], Z-Last=[1]}
  HotSpot keys   = [A-First, a-mid, Z-Last]
```

**Case-insensitively SORTED, not first-seen.** (The real backing store is a
`TreeMap<>(String.CASE_INSENSITIVE_ORDER)` made unmodifiable.) Values *within*
one name stay in insertion order. A 40-header request reports
`map().size() == 40`.

### 2.3 `BodyPublishers`, MEASURED

| call | HotSpot `contentLength()` |
|---|---|
| `noBody()` | `0` |
| `ofString("hello")` | `5` |
| `ofString("h\u00e9llo")` | `6` — UTF-8 **bytes**, not chars |
| `ofByteArray(new byte[5])` | `5` |
| `ofByteArray(new byte[0])` | `0` |
| `fromPublisher(p)` | `-1` |
| `fromPublisher(p, 12L)` | `12` |
| `ofInputStream(...)` | `-1` |

---

## 3. CratonVM before, MEASURED — the same rows on the same binary

`HttpProbe3.java`, run on both VMs back to back. The `--jdk-only` column is
transcribed verbatim:

| call | HotSpot | CratonVM before |
|---|---|---|
| `plain.method()` / `uri()` / `timeout()` | GET / the URI / empty | **identical** |
| `plain.version()` | empty | `AbstractMethodError: method java/net/http/HttpRequest.version()Ljava/util/Optional; has no Code attribute` |
| `plain.bodyPublisher()` | empty | `AbstractMethodError: ... bodyPublisher()Ljava/util/Optional; ...` |
| `plain.expectContinue()` | false | `AbstractMethodError: ... expectContinue()Z ...` |
| `plain.headers()` | `{}` | `AbstractMethodError: ... headers()Ljava/net/http/HttpHeaders; ...` |
| the same four on **every** one of the 20 builder shapes in §2 | as tabled | **the same four AbstractMethodErrors, every time** |
| `BodyPublishers.ofString("hi").contentLength()` | `2` | `AbstractMethodError: method java/net/http/HttpRequest$BodyPublisher.contentLength()J has no Code attribute` |
| `BodyPublishers.ofByteArray(new byte[5]).contentLength()` | `5` | same `AbstractMethodError` |
| `BodyPublishers.noBody().contentLength()` | `0` | same `AbstractMethodError` |
| `builder.setHeader("A","1")` | builder | `AbstractMethodError: ... setHeader(Ljava/lang/String;Ljava/lang/String;)... ` |
| `builder.headers("A","1")` | builder | `AbstractMethodError: ... headers([Ljava/lang/String;)...` |
| `builder.copy()` | builder | `AbstractMethodError: ... copy()...` |
| `builder.HEAD().build().method()` | HEAD | **HEAD** — the default method's bytecode runs |
| `builder.HEAD().build().bodyPublisher()` | **empty** | (unreachable before; the default body stores a publisher, so this would be present:0) |
| `timeout(Duration.ofSeconds(-1))` | `IllegalArgumentException: Invalid duration: PT-1S` | `IllegalArgumentException: HttpRequest timeout must be positive` — right refusal, **wrong words** |
| `method("", noBody())` | `IllegalArgumentException: illegal method <empty string>` | **accepted**, returns the builder |
| `newBuilder().build().uri()` | `IllegalStateException: uri is null` | **accepted**, returns the empty URI |
| `newBuilder(URI.create("ftp://h/x")).build().uri()` | `IllegalArgumentException: invalid URI scheme ftp` | **accepted**, returns `ftp://h/x` |
| `request.toString()` | `http://cratonvm.invalid/x GET` | `java.net.http.HttpRequest@4b3` |
| `HttpClient.Version.HTTP_2.name()` / `valueOf` / identity | HTTP_2 / HTTP_2 / true | **identical** — the enum side is sound |

Three of those rows are the ones that make this more than four registrations:
`contentLength` is a sixth `AbstractMethodError` waiting behind the four,
`setHeader`/`headers`/`copy` are three more on the builder, and the last four
rows are silent wrong answers rather than refusals — an empty URI and an `ftp:`
request both went through without a word.

---

## 4. The registry, MEASURED — who owns what

```bash
"$CV" --java-home "$JAVA_HOME" --jdk-only \
  --dump-native-registry 'C:\craton\CratonVM1\scratchpad\reg-before.txt' \
  -cp <cp> RJdkOptionalShape
```

(The flag must precede the main class or it is ignored silently, and the path
must be a Windows path — Git Bash's `/tmp` is a POSIX mapping the VM cannot
resolve.) Every row below is `owns_slot=true, overwrote=null`:

```text
java/net/http/HttpRequest    method        ()Ljava/lang/String;    net_phase_e.rs:12608
java/net/http/HttpRequest    uri           ()Ljava/net/URI;        net_phase_e.rs:12615
java/net/http/HttpRequest    timeout       ()Ljava/util/Optional;  net_phase_e.rs:12579  invocations=2
java/net/http/HttpRequest    newBuilder    ×2 (statics)
java/net/http/HttpHeaders    map           ()Ljava/util/Map;       net_phase_e.rs:13017  invocations=0
java/net/http/HttpRequest$BodyPublisher   -- NO ROWS AT ALL --
```

and, for the control:

```text
java/net/http/HttpClient  12 rows — cookieHandler, connectTimeout, followRedirects,
                          proxy, sslContext, sslParameters, authenticator, version,
                          executor, send, sendAsync ×2  (+ close/shutdown/shutdownNow/
                          isTerminated/awaitTermination)  ALL owns_slot=true
```

**Shadowing check, as the assignment requires.** Before adding any
registration I grepped the whole repo for each `(class, name, descriptor)`
triple and checked call order. Result:

* `http2.rs:2429 register_http_headers` registers `map`, `firstValue`,
  `allValues` and `firstValueAsLong` on **`java/net/http/HttpHeaders`** — the
  exact class this record adds three of those four to — against a completely
  different layout (three `Int` counters `HDR_COUNT`/`HDR_HAS_CT`/`HDR_HAS_CL`,
  a 3-slot allocation) and it answers `allValues` from a
  content-type/content-length flag pair rather than from any real header.
* `http2.rs:2025 register_http_request_builder` registers on
  **`java/net/http/HttpRequest$Builder`**, allocating **8** slots.
* MEASURED: **neither reaches the `--jdk-only` registry.** The dump carries
  exactly ONE `java/net/http/HttpHeaders` row (`map`, from `net_phase_e.rs`) and
  every `HttpRequest$Builder` row is `net_phase_e.rs`. `register_http2_natives`
  is not on this boot path.
* That is a fact about the current boot order, not a safety property. Nominated
  as **N1** — `register()` is last-write-wins with no unregister API, and if
  `http2.rs`'s registrar is ever added to this path it will overwrite four
  accessors with bodies that read a layout nothing on this path mints.

The same sweep found the *positive* case the assignment warned about
(`register_https_session_accessors` shadowing five of six HTTPS session
accessors from `http_url_connection.rs:404`) is **not** in this family: no
triple this record registers is registered anywhere else on this boot path.

**Three shapes for one class.** `java/net/http/HttpHeaders` is allocated with
**1** slot by `net_phase_e.rs:11333`, **2** by `http_client.rs:1696` and **3** by
`http2.rs:684`. Both allocators in files this lane owns now route through the
single minter `re5_make_http_headers`; `http2.rs`'s is N1.

---

## 5. What changed

### 5.1 `native-builtins/src/net_phase_e.rs`

**(a) The slot map is named, and `build()` copies all of it.** The builder and
the request shared an unnamed five-slot layout addressed by integer literals,
and `build()` copied `0..5` — a hard-coded prefix. Slots 0..=4 now have names
(`RE5_REQUEST_METHOD`/`URI`/`BODY`/`HEADERS`/`TIMEOUT_FIELD`), three are new
(`EXPECT_CONTINUE`, `VERSION`, `BODY_PUBLISHER`), and both the allocation and
the copy are driven by the single constant `RE5_REQUEST_NUM_FIELDS`. A unit test
asserts the map is dense, distinct and equal to `0..NUM_FIELDS`, because the
`0..5` literal is precisely how a slot added later would be dropped in silence.

`java.net.http.HttpRequest` declares **zero** instance fields and
`HttpRequest$Builder` is an interface, so `try_alloc_concurrent_synthetic`'s
`class_num_total_fields` is 0 for both and widening 5 → 8 cannot alias a real
field. (Checked, not assumed.)

**(b) The four missing accessors.** `version()`, `bodyPublisher()`,
`expectContinue()`, `headers()` — registered beside `timeout()`, which is the
pattern that already works. `version`/`bodyPublisher` go through
`re5_optional` (`Optional.ofNullable`), so an unset slot is
`Optional.empty` and not a flag-shaped Optional — the defect `E13-1`/`E2-1`
removed from this same family. `expectContinue` returns `Value::Int`, which is
correct *here* because `(Z)` really is an int, and `RJdkOptionalShape.httpmint`
says so in a comment as the family's negative control.

**(c) The setters that discarded their argument.** `expectContinue(Z)` and
`version(Version)` were `|_ctx, args| Ok(Some(args[0]))` — they returned the
builder to keep the fluent chain alive and threw the value away. That is the
other half of the same defect: even with (b) in place there would have been
nothing to read. `POST`/`PUT`/`method` now record the publisher OBJECT as well
as its payload (two questions, two slots), and `GET`/`DELETE`/`HEAD` CLEAR it.

**(d) `setHeader`, `headers(String...)`, `copy()`, `HEAD()`** — four builder
methods with no native, three of which threw `AbstractMethodError` (MEASURED,
§3) and one of which (`HEAD`, a default method) ran real bytecode that would
invent a body publisher.

**(e) `BodyPublisher.contentLength()J`** — the 0-of-1 interface.
`fromPublisher` now carries a DECLARED length (`-1` for the one-arg overload,
the caller's value for the two-arg one) in a second slot, because deriving it
from the payload would subscribe a `Flow.Publisher` inside a getter. The derive
path only ever sees a `String` or a `byte[]`.

**(f) The `HttpHeaders` readers.** `firstValue`, `allValues`,
`firstValueAsLong`, `toString`, all sharing one grouping helper with `map()` so
the five cannot come to disagree about what a header name means. `map()` now
sorts case-insensitively (§2.2) instead of preserving first-seen order.
`firstValueAsLong` on a non-numeric header raises `NumberFormatException` with
the oracle's message rather than answering `empty` — an unparsable
`Content-Length` must not look like an absent one.

**(g) The refusals of §2.1**, each with the measured message, including the
`timeout` message that was the right refusal in the wrong words.

**(h) The header array grows.** It was a fixed 32 entries and `header()` simply
stopped writing when it filled; MEASURED, HotSpot reports `map().size() == 40`
for 40 headers. A silently dropped header is the failure mode this directory
exists to remove.

**(i) Pinning.** Every new helper that allocates (`re5_new_request_builder`,
`re5_new_body_publisher`, `re5_builder_append_header`, `copy()`) pins its
operands across the allocation and reads them back through the pin, the way
`re5_make_http_headers` already did.

### 5.2 `native-builtins/src/http_client.rs`

`jdk/internal/net/http/HttpRequestImpl` is the implementation twin of the same
seven-accessor surface and answered **four** of them (`method`, `uri`,
`version`, `timeout`). The other three are added. The failure mode here is
*worse* than an `AbstractMethodError`: `HttpRequestImpl` is a real JDK class, so
an unregistered accessor does not refuse — it runs the JDK's own body against
this file's slot layout and returns a wrong answer. `headers()` in particular
would have read the real class's field out of slot 3, where this model keeps a
`String[]`.

`HttpResponseImpl.headers()` now mints its `HttpHeaders` through the shared
`re5_make_http_headers` instead of its own 2-slot allocation (§4, "three shapes
for one class").

### 5.3 Tests

Eleven new unit tests in the two files' existing `#[cfg(test)]` modules
(`net_phase_e.rs`'s `mod tests`, `http_client.rs`'s `mod http_client_tests`):
three surface censuses driven off the `javap` output in §0, two slot-map
coherence tests, and six behavioural tests over `MockNativeContext` covering
builder initialisation, header grouping, growth past 32, `setHeader`
replacement, publisher clearing, and the three distinct header refusal
messages.

---

## 6. Predicted after — and what would falsify it

`RJdkOptionalShape` should go from `process=172` + `AbstractMethodError` to
`httpmint=268` and `checks=1418`, matching the oracle. That is a PREDICTION.
The order in which the remaining assertions would fall, from `httpmint`'s own
source, is: the four `mint-*-absent-present` rows (need `version`/
`bodyPublisher`), then `mint-version-present-name` (needs `version` plus the
enum, which is already measured sound), then `mint-bodyPublisher-present-len`
and `mint-ofByteArray-len` (need `contentLength`, §0's fifth instance), then
`mint-expectContinue-roundtrip` (needs both the setter and the accessor).

**The one thing most likely to be wrong**, stated plainly so the next lane
checks it first: whether a registered native on
**`java/net/http/HttpHeaders`** actually runs. That class is a real, final JDK
class with real bytecode for all five methods, and it is **not** in
`force_native_over_real_jdk_bytecode`. The evidence that natives win anyway is
indirect: `java/util/Optional` is also absent from that list, also has
`real_declaring_method.has_code = true`, and its natives show
`invocations=244` in the dump — so a registered native on a real-bytecode class
does run, at least there. That is an inference from one family, not a
measurement of this one. **If `HttpHeaders`' natives do NOT win**, the real
JDK bodies will read slot 0 as the `Map` the class declares and find a
`String[]`, and `headers().map()` will answer something shaped like an array.
The fix is then either an entry in `force_native_over_real_jdk_bytecode`
(**N2**, not this lane's file) or storing a real case-insensitive unmodifiable
`Map` in slot 0 — which would be layout-faithful and make every real JDK body
correct, at the cost of changing a representation `http2.rs` also mints.

---

## 7. What this lane did NOT do

* **It did not build the binary, and therefore did not measure its own change.**
  The lane brief forbids `cargo build`/`check`/`test`. Everything in §§0–4 is
  MEASURED on the `2026-08-17 01:41:07` binary; §5's changes are PREDICTED to
  compile and PREDICTED to behave as §6 describes.
  `rustfmt --edition 2021 --check` was run **in place, in its tree** on both
  files (not on a copy — a copy makes rustfmt abort on unresolvable `mod`s and
  report success while writing nothing). `net_phase_e.rs` reports 54 diffs
  against 55 on the `HEAD` blob — one FEWER, because reformatting
  `re5_make_http_headers`' signature to `pub(crate)` removed a pre-existing
  hunk; `http_client.rs` reports 4 against 4. **No new hunk in either file.**
  That is a parse check, not a type check.
* **It did not fix the `getClass()` divergence.** The object is still stamped
  `java.net.http.HttpRequest` where HotSpot answers
  `jdk.internal.net.http.ImmutableHttpRequest`. That is Mechanism A itself —
  the mint — and closing it means minting a concrete type, which is a change to
  what class this VM fabricates, not to which methods it registers. Every
  accessor now answers correctly *on* the fabricated object; the fabrication
  stands. Nominated as **N3**.
* **It did not verify that `HttpRequest.toString()` is reachable.** The
  registration is on `java/net/http/HttpRequest`, which does not DECLARE
  `toString` — `Object.toString` is what resolves. Whether the receiver's-own-
  class native lookup finds a row registered on a class that does not declare
  the method is not settled here. The registration is correct if reached and
  inert if not; it is called out rather than claimed, because "a fix that landed
  in dead code" is a failure this branch has already recorded once (`8c72d23ca`).
* **It did not register `ofInputStream`, `ofFile`, `ofByteArrays`, `concat` or
  `ofString(String, Charset)` on `BodyPublishers`.** Those are static methods on
  a real concrete class and run real JDK bytecode today, returning real JDK
  publisher objects. That is the better outcome and it is left alone. Their
  `contentLength()` is therefore answered by real bytecode, not by the new
  interface-door native.
* **It did not touch `http2.rs`, `lib.rs`, `t27_tls.rs`,
  `http_url_connection.rs`, `phases_late/nio_file.rs`, `native_override.rs`,
  `INDEX.md` or `README.md`.**
* **It did not run `regression-suite/run.sh`,** nor any state-changing git
  command.

---

## 8. Baselines, MEASURED, on the same stale binary

Re-run after the edits, purely to establish that the binary did not change under
this lane (`mtime 2026-08-17 01:41:07 -0300` at the start of the lane, at 02:08,
and at the end):

| vector | expected green | measured now |
|---|---|---|
| `RJdkNet` | `checks=81` | `checks=81` |
| `RSslNullSession` | `checks=89` | `checks=89`, `failures=0` |
| `RSslLiveSession` | red | red — `FAILED phase=verifier javax.net.ssl.SSLPeerUnverifiedException: Certificate for <127.0.0.1> does not match any of the subject alternative names or the common name: HTTPS hostname wrong, should be <127.0.0.1>` (reaches `invalidate=14` first) |
| `RJdkAsyncChannel` | `checks=141` | `checks=141` |
| `RJdkOptionalShape` | — | unchanged: `process=172`, then the same `AbstractMethodError` |

All five are byte-identical to their pre-edit runs, which is exactly what a
source-only change against an unrebuilt binary must produce. **No number in this
section is evidence about the fix.** The next lane to build should re-run all
five; only `RJdkOptionalShape` is expected to move.

Only `RJdkOptionalShape` in the whole regression suite mentions `HttpRequest`
(`grep -l HttpRequest regression-suite/src/*.java` — one file), so the blast
radius of the new refusals in §5.1(g) is confined to that vector plus
application code.

---

## 9. Nominations

**N1 — `native-builtins/src/http2.rs` (a shadowing registrar aimed at four of
the accessors this record adds).** `register_http_headers` at `:2429` registers
`map`, `firstValue`, `allValues` and `firstValueAsLong` on
`java/net/http/HttpHeaders` against a three-`Int`-counter layout
(`HDR_COUNT`/`HDR_HAS_CT`/`HDR_HAS_CL`, allocated at `:684` with 3 slots), and
`allValues("Accept")` there answers from a content-type flag, not from any
header. `register_http_request_builder` at `:2025` does the same for
`HttpRequest$Builder` with an 8-slot allocation. MEASURED: neither is on the
`--jdk-only` boot path today — the dump carries exactly one `HttpHeaders` row
and it is `net_phase_e.rs`'s. But `register()` is last-write-wins with no
unregister API, so whichever is called last simply wins. Either delete the
`http2.rs` copies, or make them route through `net_phase_e`'s
`re5_make_http_headers` shape, or pin the boot order with a ratchet test the
way `t27_tls.rs` pins `register_re6_ssl_context`. Verify with
`--dump-native-registry`: landed when every `java/net/http/HttpHeaders` row
reports one `registered_by` and `overwrote=null`.

**N2 — `vm/src/runtime/interpreter/native_override.rs`
(`force_native_over_real_jdk_bytecode`).** `java/net/http/HttpHeaders` is a
real, final JDK class with real bytecode for `map`, `firstValue`, `allValues`,
`firstValueAsLong` and `toString`, and every object of that class this VM
produces is CratonVM-minted with a `String[]` in the slot the real class
declares as a `Map`. If a registered native does not preempt the real body
there, all five run over a layout that is not the JDK's. §6 explains why this
lane could not settle it from outside a build. Measure it first
(`headers().map().getClass().getName()` — `java.util.Collections$UnmodifiableMap`
on the oracle); add the entry only if the measurement says the native loses.

**N3 — the mint itself (`net_phase_e.rs`, but a design change, not a
registration).** `HttpRequest$Builder.build()` stamps its result with the
ABSTRACT class `java/net/http/HttpRequest`, and `BodyPublishers.*` stamp theirs
with the INTERFACE `HttpRequest$BodyPublisher`. Every accessor now answers, so
the symptom is gone, but `getClass()` still diverges on every row of §2 and any
future method added to those types is another `AbstractMethodError` waiting.
This is `G13-1`'s Mechanism A at its root: the fix is to mint a concrete
CratonVM-owned subtype (or the real `jdk.internal.net.http.ImmutableHttpRequest`)
so the receiver is never the abstract declaration. Doing that touches
`re5_do_request` and every `is_synthetic_shape` check in `http2.rs`, which is
why it is nominated rather than done here.

**N4 — `docs/known-issues/jdk-only/BASELINE-20260817.md` and `G13-1`'s §8 N3
(both outside this lane).** `G13-1` N3 counts `HttpRequest` at "3 of 7" and
lists four accessors to register. That is right as far as it goes and it is not
the whole gap: `BodyPublisher.contentLength` (0 of 1), `Builder.setHeader`,
`Builder.headers(String...)` and `Builder.copy()` are four more
`AbstractMethodError`s on the same vector, MEASURED in §3, and `HEAD()` plus the
four refusal rows are silent wrong answers. Suggested amendment to `G13-1` §8
N3, appended after "Verify with `G13Http.java`":

- exact new text: `**Extended by `G29-1` (MEASURED): the gap is nine methods, not four.** `HttpRequest$BodyPublisher.contentLength()J` has ZERO registrations (0 of 1, the `PathMatcher` ratio), and `HttpRequest$Builder.setHeader`, `.headers(String...)` and `.copy()` throw the same `AbstractMethodError`. `HEAD()` is a default method whose bytecode invents a body publisher HotSpot does not report.`

and to `BASELINE-20260817.md`'s Triage table:

- exact old text: `` | `RJdkOptionalShape` | `AbstractMethodError: method java/net/http/HttpRequest.version()Ljava/util/Optional; has no Code attribute` | interface doors | ``
- exact new text: `` | `RJdkOptionalShape` | `AbstractMethodError: method java/net/http/HttpRequest.version()Ljava/util/Optional; has no Code attribute` | `net_phase_e.rs` missing accessors (G13-1 N3, extended by G29-1) — FIXED IN SOURCE, unbuilt | ``

---

## 10. The one-sentence version

`HttpRequest` was 3 of 7 because a registrar stopped early, and sweeping the
rest of what that registrar mints found the same stop four more times — a
builder missing three of its fifteen methods, an interface with one method and
no implementation of it, and a header class with one reader out of five — so the
count that mattered was never four accessors but nine methods and four silent
wrong answers, on objects the VM had itself just fabricated out of abstract
declarations.
