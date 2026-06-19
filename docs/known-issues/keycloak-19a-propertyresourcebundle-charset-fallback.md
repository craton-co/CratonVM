# Bug #19A — `PropertyResourceBundle(InputStream)` UTF-8→ISO-8859-1 fallback

**Status:** ✅ **FIXED** on branch `fix/prop-resource-bundle-charset-fallback`.
**Class:** `org.keycloak.theme.PropertiesUtilTest.testEncodingIso` (was 1/3 failing).
**Surfaced by:** the 2026-06-18 rerun handoff "#19 — Residual CV-only bugs (charset / StAX /
placeholder)". Siblings **B** (synthetic StAX namespace/prefix) and **C** (SAML `${sysprop}`)
are **also FIXED on this branch** — see the bottom of this doc.

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

## Sibling B — synthetic StAX drops attribute prefixes and inline namespace decls ✅ FIXED

**Class:** `SAMLAttributeValueParserTest.parsesAttributeValueUserTypeWithAttributeAndInnerNamespace`
(was 1/8). The parser re-serializes a nested element subtree through an `XMLEventWriter`.

**Root cause(s)** in `native-builtins/src/xml_stax.rs`:
1. The synthetic reader built every **attribute** `QName` with `prefix=""` while setting a
   non-empty namespace URI. The JDK `XMLEventWriterImpl.writeAttribute` rejects that pair
   (`XMLStreamException: prefix cannot be null or empty`) for a prefixed inner-namespace
   attribute (`myCustomType3:restriction`). `StaxAttr` never stored the prefix.
2. The reader stubbed `getNamespaceCount()` to `0` and reported `isNamespaceAware=false`, so the
   `XMLEventAllocatorImpl` never ran `fillNamespaceAttributes` — a START_ELEMENT's own
   `xmlns(:p)="…"` declarations were dropped, so a re-serialized element with an **inline default
   namespace** (`<City xmlns="…">`) lost it.

**Fix:** store each attribute's prefix (`StaxAttr.prefix`) and use it in `getAttributeName(i)`;
collect each element's literal `xmlns`/`xmlns:p` decls (`StaxEvent.namespaces`) and expose them via
`getNamespaceCount()`/`getNamespaceURI(i)`/`getNamespacePrefix(i)`; report `isNamespaceAware=true`
and return a real `com.sun.org.apache.xerces.internal.util.NamespaceContextWrapper` from
`getNamespaceContext()` so the allocator's `setNamespaceContext` cast succeeds and the
namespace-aware event path runs. **Verified:** SAMLAttributeValueParserTest 8/0; re-serialized
string byte-identical to HotSpot; the keycloak SAML parser-test surface (the 7 other
`SAML*ParserTest` classes incl. `SAMLParserTest`) shows the **same 5 pre-existing failures** as
`dev` — the `isNamespaceAware` flip adds no regressions.

## Sibling C — `System.clearProperty` left `""` instead of removing the key ✅ FIXED

**Class:** `KeycloakSamlAdapterXMLParserTest.testXmlParserSystemPropertiesNoPropertiesSet`
(was 1/18). The report's "placeholder-replacement" diagnosis was **wrong** — the actual first
failure is `getEntityID()==""` (not `getSslPolicy`), and `StringPropertyReplacer` + the
`SystemEnvProperties.UNFILTERED` resolver + the synthetic StAX attribute reads all work in
isolation on CV.

**Root cause:** the sibling test `testXmlParserSystemPropertiesWithPropertiesSet` sets these system
properties and `System.clearProperty`s them in `finally`. CratonVM's `clearProperty` native
(`native-builtins/src/lib.rs`) was a documented "soft-clear" — `set_system_property(key, "")` —
that left the key present with an empty value instead of removing it. So after the sibling test,
`System.getProperty("keycloak-saml-properties.entityID")` returned `""` (not `null`), the resolver
returned `""`, and the `${name:sp}` default / `${name}` literal were never reached →
`getEntityID()==""`. (`System.getProperties().remove(...)` already worked; only `clearProperty`
was wrong.)

**Fix:** `clearProperty` now calls the existing `remove_system_property` trait method (returning
the prior value, matching `Hashtable.remove`). **Verified:** `System.clearProperty` then
`getProperty` returns `null`; KeycloakSamlAdapterXMLParserTest 18/0. This is a general fix —
any app relying on `clearProperty` actually unsetting a property was affected.
