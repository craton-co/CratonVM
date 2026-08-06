# Reactor Netty request line: the two SPACES came out as stale buffer bytes — the recycled `JitInvokeInfo` address, reproduced byte-for-byte

**Status: RESOLVED — 2026-08-06.** Fixed by `383e7f5cf` (*a recycled
`JitInvokeInfo` address let one call site serve another's dispatch*), which
landed on `dev` after the binary that found this. Attribution is not by
resemblance: the defect was switched back on and the **identical byte
sequences** came back.

## The original reading was wrong, and it pointed at the wrong subsystem

Tomcat quotes the bytes it received. The first version of this page read

```
POST0xe1/0x00HTTP/1.10x0d0x0aaccept-encoding:
```

as "the path and the CRLF were replaced by two garbage bytes → raw
outbound-buffer corruption", and sent the next reader at `DirectByteBuffer` /
`Unsafe` / `sun.nio.ch` socket writes. Lined up field by field against
`GET /redirect HTTP/1.1\r\n`, the bytes say the opposite:

| | expected | received |
|---|---|---|
| method | `GET`/`PATCH`/`POST`/`PUT` | **correct** |
| SP | `0x20` | `E` / `r` / `0xe1` / `C` / `0x00` |
| request-target | `/redirect` | **correct** |
| SP | `0x20` | `e` / `T` / `0x00` / `c` / `0x01` |
| version | `HTTP/1.1` | **correct** |
| CRLF + headers | `0x0d 0x0a accept-encoding: …` | **correct** |

**Only the two SP separators are wrong**, the nine-byte path between them is
intact, and the length is unchanged. A buffer overwrite does not do that.

`HttpRequestEncoder.encodeInitialLine` writes exactly those two bytes with a
one-argument call and everything else in bulk:

```
133: aload_1;  134: bipush 32;  136: invokevirtual ByteBuf.writeByte:(I)   <- SP 1
139: aload 4;  141: getstatic UTF_8;  144: invokevirtual writeCharSequence  <- path, correct
164: aload_1;  165: bipush 32;  167: invokevirtual ByteBuf.writeByte:(I)   <- SP 2
181: aload_1;  182: sipush 3338;  185: invokestatic writeShortBE           <- CRLF, correct
```

The argument is `bipush 32` — **a constant in the bytecode**. There is no field
read to get wrong and no arithmetic to get wrong. Either the call delivered
something else, or it did not reach `writeByte` at all and the position kept
whatever the POOLED buffer already held. That is a dispatch defect, and the
byte values support the second reading: `E`/`e` and `C`/`c` differ from each
other by exactly `0x20`, and `0xe1`/`0x00` show up on the run whose previous
buffer content was TLS.

## Reproduced, on one binary, by switching the defect

`diag/site-alias-detector-20260806` (pushed) adds
`CRATONVM_JIT_NO_SITE_MEMO_FLUSH=prefix`, which makes
`clear_site_keyed_dispatch_memos` flush **exactly the two memos the code
flushed before `383e7f5cf`** (`DISPATCH_CACHE`, `VIRTUAL_DISPATCH_CACHE`) and
leave the other six stale. One binary, both arms, same host, back to back.

**Positive control first** — the switch has to reproduce a KNOWN failure or a
green in the other arm proves nothing:

| host | `WebMvcAutoConfigurationTests` | defect on | fix on |
|---|---|---|---|
| Windows | 93 tests | **88 failed** | 0 failed |
| Azure Linux | 93 tests | **89 failed** | 0 failed |

88/93 is the number the 2026-08-05 full-suite run recorded for that class.

**Then this class**, `ReactorClientHttpRequestFactoryBuilderTests`:

| host | shape | arm | result |
|---|---|---|---|
| Azure | 8 concurrent | **defect on** | 1 FAIL (5/33), 2 crash at discovery, **8 corrupted request lines** |
| Azure | 8 concurrent, load avg 645 | **fix on** | **8/8 PASS, 0 corrupted lines** |
| Azure | 10 serial | **defect on** | 3 crash at discovery, 1 FAIL (3/33), 6 PASS |
| Azure | 10 serial | fix on | 10/10 PASS |
| Azure | 5 serial (all memos stale) | **defect on** | 2 FAIL, `containersFailed=4` |
| Windows | 3 serial | **defect on** | 1 FAIL (`MockitoException: cannot mock ReactorResourceFactory`) |

The corrupted lines from the defect-on concurrent arm:

```
2  GET0x00/0x01HTTP/1.10x0d0x0aaccept-encoding:
1  GETE/redirecteHTTP/1.10x0d0x0aaccept-encoding:     <- byte-identical to the 08-05 report
2  POST0xe1/0x00HTTP/1.10x0d0x0aaccept-encoding:      <- byte-identical to the 08-05 report
1  PUTC/redirectcHTTP/1.10x0d0x0aaccept-encoding:
```

Two of those are the **exact strings** this page was opened with. The fixed arm
ran at a HIGHER load average than the defect arm (645 vs 306) and produced
none — so the green is not a quieter machine.

Two further fingerprints, both naming the same defect:

* the discovery crashes are
  `NoSuchMethodError: java.lang.Class.annotationType()` from
  `AnnotationUtils.findAnnotation` — **the signature `383e7f5cf`'s own commit
  message quotes** as its `VIRTUAL_TARGET_CACHE` face;
* with every memo left stale, the four containers that fail to resolve are
  `redirectDontFollow`, `redirectDefault`, `connectWithSslBundle` and
  `connectWithSslBundleAndOptionsMismatch` — **three of the four methods this
  page reported**.

## Why it never reproduced before the switch existed

Eleven runs on the original host with the original pre-fix binary — 3 serial
and 8 concurrent — were all 33/33, and so were 6 Windows runs. The defect needs
enough `CompiledMethod`s to be DROPPED for a `JitInvokeInfo` address to be
re-issued, and one class in a quiet process does not supply that. It took
8-way concurrency at load ~300 to produce it even with the defect deliberately
on. The lesson, recorded in
[[reference_read_the_quoted_bytes_before_blaming_the_buffer_layer]]: for a
full-suite-only failure, **a quiet green settles nothing in either direction —
switch the suspected defect instead of trying to out-wait it.**

## What landed with this

Only the read-only half: `CRATONVM_DBG_SITE_ALIAS=1` records the
`(class, method, descriptor)` each `JitSiteKey` first named and names every key
that later denotes a different one:

```
[site-alias] #40 of 1546 keys: key=0x23962002f40
  WAS java/lang/String.checkBoundsOffCount(III)I
  NOW java/lang/StringBuilder.append(Ljava/lang/String;)Ljava/lang/StringBuilder;
```

It answers "does this workload recycle site keys at all?" in ONE run — this
class does, abundantly, including on Netty and ByteBuddy sites — which is the
precondition question the whole 2026-08-05 corruption family turns on.

**The running totals ride on the event line on purpose.** The first cut put
them in a shutdown summary, and that summary never printed once: a JUnit
runner ends the process with `System.exit`, so anything emitted at VM shutdown
is unreachable in exactly the workloads this exists for. Printing is capped at
40 events; the count keeps going.

The defect-injection switch (`CRATONVM_JIT_NO_SITE_MEMO_FLUSH`) is deliberately
**not** on `dev`; it lives on `diag/site-alias-detector-20260806`, which is
pushed. Check that branch out to re-run the A/B above.

## Affected classes

- `module/spring-boot-http-client` —
  `org.springframework.boot.http.client.ReactorClientHttpRequestFactoryBuilderTests`
  (4/33 on 2026-08-05; 33/33 on `dev` now, 10 serial + 8 concurrent on Azure,
  5 serial + 3 concurrent-equivalent on Windows).

## The other anomaly in the original log, still unexplained

268 × `Selector.select() returned prematurely 512 times in a row; rebuilding
Selector` — roughly one rebuild per 10 ms for the length of the run. It did not
appear in ANY of the ~40 runs here, defect-on or defect-off, on either host. It
is therefore not part of this defect, and it is not established to be a defect
at all; but a Netty client whose selector is being rebuilt every 10 ms is not in
a normal state. If it recurs, it is its own page.
