# BUG-TC0622 — RemoteIpFilter loopback-proxy non-match (Gap A) + WebDAV bounded ByteArrayOutputStream not enforced (Gap B)

Two small, independent CratonVM gaps from the Apache Tomcat full suite. Both
tests PASS on HotSpot, FAIL on CratonVM.

**Run date:** 2026-06-23
**Binary:** dev `df11ac00` (worktree exe `C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe`)
**Classpath:** `C:\craton\CratonVM\apps\tomcat\.tooling\cp.txt`
**Logs:** `C:\craton\CratonVM\apps\tomcat\.tooling\results\tcfull0622\craton\`
  * `org.apache.catalina.filters.TestRemoteIpFilter.log[.err]`
  * `org.apache.catalina.servlets.TestWebdavBoundedByteArrayOutputStream.log[.err]`
**HotSpot reference:** `...\results\clean_hs\hotspot\` (both classes green there).

---

## Gap A — `TestRemoteIpFilter`: loopback connection not recognised as an internal proxy

**Test:** `org.apache.catalina.filters.TestRemoteIpFilter`.
**Result (doc original, dev `df11ac00`):** CratonVM `Tests run: 26, Failures: 2` —
`testWithTomcatServer`, `testJSessionIdSecureAttributeMissing`.
**HotSpot: `OK (26 tests)`.**
**Severity:** Medium (X-Forwarded-* honouring broken for loopback/CIDR-matched peers).

> ### ✅ RESOLVED (2026-06-23, branch `fix/tc0622-remoteip-loopback`)
>
> **Status on current dev (`0284432c`): the test now PASSES `OK (26 tests)`** —
> the two documented failures no longer reproduce (verified single-process via
> `JUnitCore` and via the parallel `run-suite.ps1` harness, 26/26). The original
> ComparisonFailure / NPE were already fixed by a net-layer change merged in the
> `df11ac00..0284432c` range (most likely the real-JDK `HttpURLConnection`
> carrier work, `7b8f37d1`). The infamous netmask `IllegalArgumentException` is
> confirmed a red herring exactly as analysed below.
>
> **A genuine, still-present divergence in the predicted subsystem was found and
> fixed anyway.** The doc correctly fingered the InetAddress mirror. Probing
> `NetMaskSet.parse("127.0.0.0/8,…").contains(...)` showed:
> `contains("::ffff:127.0.0.1")` returned **false** on CratonVM vs **true** on
> HotSpot. Root cause: CratonVM's `InetAddress.getByName` left an **IPv4-mapped
> IPv6 literal** (`::ffff:a.b.c.d`) as a 16-byte `Inet6Address`, whereas HotSpot
> folds it to a 4-byte `Inet4Address`. `NetMask.matches` rejects on
> `candidate.length != netaddr.length` (16 vs 4), so every IPv4 CIDR test against
> a v4-mapped peer silently fails — precisely failure-mode (1) hypothesised
> below. This is latent in the suite only because the harness sets
> `-Djava.net.preferIPv4Stack=true`; it would re-trigger this exact bug class on
> any dual-stack accept.
>
> **Fix:** `native-builtins/src/net_phase_e.rs` — new `hotspot_ip_string`
> helper applied in `alloc_inet_address` (the single universal mirror builder, so
> `getByName`/`getAllByName`/`getByAddress`/socket-peer mirrors are all covered).
> It canonicalises the stored address string to HotSpot's exact textual form:
> (a) `::ffff:a.b.c.d` is folded to its IPv4 dotted-quad → `Inet4Address` with a
> 4-byte `getAddress()` (the CIDR-match fix); (b) genuine IPv6 is rendered in
> HotSpot's full uncompressed eight-group form
> (`Inet6Address.numericToTextFormat`), e.g. `::1` → `0:0:0:0:0:0:0:1`, instead
> of Rust's RFC-5952 compressed `::1`, so `getHostAddress()`/`toString()` are
> byte-identical to HotSpot. Plain IPv4 and non-IP hosts pass through unchanged.
> Verified: `getByName("::ffff:127.0.0.1")` → `Inet4Address` / `127.0.0.1` /
> `[127,0,0,1]`; `getByName("::1").getHostAddress()` → `0:0:0:0:0:0:0:1`
> (== HotSpot); `contains("::ffff:127.0.0.1")` now `true`; `TestRemoteIpFilter`
> 26/26; net unit tests green (incl. new `hotspot_ip_string_matches_jdk_text_form`).

**Recommendation (original): HANDOFF** (needs a live server repro to pin the exact byte/string divergence; see below).

### Symptom

The two visible failures are:

```
1) testJSessionIdSecureAttributeMissing
   java.lang.NullPointerException: Cannot invoke "java.util.List.get(int)"
     at ...TestRemoteIpFilter.testJSessionIdSecureAttributeMissing(TestRemoteIpFilter.java:829)
