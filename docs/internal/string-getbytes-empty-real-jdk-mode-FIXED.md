# `String.getBytes()` (all overloads) returns an empty byte array in real-JDK mode — severe, confirmed present at `dev` HEAD before this investigation's own fix

Status: open — **severe**, broad blast radius suspected; root cause narrowed to a likely mechanism, not yet fixed

Date observed: 2026-07-14, while verifying [`keycloak-testclassserver-invalidpackage-classnotfound-FIXED.md`](keycloak-testclassserver-invalidpackage-classnotfound-FIXED.md) (now moved to `docs/internal/` as FIXED) on the Azure Linux build host.

## Summary

`"anything".getBytes()` — and every other `String.getBytes(...)` overload tried
(`getBytes(Charset)`, `getBytes(String charsetName)`) — returns a **zero-length**
byte array under CratonVM real-JDK mode (`--java-home <jdk25>`), on both:

- The unmodified `dev` HEAD binary (commit `6addc1e0`), confirmed via a clean
  worktree build with no local changes.
- A binary built from a feature branch with an unrelated classloader fix
  applied (confirmed NOT the cause — the bug reproduces identically on the
  branch point before that fix).

So this is a **pre-existing** regression, not introduced by any change in this
investigation. `String.length()` and other non-encoding String operations
(concatenation, arithmetic, `ArrayList` operations tested alongside it) are
unaffected — this is specific to byte-encoding.

```java
System.out.println("hello".getBytes().length);                                    // prints 0, expect 5
System.out.println("hello".getBytes(java.nio.charset.StandardCharsets.UTF_8).length); // prints 0, expect 5
System.out.println("hello".getBytes("UTF-8").length);                             // prints 0, expect 5
System.out.println(java.nio.charset.Charset.defaultCharset());                    // prints "UTF-8" correctly
```

## Why this matters (discovered via a downstream symptom)

This was found while diagnosing why a `com.sun.net.httpserver.HttpServer`
handler always answered `200 OK` / `Content-Length: 0` regardless of what the
handler wrote to the response body. Initial investigation suspected the
handler wasn't being dispatched at all (it is — confirmed via `System.err`
prints as the first statement in `handle()`, which DO appear). Deeper
instrumentation traced it to the handler's own `"...".getBytes()` call
producing a zero-length array, which then made
`sendResponseHeaders(200, bytes.length)` correctly, faithfully report a
`Content-Length: 0` body. **The HTTP server plumbing itself is not the bug —
`String.getBytes()` is**, and it silently breaks anything built on top of it:
HTTP response/request bodies, hashing, most I/O, `String(byte[])`
round-trips, etc. This is very likely under-detected because most test
suites' failures land on `com.sun.net.httpserver`/serialization/hashing
symptoms that get diagnosed (or misdiagnosed) at that higher layer rather
than traced back to `getBytes()` itself.

## Root cause (hypothesis, not yet confirmed by a live A/B bisect)

`native-builtins/src/lib.rs` registers a Rust native for the no-arg overload:

```rust
registry.register(
    "java/lang/String",
    "getBytes",
    "()[B",
    native_string_get_bytes,
);
```

(`native_string_get_bytes` itself, in `native-builtins/src/lang_string.rs`, is
simple and correct — reads the string, UTF-8-encodes it, builds a byte array.
It is not the source of the bug if it actually runs.)

Neither `getBytes(Charset)` nor `getBytes(String)` appear to have their own
native registration at all near this call site, meaning those overloads run
**real JDK bytecode** (`StringCoding`/`Charset` encoder machinery), which is
consistent with them failing identically to the no-arg form if that
machinery is what's actually broken.

