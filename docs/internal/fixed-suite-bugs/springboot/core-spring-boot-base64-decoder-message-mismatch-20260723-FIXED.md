# Base64 decode error message text doesn't match real JDK ("Invalid base64 char" vs "Illegal base64") — FIXED

**Status: FIXED — filed 2026-07-23, closed 2026-07-28**

## Symptom (as filed)

```
java.lang.AssertionError:
Expecting throwable message:
  "Invalid base64 char:  "
to contain:
  "Illegal base64"
but did not.

Throwable that failed the check:

java.lang.IllegalArgumentException: Invalid base64 char:
	at org.springframework.boot.io.Base64ProtocolResolver.decode(Base64ProtocolResolver.java:47)
	at org.springframework.boot.io.Base64ProtocolResolver.resolve(Base64ProtocolResolver.java:41)
```

`Base64ProtocolResolverTests.base64LocationWithInvalidBase64ThrowsException()`
and `JksSslStoreBundleTests.invalidBase64EncodedLocationThrowsException()` both
assert that a malformed base64 payload throws `IllegalArgumentException` with a
message containing `"Illegal base64"` (the real `java.util.Base64.Decoder`'s
wording) — CratonVM's decoder threw different message text.

## Resolution

### 1. The filed symptom — already fixed before this session

The literal `"Invalid base64 char: <c>"` string was replaced by
`b64_illegal_char_msg` (`native-builtins/src/lib.rs`) as part of two
independent earlier clusters, both already merged into `dev`:

- `spring-boot-core39-clusterb-diagnostics-process-base64-json-FIXED.md`
- `core39-clusterD-lifecycle-ssl-validation-FIXED.md` (item 4)

Confirmed by re-running both affected classes on an unmodified `origin/dev`
build (`ccf774db3`): **`Base64ProtocolResolverTests` 3/3 and
`JksSslStoreBundleTests` 14/14 pass, JIT and `--nojit`.** The doc was stale,
not wrong. `JksSslStoreBundleTests`'s two other failures (the sibling doc
`core-spring-boot-keystore-provider-name-swallowed-20260723.md`) are green in
the same runs.

### 2. The residuals — fixed here

Getting one message right left the rest of `b64_decode` diverging from the
real decoder. A 94-case probe
(`docs/internal/fixed-suite-bugs/repros/base64-decoder-parity/Base64Probe.java`,
reference output from HotSpot 25 alongside it) found **122 diff lines** against
real Java on the pre-fix `dev` binary. Root cause: the decoder was a
hand-rolled quad-at-a-time loop rather than a port of the JDK's shift-register
`decode0`, so each malformed shape got whatever ad-hoc message that loop
happened to produce — and MIME was implemented as "strip whitespace" rather
than RFC 2045's "ignore every non-alphabet byte".

`b64_decode` is now a line-for-line port of real
`java.util.Base64.Decoder.decodedOutLength` (its two length guards) plus
`decode0` (`native-builtins/src/lib.rs`). Fixed divergences:

| Case | Real JDK 25 | CratonVM before |
| --- | --- | --- |
| `getMimeDecoder().decode("AB@@CD")` | ignores non-alphabet bytes → `001083` | `IllegalArgumentException` |
| `decode("A")` | `Input byte[] should at least have 2 bytes for base64 bytes` | `Incomplete base64 input` |
| `getMimeDecoder().decode("A")` | `Last unit does not have enough valid bits` | `Incomplete base64 input` |
| `decode("AB=")`, `"AB=C"`, `"=AAA"`, `"ABCD="`, `"ABCD=="` | `Input byte array has wrong 4-byte ending unit` | 3 different ad-hoc messages |
| `decode("ABCDE")`, `decode("A=AA")` | `Last unit does not have enough valid bits` | `Incomplete base64 input` / `Illegal base64 character 3d` |
| `decode("AB==CD")` | `Input byte array has incorrect ending byte at 4` (MIME: `at 5`) | silently returned 1 byte, no throw |
| `decode(byte[]{'A','B',0xff,'D'})` | `Illegal base64 character -1` | `Illegal base64 character ff` |
| `decode("AB€D")` | `Illegal base64 character 3f` | `Illegal base64 character e2` |
| `getMimeEncoder().encodeToString(57 bytes)` | 76 chars, no trailing CRLF | 78 chars, phantom trailing CRLF |

Three details worth keeping in mind, all verified against HotSpot rather than
reasoned about:

1. **The illegal-character hex is signed.** Real JDK passes `src[sp - 1]` — a
   `byte` — to `Integer.toString(int, 16)`, so `0xff` prints as `-1` and
   `0xc3` as `-3d`, not `ff`/`c3`.
2. **`decode(String)` is `decode(s.getBytes(ISO_8859_1))`**, one byte per
   UTF-16 unit with anything above `U+00FF` replaced by `'?'`. Feeding it
   UTF-8 made a non-ASCII char report its first continuation byte instead of
   `3f`.
