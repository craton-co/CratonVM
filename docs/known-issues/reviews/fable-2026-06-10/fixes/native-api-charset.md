# Fix: native-api/src/charset.rs — lossy fallbacks produced plausible-but-wrong output

## Finding

native-api.md **V2 (LOW)** — charset lossy fallbacks silently substitute on an
unsupported charset name (and, more broadly, on an unmappable char in a
supported single-byte charset), producing plausible-looking but wrong output:

- `charset.rs:149` — `decode_bytes_lossy` returned an **all-`U+FFFD`** buffer
  for an unsupported charset name. This discards every valid byte and only
  coincidentally preserves the length (an arbitrary 1:1 byte→char map for an
  unknown name).
- `charset.rs:167` — `encode_chars_lossy` **silently re-encoded as UTF-8** for
  an unsupported name. A sink expecting (say) `windows-1252` then received
  UTF-8 multibyte sequences — bytes in the wrong encoding.
- Same line — the supported single-byte code pages (windows-1252/1251, KOI8-R,
  ISO-8859-2/15) that legitimately fail strict encode with `Unmappable` ALSO
  fell into that UTF-8 catch-all, so a lossy `windows-1252` encode of an emoji
  emitted UTF-8 bytes instead of the `'?'` REPLACE substitution.

## Root cause

The `*_lossy` functions have an **infallible** signature (`-> Vec<u16>` /
`-> Vec<u8>`) — callers in `native-builtins/src/charset.rs`,
`native-io/src/stream_decoder.rs`, and `native-io/src/stream_encoder.rs` (none
owned here) consume a plain `Vec`. So the unsupported-charset case cannot
surface a `CodingError`; the original catch-alls chose the *worst* infallible
substitutions (all-U+FFFD on decode; whole-input UTF-8 on encode). And the
encode match arm only special-cased US-ASCII / ISO-8859-1, letting every other
supported charset's `Unmappable` error fall through to UTF-8.

## Exact change (file:line)

`native-api/src/charset.rs`

1. `decode_bytes_lossy` catch-all (was line 149): replace the
   `bytes.iter().map(|_| REPLACEMENT_CHAR)` all-U+FFFD buffer with an
   **ISO-8859-1 (Latin-1) byte-identity** passthrough
   (`bytes.iter().map(|&b| b as u16)`). Latin-1 is the only charset that maps
   every byte 0x00..=0xFF losslessly, so this preserves byte identity and is
   round-trippable instead of fabricating replacement chars for valid data.

2. `encode_chars_lossy` `Err(_)` arm (was lines 157-168), rewritten:
   - UTF-8/UTF-16{,BE,LE}/UTF-32{,BE,LE} → `encode_utf8` (representable;
     effectively unreachable since those encoders already substitute U+FFFD).
   - US-ASCII / ISO-8859-1 → unchanged per-char `'?'` substitution.
   - **windows-1252/1251, KOI8-R, ISO-8859-2, ISO-8859-15** → new
     `encode_sb_lossy(chars, <cs>_rev())` — encodes mappable units in the
     *target* charset and substitutes `'?'` (REPLACEMENT_BYTE) for unmappable
     ones, instead of dropping to UTF-8.
   - Unsupported name → Latin-1 byte-identity with `'?'` substitution
     (`if c < 0x100 { c as u8 } else { REPLACEMENT_BYTE }`), never the wrong
     encoding.

3. New private helper `encode_sb_lossy(chars: &[u16], rev: &[u8; 65536]) -> Vec<u8>`
   (added after `encode_chars_lossy`): mirrors the existing `encode_sb`
   (`c < 0x80` passthrough, `rev[c as usize]` lookup, `0`-slot = unmappable) but
   never errors — emits `REPLACEMENT_BYTE` for the `0` slot. Reuses the same
   prebuilt reverse tables (`cp1252_rev()` etc.).

## Files touched

- `native-api/src/charset.rs` (the two lossy functions + one new private helper
  + 3 inline `#[cfg(test)]` tests).
- `docs/reviews/fable-2026-06-10/fixes/native-api-charset.md` (this note).

No other files edited. Public signatures of `decode_bytes_lossy` /
`encode_chars_lossy` are unchanged, so the non-owned callers are unaffected.

## Tests added

Three inline tests in the existing `mod tests`:
- `lossy_decode_unknown_name_is_latin1_not_all_replacement` — unknown name
  decode round-trips bytes as Latin-1, no U+FFFD.
- `lossy_encode_unknown_name_is_latin1_with_question_mark` — unknown name
  encode keeps representable bytes, emits `'?'` for the rest (emoji = 2 `'?'`).
- `lossy_encode_single_byte_charset_substitutes_question_mark` — windows-1252
  lossy encode of `A` + euro + emoji = `[b'A', 0x80, b'?', b'?']` (target
  charset, not UTF-8).

All use only `encode_utf16()` / `decode_bytes_lossy` / `encode_chars_lossy` /
`REPLACEMENT_CHAR`, matching existing test patterns in the module; no new deps.
Pre-existing tests (`lossy_decode_replaces`, integration
`lossy_handles_malformed_utf8`, `lossy_encode_unmappable_becomes_question_mark`)
exercise only supported-charset paths and are unaffected.

## Follow-up & risk

- Risk: very low. Changes are confined to the lossy error-fallback arms; the
  strict `decode_bytes`/`encode_chars` happy paths and all supported-charset
  success paths are untouched. The infallible signature is preserved so no
  caller needs updating.
- Behavioral note for downstream owners: any caller that *relied* on the old
  all-U+FFFD decode or UTF-8 encode for an unknown charset name now gets a
  Latin-1 byte-identity result instead. This is strictly more faithful, but if
  some path was using the U+FFFD count as an "is-this-charset-known" probe it
  should instead call the fallible `decode_bytes`/`encode_chars` and check for
  `CodingErrorKind::UnsupportedCharset`. (No such probe found in this crate.)
- Not changed (out of scope / different owners): the fallible `decode_bytes` /
  `encode_chars` already correctly return `UnsupportedCharset` for unknown
  names — that contract is intact. Surfacing the unsupported-charset error all
  the way to a Java `UnsupportedCharsetException` would require the byte-sink
  callers (native-builtins / native-io) to switch from the lossy to the
  fallible API; left as a note since those files are not owned here.