Prime suspect: commit `d8092acbaa9c13575f9fa1a0f37752a292177bf1`
(`fix-tests-real-jdk-contracts`, landed the same day, ~90 minutes before this
investigation started) introduced `NativeKind`-based stub-dropping for
real-JDK mode — `native_methods.set_drop_synthetic_stubs(true)`
(`vm/src/vm/vm_init.rs:1337,1731`), intentionally making "real-JDK mode must
not register synthetic overrides" (see that commit's own new test in
`vm_init.rs`). The registry's default `NativeKind` for a registration that
doesn't explicitly opt into `Bridge`/`Intrinsic` is `SyntheticStub`
(`native-api/src/registry.rs`, per code inspected during this session). If
`native_string_get_bytes`'s registration above is one of the (likely many)
registrations across the ~12,000-entry native surface that was never
explicitly re-categorized away from the default `SyntheticStub`, this one
commit would have silently dropped it for real-JDK mode — falling through to
real `String.getBytes()` bytecode, which itself may depend on JDK
charset-provider/encoder internals CratonVM does not fully support, and which
apparently fails silently (empty array) rather than throwing.

This would make `d8092acb` responsible for (at least) TWO independent,
currently-open severe regressions discovered the same day:

1. The already-documented `InternalError: null property: java.home` /
   `System.getProperties()`-returns-empty-singleton bug (see this repo's most
   recent `docs(known-issues)` commit message on `dev` HEAD, `6addc1e0`, item 3).
2. This `getBytes()`-empty bug.

Both may or may not share a root mechanism (both are "real JDK bytecode path
now reachable in real-JDK mode where it wasn't reachable before, and that
path is broken in some environment-dependent way") — that shared shape is
suggestive but NOT confirmed as literally the same code path. Do not assume
fixing one fixes the other without re-verifying.

## Recommended next step

Do NOT attempt to patch `native_string_get_bytes`'s own logic — it looks
correct and may not even be the code path that's executing. Instead:

1. Confirm with `--dump-native-registry` (or equivalent) whether
   `java/lang/String.getBytes()[B]`'s `NativeKind` is `SyntheticStub` (would
   confirm it's being dropped) vs `Bridge`/`Intrinsic` (would mean the native
   IS running, and the bug is actually inside `native_string_get_bytes` or
   `ctx.read_string`/`ctx.new_array`/`ctx.set_array_element` themselves in
   this specific build — a different, more surprising finding).
2. If dropped: `git diff d8092acb^..d8092acb -- native-builtins/src/lib.rs`
   and specifically check whether this `getBytes` registration site (and
   siblings — `String` has many similarly-shaped native registrations
   nearby) sits inside a `with_category(Bridge, ...)`/`set_category` block
   the way `register_essential_natives`'s outer wrapper does for Phase E
   (`net_phase_e.rs`) — if not, that's the fix: wrap it (and any other
   incorrectly-defaulted-to-`SyntheticStub` genuine VM bridges) in the
   correct category, OR give `d8092acb`'s new hardening pass an explicit
   allowlist/audit pass across the whole registry before trusting it broadly.
3. Given the severity (this plausibly breaks most String-to-bytes-dependent
   real-world code — I/O, hashing, HTTP bodies, serialization), treat this as
   higher priority than typical "known issues" triage; it may be silently
   causing many other suites' failures to be misdiagnosed at a higher layer.

## Repro

```java
public class GetBytesRepro {
    public static void main(String[] a) {
        System.out.println("getBytes()=" + "hello".getBytes().length);
        System.out.println("getBytes(UTF_8)=" + "hello".getBytes(java.nio.charset.StandardCharsets.UTF_8).length);
        System.out.println("getBytes(\"UTF-8\")=" + "hello".getBytes("UTF-8").length);
    }
}
```
```
cratonvm --java-home <jdk25> -c . GetBytesRepro
# expected (HotSpot): 5 / 5 / 5
# actual (CratonVM, dev HEAD 6addc1e0): 0 / 0 / 0
```

## Evidence

Verified interactively on the Azure Linux build host (`victor@20.83.144.174`,
`/data/data/cratonvm` main worktree at `6addc1e0`, and worktree
`/data/wt-testclassserver-cnfe-20260714` branch
`fix/testclassserver-invalidpackage-cnfe-20260714`) against both a binary
built from unmodified `dev` HEAD (`cratonvm-baseline-20260714`) and one with
this session's unrelated classloader fix applied
(`cratonvm-urlclfix-20260714`) — identical `0 / 0 / 0` result on both,
confirming the bug is pre-existing and branch-independent.