2) testWithTomcatServer
   org.junit.ComparisonFailure:
     at ...TestRemoteIpFilter.testWithTomcatServer(TestRemoteIpFilter.java:779)
```

* `testWithTomcatServer:779` — `assertEquals(expectedRemoteAddr /* "my-remote-addr" */, mockServlet.remoteAddr)`.
  The `X-Forwarded-For: my-remote-addr` header was **not** applied, so the servlet
  still saw the raw socket peer address.
* `testJSessionIdSecureAttributeMissing:829` — `resHeaders.get("Set-Cookie").get(0)`
  NPEs because no `Set-Cookie` header came back (the `X-Forwarded-Proto: https`
  was not honoured → session cookie not marked `Secure` / response shape differs).
  The NPE is **downstream** of the same root: the filter never trusted the request.

### Root cause (analysis)

**The `IllegalArgumentException: One or more netmasks provided are invalid:
192\.168\.0\.10|192\.168\.0\.11 ... The address [...] is not valid` line in the
`.err` log is a RED HERRING — it is NOT the cause.** It is *intentional* test
behaviour and appears **byte-for-byte identically in the HotSpot reference log**
(`clean_hs/.../TestRemoteIpFilter.log.err` lines 6, 155, 385) while HotSpot still
reports `OK (26 tests)`. Several in-process tests
(`testInvokeAllowedRemoteAddrWithNullRemoteIpHeader`, `testInvokeNotAllowedRemoteAddr`,
`testInvokeAllProxiesAreInternal`) deliberately feed the regex literal
`192\.168\.0\.10|192\.168\.0\.11` as `internalProxies`; the current Tomcat
`RemoteIpFilter` is **CIDR-only** (`NetMaskSet.parse` → `new NetMask(...)` →
`InetAddress.getByName(...)`), so that value is *correctly* rejected on both
runtimes. CratonVM's rejection is right: in `inet_address.rs::resolve_addrs` the
string is not an IPv4/IPv6 literal and `getaddrinfo`/`to_socket_addrs` fails on
the `\`/`|` characters → `UnknownHostException` → `netmask.invalidAddress`.
*(Filter source: `apps/tomcat/java/org/apache/catalina/filters/RemoteIpFilter.java`
lines 814-816, 1343-1347; `NetMaskSet.parse` / `NetMask(String)`.)*

The two genuine failures are the only **server tests** in the class
(`testWithTomcatServer:739`, `testJSessionIdSecureAttributeMissing:789`). They set
**no** `internalProxies` — they use the hard-coded default
(`RemoteIpFilter.java:814`):

```
NetMaskSet.parse("10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,169.254.0.0/16,"
               + "100.64.0.0/10,127.0.0.0/8,::1/128,fe80::/10,fc00::/7")
```

Both tests open a **real `HttpURLConnection` to `http://localhost:<port>`**, so the
connector reports a loopback peer (`127.0.0.1` or IPv6 `::1`). For the filter to
honour `X-Forwarded-For`, `RemoteIpFilter.isInternalProxy(remoteIp)` →
`checkIsCidr(internalProxies, remoteIp)` → `NetMaskSet.contains(remoteIp)` →
`InetAddress.getByName(remoteIp)` then `NetMask.matches(addr)` must return `true`
against `127.0.0.0/8` (or `::1/128`). On CratonVM it returns `false`, so the
peer is treated as untrusted and the forwarded headers are dropped → both
assertions fail. HotSpot matches loopback against the default set, so the headers
are applied.

