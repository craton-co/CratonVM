# Bug #19A — `PropertyResourceBundle(InputStream)` UTF-8→ISO-8859-1 fallback

**Status:** ✅ **FIXED** on branch `fix/prop-resource-bundle-charset-fallback`.
**Class:** `org.keycloak.theme.PropertiesUtilTest.testEncodingIso` (was 1/3 failing).
**Surfaced by:** the 2026-06-18 rerun handoff "#19 — Residual CV-only bugs (charset / StAX /
placeholder)". Siblings **B** (synthetic StAX null attribute prefix) and **C** (SAML `${sysprop}`
placeholder left-literal) remain **OPEN** — see the bottom of this doc.

## Symptom

`new PropertyResourceBundle(new ByteArrayInputStream("key=Umlaut: äöü".getBytes(ISO_8859_1)))
.getString("key")` returned `"Umlaut: ???"` (the umlauts became `0x3F`), where HotSpot returns
`"Umlaut: äöü"`. `Properties.load(InputStream)` (ISO-8859-1) and `InputStreamReader(stream,
ISO_8859_1)` both already worked on CV — only the `PropertyResourceBundle` decode path was wrong.

## Root cause

`PropertyResourceBundle(InputStream)` (JDK 9+) builds its reader as
`new InputStreamReader(stream, new sun.util.PropertyResourceBundleCharset(strictUTF8).newDecoder())`.
That decoder (`PropertiesFileDecoder`) decodes UTF-8 but, on the **first** malformed/unmappable
byte, resets and decodes the entire buffer as **ISO-8859-1**, sticking with ISO-8859-1 for the
rest of the stream (unless `strictUTF8`, which is only set when the system property
`java.util.PropertyResourceBundle.encoding` is `UTF-8`).

CratonVM runs `InputStreamReader` as real bytecode, whose constructor calls
`sun.nio.cs.StreamDecoder.forInputStreamReader(in, this, decoder)`. CratonVM intercepts that
factory (`native-io/src/stream_decoder.rs`) and replaces the whole `StreamDecoder` with a native
shim that resolves only a charset **name** and decodes with its own engine — so the real
`PropertiesFileDecoder.decodeLoop` (and its UTF-8→ISO-8859-1 fallback) **never ran**. The
`PropertyResourceBundleCharset` resolved to no usable name → defaulted to UTF-8 → the ISO-8859-1
umlaut bytes (`E4 F6 FC`, invalid UTF-8) were lossily replaced.

## Fix

In `native-io/src/stream_decoder.rs`, detect a `sun.util.PropertyResourceBundleCharset` charset
(or its inner `PropertiesFileDecoder`) at `forInputStreamReader` time and attach a `PropState`
to the decoder's side-table entry. The streaming decode (`decode_into`) then routes that decoder
through a new `decode_prop` that mirrors `PropertiesFileDecoder.decodeLoop`:

- try UTF-8 on `(carried tail + freshly read)` bytes;
- a **truncated** trailing multi-byte sequence mid-stream is carried to the next refill (not an
  error yet);
- a **malformed/unmappable** byte (or a truncated tail at EOF) switches the whole current buffer
  to ISO-8859-1 and marks the decoder sticky-ISO for the rest of the stream;
- `strictUTF8` (read from the charset's `strictUTF8` field, default non-strict) suppresses the
  fallback.

Non-`PropertyResourceBundleCharset` decoders are unchanged (the existing prefix-split path).

## Verification

- New Rust unit tests in `stream_decoder.rs` (`prop_decoder_*`): ISO fallback, valid-UTF-8 kept,
  truncated-tail carry, truncated-at-EOF fallback, sticky-ISO, strict-no-fallback. Full
  `native-io` suite: **295 passed, 0 failed**.
- Standalone repro (`PropertyResourceBundle` over ISO-8859-1 / UTF-8 / ASCII / a 500-line
  streaming payload): fixed CV code units are **byte-identical to HotSpot jdk-25** (`RESULT=PASS`);
  the unfixed `dev` binary printed `Umlaut: ???` (`RESULT=FAIL`).
- Real test via the keycloak universal-cp runner:
  `KcRunner org.keycloak.theme.PropertiesUtilTest` → **`tests=3 failed=0`** (unfixed `dev`:
  `failed=1`, `testEncodingIso` `AssertionError`).

This also corrects the common `ResourceBundle.getBundle(...)` path that loads `.properties`
message bundles through `PropertyResourceBundle(InputStream)`.

## Repro

```
cratonvm.exe --java-home "C:\Program Files\Java\jdk-25" \
  -cp "<kc>/kc-runner;$(cat <kc>/kc-universal-cp.txt)" \
  KcRunner org.keycloak.theme.PropertiesUtilTest
```

## Open siblings (NOT addressed here)

- **B — synthetic StAX null attribute prefix**:
  `SAMLAttributeValueParserTest.parsesAttributeValueUserTypeWithAttributeAndInnerNamespace`.
  `native-builtins/src/xml_stax.rs` builds the attribute `QName` with a null prefix for a
  prefixed inner-namespace attribute → `XMLStreamException: prefix cannot be null or empty`.
- **C — SAML `${sysprop}` placeholder not left literal when unset**:
  `KeycloakSamlAdapterXMLParserTest.testXmlParserSystemPropertiesNoPropertiesSet`. keycloak's
  `StaxParserUtil`/`StringPropertyReplacer` `${name}` replacement resolves/strips an unset,
  default-less placeholder instead of leaving it literal.