---

## FIXED 2026-07-14

**Root cause confirmed**: identical failure pattern to the same-day
`java.util.Properties` regression (commit `f62d2073`, "pin java.util.Properties
side-table bridges to `NativeKind::Bridge`"). `register_real_charset_natives`
(`native-builtins/src/charset.rs:920`) — which registers all three modern
`String.getBytes` overloads (`()[B`, `(Charset)[B`, `(String)[B`) plus the
`CharsetEncoder`/`CharsetDecoder` bridge natives — never set its own registry
category. Both of its real-JDK-mode call sites (`vm/src/vm/vm_init.rs:1556`,
`:2125`) sit outside any `Bridge`/`Intrinsic` scope, so its registrations
inherited the registry's default category, `SyntheticStub`. Commit `d8092acb`
("fix-tests-real-jdk-contracts") made real-JDK mode silently DROP every
`SyntheticStub` registration (`set_drop_synthetic_stubs(true)`), which is
correct for genuine approximations but wrongly caught these — the file's own
comments (lines 984-997) already documented that real `String.getBytes()`
bytecode's `CharsetEncoder.encode` path reads zero chars against CratonVM's
synthetic `Buffer` field-layout overlay, which is exactly what these natives
exist to work around. With the natives dropped, every `getBytes()` call
silently fell through to that broken real-bytecode path and returned an empty
array — confirmed via the `CRATONVM_DBG_DROPPED_STUBS=1` diagnostic (added by
the Properties fix) listing `java/lang/String.getBytes()[B]`,
`.getBytes(Ljava/nio/charset/Charset;)[B`, and `.getBytes(Ljava/lang/String;)[B`
all as dropped stubs.

**Fix**: wrapped `register_real_charset_natives`'s entire body in
`registry.with_category(NativeKind::Bridge, |registry| { ... })`, mirroring
`f62d2073`'s fix to `register_properties_sidetable` — the same class of bug,
same fix shape, different function.

**Verified**: `"hello".getBytes()` / `.getBytes(UTF_8)` / `.getBytes("UTF-8")`
all correctly return 5 (was 0 on both an unmodified-dev-HEAD baseline and a
binary built immediately before this fix). Confirmed this was indeed the true
root cause of the `com.sun.net.httpserver.HttpServer` "always `Content-Length:
0`" symptom noted in
[`keycloak-testclassserver-invalidpackage-classnotfound-FIXED.md`](keycloak-testclassserver-invalidpackage-classnotfound-FIXED.md)'s
residual note — a live server/curl round trip now correctly returns real
bytes with a matching `Content-Length`. Regression-checked `new
String(bytes, charset)` (byte→String, the inverse direction),
`Charset.encode(String)`/`Charset.decode(ByteBuffer)` (the
`CharsetEncoder`/`CharsetDecoder` bridges this same fix also restores) — all
correct.

**Bonus finding**: this session also confirmed (not this session's own fix —
already landed via `f62d2073`, which the Properties bug's own chain runs
through) that
[`locale-real-jdk-bootstrap-noclassdeffounderror-FIXED.md`](locale-real-jdk-bootstrap-noclassdeffounderror-FIXED.md)
is ALSO fixed — `Locale.getDefault()`/`Locale.US` now both correctly print
`en_US` instead of throwing. Moved to `docs/internal/` alongside this doc.

**Residual — NOT fixed, new finding**: `com.sun.net.httpserver.HttpExchange
.getRequestURI()` returns a `URI` object whose `toString()`/`getPath()` are
empty strings, even though a directly-constructed `new URI(...)` works
correctly — see
[`httpserver-exchange-requesturi-getpath-empty.md`](../known-issues/httpserver-exchange-requesturi-getpath-empty.md).
This still blocks the literal upstream Keycloak `TestClassServerTest` end to
end, since `TestClassServer`'s handler routes on
`httpExchange.getRequestURI().getPath()`.