The divergence is in the CratonVM **InetAddress mirror / loopback-peer plumbing**
used by the CIDR match, one of:

1. The string `request.getRemoteAddr()` reports for a loopback peer differs from
   what HotSpot reports (e.g. an IPv6-mapped/zoned form, or `0:0:0:0:0:0:0:1`
   rendered such that `getByName` resolves it to a non-loopback / wrong-length
   address). `NetMask.matches` first rejects on `candidate.length != netaddr.length`
   (4 vs 16), so a v4↔v6 family mismatch silently yields `false`.
2. `java.net.InetAddress.getAddress()` on the CratonVM mirror returns the wrong
   bytes/length. The mirror is built by `net_phase_e::alloc_inet_address_external`
   (`inet_address.rs::alloc_inet_address_mirror`); the `getAddress()[B` native is
   `net_phase_e.rs:2736` / `:2835`. If those bytes don't equal the four loopback
   octets `7f 00 00 01` (resp. the 16-byte `::1`), every CIDR compare in
   `NetMask.matches` fails.

Pinning which of (1)/(2) fires needs the server actually running (the loopback
peer address is only produced by the live NIO connector), hence HANDOFF.

### Reproduction

Server test — use the suite runner so the JVM args (`-Dtomcat.test.basedir`,
`tomcatbuild`/`temp`, `--add-opens`) are supplied:

```powershell
cd C:\craton\CratonVM\apps\tomcat
# via the harness (sets basedir/temp/add-opens):
.\.tooling\run-suite.ps1 -Exe C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe `
    -Tests org.apache.catalina.filters.TestRemoteIpFilter