3. **The "trailing bytes after the padding" index differs between BASIC and
   MIME by one.** Real JDK's `if (isMIME && base64[src[sp++] & 0xff] < 0)`
   short-circuits, so `sp++` only runs in MIME mode — hence `at 4` vs `at 5`
   for the same input.

The MIME encoder's line separator is now emitted lazily (before whatever
follows) rather than eagerly after the quad that fills a line, matching real
`encode0`'s `if (dp < len && sp < end)` guard.

## Verification

Fresh release binary `cratonvm-base64msg-fixed.exe`, branch
`fix/base64-decoder-msg-closure-20260727` off `origin/dev` `ccf774db3`.

**Probe:** `Base64Probe`'s 94 cases are now **byte-identical to HotSpot 25**
under both JIT and `--nojit` (pre-fix `dev`: 122 diff lines).

**Spring Boot `core/spring-boot`**, `run-single-class.ps1`:

| Class | before (dev) JIT | after JIT | after `--nojit` |
| --- | --- | --- | --- |
| `Base64ProtocolResolverTests` | 3/3 | 3/3 | 3/3 |
| `JksSslStoreBundleTests` | 14/14 | 14/14 | 14/14 |
| `AppendableByteArrayTests` | 4/4 | 4/4 | 4/4 |
| `PemContentTests` | 16/16 | 16/16 | 16/16 |
| `PemPrivateKeyParserTests` | 47/47 | 47/47 | 47/47 |
| `PemSslStoreBundleTests` | 11/11 | 11/11 | 11/11 |
| `PemCertificateParserTests` | 2/2 | 2/2 | — |
| `LoadedPemSslStoreTests` | 5/5 | 5/5 | — |
| `AliasKeyManagerFactoryTests` | 1/1 | 1/1 | — |
| `DefaultSslBundleRegistryTests` | 12/12 | 12/12 | — |
| `DefaultSslManagerBundleTests` | 10/10 | 10/10 | — |
| `FixedTrustManagerFactoryTests` | 1/1 | 1/1 | — |
| `NoSuchSslBundleExceptionTests` | 1/1 | 1/1 | — |
| `SslBundleKeyTests` | 5/5 | 5/5 | — |
| `SslBundleTests` | 3/3 | 3/3 | — |
| `SslManagerBundleTests` | 9/9 | 9/9 | — |
| `SslOptionsTests` | 8/8 | 8/8 | — |
| `SslStoreBundleTests` | 2/2 | 2/2 | — |
| `PemSslStoreTests` | 2/5 | 2/5 | — |

`PemSslStoreTests`'s 3 failures are **unchanged before and after** and
unrelated: `MockitoException: Mockito cannot mock this class:
java.security.cert.X509Certificate` — a pre-existing CratonVM/Mockito gap, not
a base64 defect.

**Re-verified after merging `origin/dev` forward** (17 commits, including JIT
and interpreter changes, up to `2cdc451fb`): rebuilt as
`cratonvm-base64msg-postmerge.exe`; probe still byte-identical to HotSpot in
both modes; `Base64ProtocolResolverTests`, `JksSslStoreBundleTests`,
`PemContentTests`, `PemPrivateKeyParserTests`, `PemSslStoreBundleTests`,
`PemCertificateParserTests`, `LoadedPemSslStoreTests`,
`AppendableByteArrayTests` all green, `PemSslStoreTests` still exactly its 3
pre-existing Mockito failures. The diff against `origin/dev` after the merge
touches exactly the 8 files this fix owns — no silent merge loss.

Rust unit tests added alongside the code (`base64_tests` in
`native-builtins/src/lib.rs`), each expectation copied from the HotSpot probe
output rather than derived: `decode_error_messages_match_real_jdk_wording`,
`illegal_char_message_uses_the_signed_byte_in_hex`,
`mime_decoder_ignores_every_non_alphabet_byte_not_just_whitespace`,
`decode_accepts_the_well_formed_shapes`,
`mime_encoder_omits_the_trailing_line_separator`,
`mime_round_trip_survives_the_line_separators`.

## Reproducing

```
javac -d <out> docs/internal/fixed-suite-bugs/repros/base64-decoder-parity/Base64Probe.java
java -cp <out> Base64Probe            > hotspot.txt
cratonvm.exe --java-home <jdk25> -cp <out> Base64Probe > craton.txt
diff hotspot.txt craton.txt           # must be empty
```

`hotspot-jdk25-reference.txt` in the same directory is the captured HotSpot 25
output, so the diff can be run without a real JDK on hand.

## Affected classes

| Module | Class | Result |
|---|---|---|
| core/spring-boot | org.springframework.boot.io.Base64ProtocolResolverTests | PASS |
| core/spring-boot | org.springframework.boot.ssl.jks.JksSslStoreBundleTests | PASS |
