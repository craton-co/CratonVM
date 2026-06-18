# Fix: charset-unsupported — surface unsupported-charset NAME errors

Report: `docs/reviews/fable-2026-06-10/native-api.md` (V2 — charset lossy
fallbacks silently substitute on unsupported charset name).
Carry-over from Round 1 (`native-api-charset`).

## Finding

Round 1 made `native-api/src/charset.rs` correct for *supported* charsets, but
the byte-sink callers in `native-builtins` / `native-io` still used the LOSSY,
infallible `decode_bytes_lossy` / `encode_chars_lossy` API even on the code
paths that take a **user-supplied charset NAME**. So an UNKNOWN or
unsupported charset name silently fell back instead of surfacing the JVM error:

- `String.getBytes(String charsetName)` with a name that *normalizes* to a
  canonical charset the engine does not implement (e.g. `"Shift_JIS"`,
  `"EUC-JP"`, `"GBK"`) slipped past the empty-name guard into
  `encode_chars_lossy`, which silently produced ISO-8859-1 (Latin-1) bytes for
  a charset the platform claims not to support. HotSpot throws
  `UnsupportedEncodingException`.
- `sun.nio.cs.StreamDecoder.forInputStreamReader(in, lock, String)` and
  `StreamEncoder.forOutputStreamWriter(out, lock, String)` (the public
  factories behind `InputStreamReader`/`OutputStreamWriter` constructed with a
  charset *name*) mapped every unknown/unsupported name to `"UTF-8"` via their
  local `normalize()` and then transcoded the whole stream as UTF-8. Both JDK
  factories declare `throws UnsupportedEncodingException`.

## Root cause

Two distinct "unsupported" cases were both masked:
1. A name with no canonical mapping (`normalize_charset_name` → empty, or the
   local native-io `normalize` → its `_ => "UTF-8"` arm).
2. A name that canonicalizes cleanly but the transcoding *engine* lacks a coder
   for — `engine::decode_bytes`/`encode_chars` return
   `CodingErrorKind::UnsupportedCharset`, but the callers only ever invoked the
   `*_lossy` variants, which can never report that.

The fix routes the NAME paths through the FALLIBLE engine API (or a cheap
empty-slice probe of it) and throws on `UnsupportedCharset`, while keeping the
lossy/replacement path for genuinely-malformed *input* when the charset IS
supported, and keeping the UTF-8 fallback on the already-validated
Charset-*object* paths (reached only after `Charset.forName`/`isSupported`).

The engine probe uses an **empty input slice**: the engine's charset-name
`match` returns `UnsupportedCharset` before examining any byte/char, so the
check does zero transcoding work. (decode/encode support are name-symmetric in
the engine, so probing `decode_bytes` gates the encode path too.)

## Exact change

`native-builtins/src/charset.rs`
- New `pub(crate) fn engine_supports(canon: &str) -> bool` — empty-slice probe
  of `engine::decode_bytes` for `CodingErrorKind::UnsupportedCharset`.
- `native_string_get_bytes_named` (`String.getBytes(String)`): the guard
  `if norm.is_empty()` → `if norm.is_empty() || !engine_supports(&norm)`, so a
  real-but-unimplemented canonical name now throws the same checked
  `UnsupportedEncodingException` (encoded as `RuntimeError::IOException` with a
  `"UnsupportedEncodingException: …"` message, the pre-existing convention in
  this method) instead of producing lossy Latin-1.
- The `decode_with_charset`/`encode_with_charset` (Charset-*object*) and the
  ctx-less `encode_str_named`/`decode_str_named` helpers are intentionally left
  on the lossy path (object path is post-`forName`-validated; the ctx-less
  helpers cannot throw).

`native-io/src/stream_decoder.rs`
- Replaced the infallible local `cratonvm_native_builtins_normalize` with
  `normalize_supported(name) -> Option<String>` that returns `None` for an
  unknown alias OR an engine-unimplemented canonical name (empty-slice
  `engine::decode_bytes` probe). `normalize()` now delegates
  `normalize_supported(n).unwrap_or("UTF-8")`, preserving the
  Charset-object/`resolve_name` UTF-8 fallback unchanged.
- New `throw_unsupported_encoding(ctx, name)` — constructs the real
  `java/io/UnsupportedEncodingException(String)` via `new_object_initialized`
  and returns `MethodCallFailed::ExceptionThrown`, with a generic `IOException`
  (its superclass) fallback if the class can't be constructed (mirrors the
  `throw_jca` pattern in `jca/key_factory.rs`).
- `native_sd_for_isr_name` (`forInputStreamReader(…,String)`) now throws via
  that helper on an unsupported name instead of falling back to UTF-8. The
  null/missing-charset-arg path still defaults to UTF-8 (unchanged leniency).

`native-io/src/stream_encoder.rs`
- Same `normalize_supported` (probing `engine::encode_chars`) +
  `throw_unsupported_encoding` helpers; `native_se_for_osw_name`
  (`forOutputStreamWriter(…,String)`) now throws on an unsupported name.

## Files touched
- `native-builtins/src/charset.rs`
- `native-io/src/stream_decoder.rs`
- `native-io/src/stream_encoder.rs`

## Tests added
Pure-function unit tests (no ctx / no I/O needed — confident they compile):
- `native-builtins/src/charset.rs`: `engine_supports_known_canonical_names`,
  `engine_supports_rejects_unimplemented_canonical_name` (asserts
  `normalize_charset_name("Shift_JIS") == "Shift_JIS"` yet
  `!engine_supports("Shift_JIS")`).
- `native-io/src/stream_decoder.rs`: `normalize_supported_accepts_known_aliases`,
  `normalize_supported_rejects_unknown_name`,
  `normalize_supported_rejects_real_but_unimplemented_charsets`.
- `native-io/src/stream_encoder.rs` (new `#[cfg(test)] mod tests`):
  the three `normalize_supported_*` tests plus
  `normalize_keeps_utf8_fallback_for_object_path` (pins the object-path
  UTF-8 fallback so the change stays scoped to the NAME path).

## Follow-up & risk

- Behavioral risk (intended): apps that constructed a Reader/Writer or called
  `getBytes` with a charset *name our engine doesn't implement yet* (Shift_JIS,
  EUC-JP, Big5, GBK, GB2312, GB18030, EUC-KR, ISO-2022-JP, KOI8-U, windows-1250)
  previously got a silently-wrong UTF-8/Latin-1 transcode; they now get the
  faithful `UnsupportedEncodingException`. This is correct JVM behavior but
  converts a silent-wrong-output into a thrown exception — watch app-gauntlet
  for any suite that exercised one of those names. Net positive (no more silent
  corruption); if a real app legitimately needs one of these charsets the right
  fix is to add the coder to `native-api/src/charset.rs`, not to re-mask it.
- The two `normalize_supported` copies in stream_decoder/stream_encoder remain
  duplicated (native-io cannot depend on native-builtins without a cycle) —
  unchanged from the pre-existing duplicated `normalize`; a shared
  charset-name table in `cratonvm-native-api` (owned by another agent) would
  de-dup it. Out of scope here.
- `String.getBytes(String)` keeps encoding the checked exception as
  `RuntimeError::IOException` (message-prefixed), matching the method's
  existing convention; the StreamDecoder/StreamEncoder factories construct the
  concrete `UnsupportedEncodingException` class via `new_object_initialized`,
  which is more faithful for a narrower `catch (UnsupportedEncodingException)`.