```

Expected: `Tests run: 26, Failures: 2` (the two server tests). Minimal-cause
probe for the handoff (no server needed):

```java
// Should print true,true on HotSpot; expect at least one false on CratonVM.
java.net.InetAddress a4 = java.net.InetAddress.getByName("127.0.0.1");
java.net.InetAddress a6 = java.net.InetAddress.getByName("::1");
org.apache.catalina.util.NetMaskSet s = org.apache.catalina.util.NetMaskSet.parse("127.0.0.0/8,::1/128");
System.out.println(s.contains("127.0.0.1") + "," + s.contains("0:0:0:0:0:0:0:1"));
System.out.println(java.util.Arrays.toString(a4.getAddress())); // expect [127, 0, 0, 1]
System.out.println(java.util.Arrays.toString(a6.getAddress())); // expect 16 bytes ending in 1
```

### Recommendation — HANDOFF

Drive the probe above plus the server test; confirm whether the gap is
`getRemoteAddr()` string form or `InetAddress.getAddress()` byte content for the
loopback peer, then fix the offending native in `net_phase_e.rs` (mirror
`getAddress`/`getHostAddress` at `:2719`/`:2736`/`:2835`) or the connector
remote-addr plumbing. **Do not touch the regex/netmask path — it is correct and
HotSpot-identical.** Low blast radius once the loopback byte/string form is
confirmed.

---

## Gap B — `TestWebdavBoundedByteArrayOutputStream`: native `ByteArrayOutputStream` shadows the subclass `write` override (BUG-J family)

**Test:** `org.apache.catalina.servlets.TestWebdavBoundedByteArrayOutputStream`.
**Result:** CratonVM `Tests run: 5, Failures: 2` — `testReset`, `testWriteByteArray`.
**HotSpot: PASS (5/5).**
**Severity:** Medium (WebDAV request-body size bound silently not enforced →
`SC_REQUEST_TOO_LONG` never fires; resource-exhaustion guard defeated).
**Recommendation: FIX (bounded).** This is the **BUG-J "native shadows a subclass
override"** family.

> **STATUS: ✅ FIXED** (branch `fix/tc0622-baos-subclass-write`, commit
> `7fe231dc`, merged to dev). The real culprit was the **one-arg `write([B)V`**
> native — not `write([BII)V`. `native-io/src/lib.rs::native_baos_write_byte_array`
> (registered for `java/io/ByteArrayOutputStream.write([B)V`) delegated straight
> into the backing store, writing the bytes directly. Real
> `java.io.OutputStream.write(byte[])` is `write(b, 0, b.length)` — a **virtual**
> call to `write([BII)V`; `ByteArrayOutputStream` declares no `write(byte[])`, so
> this native stands in for the inherited `OutputStream` bytecode and writing
> directly bypassed the subclass `write([BII)V` bound check. (The two failing
> tests drive `bbaos.write(ONE_BYTE_ARRAY)`, the one-arg form.) Fix: the native
> now performs `ctx.invoke_virtual(this, "write", "([BII)V", [b, 0, b.length])`,
> so a subclass override runs while a plain BAOS / non-overriding subclass
> (`sun.security.util.DerOutputStream`) resolves to the base `write([BII)V`
> native unchanged. Validated: `TestWebdavBoundedByteArrayOutputStream` OK(5)
> JIT+nojit; plain BAOS / DataOutputStream / ECDSA-`DerOutputStream` round-trips
> ==HotSpot; native-io 315 tests green. NOTE: the `write([BII)V` natives were
> NOT the shadow — a subclass overriding `write([BII)V` already dispatches
> correctly via `populate_virtual_invoke_cache`'s receiver-bytecode
> short-circuit; only the inherited one-arg `write([B)V` slipped through.

### Symptom

```
1) testReset(...TestWebdavBoundedByteArrayOutputStream)
   java.lang.AssertionError: Writing 11th byte failed to trigger error
     at ...TestWebdavBoundedByteArrayOutputStream.testReset(...:116)
2) testWriteByteArray(...)
   java.lang.AssertionError: Writing 11th byte failed to trigger error
     at ...TestWebdavBoundedByteArrayOutputStream.testWriteByteArray(...:59)
```

`BoundedByteArrayOutputStream` (limit 10) is expected to throw
`ArrayIndexOutOfBoundsException` on the 11th byte; on CratonVM the overrun is
silently accepted.

### Root cause (analysis)

`WebdavServlet.BoundedByteArrayOutputStream`
(`apps/tomcat/java/org/apache/catalina/servlets/WebdavServlet.java:3106-3139`)
extends `java.io.ByteArrayOutputStream` and overrides **only**:

```java
@Override public synchronized void write(int b)            { size++; if (size > sizeLimit) throw new AIOOBE(); super.write(b); }
@Override public synchronized void write(byte[] b, int off, int len) { size += len; if (size > sizeLimit) throw new AIOOBE(); super.write(b, off, len); }
@Override public synchronized void reset()                 { size = 0; super.reset(); }
```

It does **not** override the one-arg `write(byte[])` — that is inherited from
`java.io.OutputStream.write(byte[])`, whose JDK bytecode is simply
`write(b, 0, b.length)`, i.e. a **virtual** call that must dispatch to the
subclass's overriding `write([BII)V` (the bound check).

The two failing tests (`testWriteByteArray:50`, `testReset:101`) drive the stream
through `bbaos.write(ONE_BYTE_ARRAY)` — the **one-arg** form. The three passing
tests use paths that hit the override directly: `testWriteByte` (`write(int)`),
`testWriteByteSubArray` (`write(b,0,1)`), `testWriteBytes` (`writeBytes`).

CratonVM registers an **unconditional** intrinsic for `java/io/ByteArrayOutputStream`
in `native-builtins/src/serialization.rs::register_byte_array_output_stream`
(wired at `native-builtins/src/lib.rs:12217-12226`), including:

```
register(cls, "write", "(I)V",   ...)   // serialization.rs:4053
register(cls, "write", "([BII)V", ...)  // serialization.rs:4078  <-- the shadow
register(cls, "reset", "()V",    ...)   // serialization.rs:4129
register(cls, "toByteArray", ...) / size / toString / flush / close
```

These natives are declared on the **base** class and write `buf`/`count` directly
(fields 0/1). When `OutputStream.write([B)` executes its `invokevirtual write([BII)V`,
CratonVM resolves the base-class native (the intrinsic) instead of the
`BoundedByteArrayOutputStream` Java override — exactly the **BUG-J** mechanism
(`BUG-J-resourcebundle-native-shadows-subclass.md`: a native registered on the
parent silently wins over a real subclass override). The bound check in the
subclass's `write([BII)V` is therefore bypassed, `size` is never incremented past
the limit, and no `ArrayIndexOutOfBoundsException` is thrown.

Note this is a *correctness inversion* of the comment at `lib.rs:12217-12225`: the
intrinsic was made unconditional precisely so it would "always win over the real
bytecode" for `DerOutputStream` (whose inherited `getfield`/`putfield` slots were
mis-resolved). That same "always win" is what breaks a subclass that legitimately
**overrides** `write`.

### Reproduction

Pure unit test, no server props needed:

```powershell
cd C:\craton\CratonVM\apps\tomcat
$CP = (Get-Content .tooling\cp.txt -Raw).Trim()
C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe -Xmx2g -cp $CP `
  org.junit.runner.JUnitCore org.apache.catalina.servlets.TestWebdavBoundedByteArrayOutputStream
```

Expected on CratonVM: `Tests run: 5, Failures: 2` (`testReset`, `testWriteByteArray`).
Minimal standalone repro:

```java
class B extends java.io.ByteArrayOutputStream {
    int n; @Override public void write(byte[] b,int o,int l){ n+=l; if(n>3) throw new ArrayIndexOutOfBoundsException(); super.write(b,o,l);} }
B b = new B();
b.write(new byte[]{1,1,1});           // ok
try { b.write(new byte[]{1}); System.out.println("BUG: no throw"); }
catch (ArrayIndexOutOfBoundsException e) { System.out.println("OK"); }
```

HotSpot prints `OK`; CratonVM prints `BUG: no throw`.

### Recommendation — FIX (bounded, BUG-J family)

Gate the `ByteArrayOutputStream` intrinsic natives (`write(I)V`,
`write([BII)V`, `reset()V`, and ideally `toByteArray`/`size`/`toString`) on a
**non-subclass receiver**: only take the native fast path when
`class_id_of(this) == java/io/ByteArrayOutputStream` (the synthetic/base shape the
intrinsic was written for — `DerOutputStream` is the case the comment at
`lib.rs:12217` cites, but that subclass does *not* override `write`, so it can be
handled by checking "no overriding `write` in the receiver's vtable" rather than
the bare class id). For a receiver whose dynamic class overrides `write([BII)V`,
fall through to the real bytecode so the subclass bound check runs. This mirrors
the BUG-J fix recipe (detect non-base receiver → run the subclass's real method).
Verify no regression on the `DerOutputStream`/ECDSA-DER path that motivated the
unconditional registration, and re-run
`TestWebdavBoundedByteArrayOutputStream` (expect 5/5) and the serialization /
DER suites.

---

## Cross-check vs existing BUG-*.md

* **BUG-J** (`BUG-J-resourcebundle-native-shadows-subclass.md`) — same family as
  Gap B (parent-class native shadows a real subclass override; fix = gate the
  native on a non-synthetic/overriding receiver and run the subclass method).
  Gap B is a fresh instance on `ByteArrayOutputStream.write`.
* **BUG-DF05** (`BUG-DF05-regex-pattern-cast-object-to-byte-array.md`) — *not*
  related to Gap A despite both touching "regex": DF05 was a `new
  String(StringBuilder)` char[]/byte[] mismatch surfacing through the Xerces
  regex engine. Gap A's regex `IllegalArgumentException` is a confirmed red
  herring (HotSpot-identical) and the real cause is loopback-peer CIDR matching,
  not `Pattern`.
