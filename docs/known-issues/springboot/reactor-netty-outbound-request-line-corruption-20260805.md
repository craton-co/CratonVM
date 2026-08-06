# Reactor Netty HTTP client: the two SPACES in the request line come out as garbage bytes — everything else is intact

**Status: OPEN — found 2026-08-05, re-read and re-scoped 2026-08-05.**
Believed fixed by `383e7f5cf` (recycled `JitInvokeInfo` address); **not
confirmed** — see "Why this is still open".

## Symptom

`ReactorClientHttpRequestFactoryBuilderTests` — 4/33 methods fail against a
local embedded Tomcat, all with a 400 from the *server*
(HotSpot: 33/33, confirmed on both the Azure baseline and Windows):

- `redirectDontFollow(String="POST")` — expected 302, got 400
- `redirectDefault(String="GET")`, `redirectDefault(String="PATCH")` — expected 200, got 400
- `connectWithSslBundle(String="POST")` — body is Tomcat's 400 page, not the echo

## What is actually corrupted — the original reading was wrong

Tomcat's error page quotes the bytes it received. All three from the same run:

```
GETE/redirecteHTTP/1.10x0d0x0aaccept-encoding:
PATCHr/redirectTHTTP/1.10x0d0x0aaccept-encoding:
POST0xe1/0x00HTTP/1.10x0d0x0aaccept-encoding:
```

Line them up against what should have been sent —
`GET /redirect HTTP/1.1\r\naccept-encoding: `:

| | expected | received |
|---|---|---|
| method | `GET` / `PATCH` / `POST` | **correct** |
| SP | `0x20` | `E` / `r` / `0xe1` |
| request-target | `/redirect` / `/` | **correct** |
| SP | `0x20` | `e` / `T` / `0x00` |
| version | `HTTP/1.1` | **correct** |
| CRLF | `0x0d 0x0a` | **correct** |
| headers | `accept-encoding: …` | **correct** |

**Only the two SP separators are wrong, and they are wrong differently every
time.** The first version of this page read the same bytes as "the path and
the line terminator were replaced by two garbage bytes" and concluded raw
outbound-buffer corruption, pointing the next reader at `DirectByteBuffer` /
`Unsafe` / `sun.nio.ch` socket writes. That is the wrong place: a buffer
overwrite does not preserve a nine-byte path and a CRLF and hit exactly the
two single-byte writes between them.

`HttpObjectEncoder.encodeInitialLine` writes the request line as: bulk-copy
the method, **`buf.writeByte(SP)`**, bulk-write the URI, **`buf.writeByte(SP)`**,
bulk-write the version, `writeShort(CRLF_SHORT)`. Every bulk write and the
`writeShort` are correct; the two `writeByte(SP)` calls are not. `SP` is
`HttpConstants.SP`, a `static final byte` of value 32. So the value delivered
to a one-argument call is garbage while the surrounding calls on the same
buffer are fine.

## Where that points

"A compiled call receives an argument that belongs to something else" is
exactly the defect fixed on 2026-08-05 by `383e7f5cf` — *a recycled
`JitInvokeInfo` address let one call site serve another's dispatch*. Its own
report is the same shape from the other end: *"An int had been delivered where
an `Annotation[]` belongs — and only argument 0 was wrong; args 1/2/4/5 all
held plausible heap pointers."* Here an int is delivered where the int 32
belongs, at two call sites, in a method whose other calls are untouched.

The full-suite binary that produced this page (`2398d729e`, built 11:36 UTC,
run 11:54) **predates** that fix.

## Why this is still open

Because it has not been reproduced since, and a defect that cannot be
reproduced cannot be declared fixed by a commit it merely resembles.

| host | binary | shape | runs | result |
|---|---|---|---|---|
| Azure Linux (the host that found it) | `2398d729e` — **pre-fix** | serial | 3 | 33/33 |
| Azure Linux | `2398d729e` — **pre-fix** | 8 concurrent, load avg 28 | 8 | 33/33 |
| Windows | `383e7f5cf^` — **pre-fix** | serial | 1 | 33/33 |
| Windows | current dev — post-fix | serial | 5 | 33/33 |

The one observation is from an 8-shard × 2-parallel full-suite run (16
concurrent CratonVM processes). The recycling this depends on needs
`CompiledMethod`s to be DROPPED so their `JitInvokeInfo` boxes are freed and
re-issued, which a single-class run may never do — so "quiet runs are green"
is weak evidence either way, and eight copies of ONE class is not the same
churn as 1975 different ones.

**What would settle it:** the next full-suite run on a post-`383e7f5cf`
binary. If this class passes there, retire the page citing that run. If it
fails there, the byte table above is where to start, and the first question to
answer is whether it is `writeByte`'s ARGUMENT or its receiver's write INDEX
that is wrong.

## A second anomaly in the same log, unexplained

268 occurrences of

```
WARN io.netty.channel.nio.NioIoHandler -- Selector.select() returned prematurely 512 times in a row; rebuilding Selector
```

roughly one rebuild every 10 ms for the length of the run. That is Netty's
defence against the Linux epoll spin bug, and it means CratonVM's
`Selector.select()` returned with no ready keys 512 times in a row,
repeatedly. None of the eleven Azure re-runs above produced a single one, so
it is load- or timing-dependent too. It is **not** established to be the same
defect, or even a defect at all — but a client whose selector is being rebuilt
every 10 ms is not in a normal state, and if the byte corruption comes back
this is the other thread to pull.

## Affected classes
- `module/spring-boot-http-client` —
  `org.springframework.boot.http.client.ReactorClientHttpRequestFactoryBuilderTests`
  (4/33: `redirectDontFollow`, `redirectDefault` ×2, `connectWithSslBundle`)

## Reproducing (both hosts)

```
apps/spring-boot-suite-runner/run-single-class.ps1 -Module module/spring-boot-http-client \
  -ClassName org.springframework.boot.http.client.ReactorClientHttpRequestFactoryBuilderTests -Exe <exe>
```

```
/data/sbrun.sh <exe> jit module/spring-boot-http-client \
  org.springframework.boot.http.client.ReactorClientHttpRequestFactoryBuilderTests /tmp/out 3 900
```

~13 s per run on Azure, ~3 min on Windows.

## See also
`jdk-httpclient-sslbundle-tls-handshake-eintr-20260805.md` — the JDK
`HttpClient` variant of the same test method (`connectWithSslBundle`) fails
with an EINTR during the TLS handshake read. Different client stack, different
symptom; the shared-lower-level-cause guess in the first version of this page
is weaker now that the corruption here reads as an argument-passing shape
rather than an I/O one.
