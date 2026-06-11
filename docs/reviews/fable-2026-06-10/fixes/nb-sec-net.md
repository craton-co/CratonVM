# Fix note: nb-sec-net (HTTP/1 chunked + HTTP/2 frame DoS; HPACK Huffman stub)

Owned files: `native-builtins/src/http_client.rs`, `native-builtins/src/http2.rs`.

## Finding

From `docs/reviews/fable-2026-06-10/nb-security.md`:

- **B3 (HIGH DoS)** — `http_client.rs::read_chunked` has a chunk-size integer
  overflow (`size + 2` wraps for a near-`usize::MAX` size → later `&prefix[..size]`
  panic) and buffers an entire chunk *before* the body cap is checked (a single
  `0xFFFFFFFF` chunk header forces a multi-GiB allocation). The HTTP/2 frame loop
  in `http_client.rs::http2_request` is similarly unbounded: `vec![0u8; length]`
  per frame with no `SETTINGS_MAX_FRAME_SIZE` enforcement (24-bit length ⇒ up to
  16 MiB per frame) and `header_block` accumulating across CONTINUATION frames
  with no cap (endless CONTINUATION → OOM, the B4 overlap).
- **S2 (STUB / correctness)** — `http_client.rs::decode_hpack_string` returns the
  raw (still Huffman-coded) bytes via `from_utf8_lossy` when the Huffman bit is
  set, i.e. silently returns garbled header data for the common case (servers
  Huffman-code compressible header strings by default).

## Root cause

- `read_chunked` parsed the hex size with no upper bound and did unchecked
  `size + 2` arithmetic, and the body-cap check ran *after* the chunk was fully
  buffered.
- `http2_request` trusted the wire `length` field for `vec![0u8; length]` and
  never bounded the cumulative `header_block`.
- The read-side HPACK Huffman decoder was never implemented (the code only ever
  emitted non-Huffman strings on the encode side, so the decode side was a stub).

## Exact change (file:line)

`native-builtins/src/http_client.rs`
- Import the new decoder: `use crate::http2::{hpack_huffman_decode, HpackStaticTable};`
  (was `HpackStaticTable` only).
- New constants `H2_MAX_FRAME_SIZE = 16 KiB` and `H2_MAX_HEADER_BLOCK = 256 KiB`
  next to `MAX_RESPONSE_BODY`.
- `read_chunked`: after parsing `size`, reject `size > MAX_RESPONSE_BODY`
  *before* any buffering; reject the running aggregate `out.len() + size >
  MAX_RESPONSE_BODY` before reading the chunk body (so `size + 2` can no longer
  overflow — `size` is now provably ≤ 16 MiB).
- `http2_request` frame loop: reject `length as usize > H2_MAX_FRAME_SIZE`
  *before* `vec![0u8; length as usize]`; cap `header_block.len() + p.len()` /
  `header_block.len() + payload.len()` against `H2_MAX_HEADER_BLOCK` in both the
  HEADERS (0x1) and CONTINUATION (0x9) arms.
- `decode_hpack_string`: when the Huffman bit is set, call
  `hpack_huffman_decode(bytes)` and `String::from_utf8`, propagating any error
  (fail-closed) instead of `from_utf8_lossy` of the coded bytes.
- `decode_hpack_int`: replaced the unchecked `value += (..) << shift` with
  `checked_shl` + `checked_add` (B5 hardening in the same hot path).

`native-builtins/src/http2.rs`
- Added the full RFC 7541 Appendix B Huffman code table `HPACK_HUFFMAN_TABLE`
  (257 `(code, bit_length)` entries incl. EOS at index 256), the public
  `hpack_huffman_decode(&[u8]) -> Result<Vec<u8>, String>` decoder, and the
  private `huffman_lookup` helper, placed right after the `HpackStaticTable`
  impl. The decoder is greedy over the prefix-free code, **fails closed** on an
  in-stream EOS symbol, on a leftover code ≥ 8 bits, and on padding that is not
  all-ones (RFC 7541 §5.2), and bounds the output `Vec` capacity at
  `input_len*8/5 + 1`.

## S2 decision

**Implemented** a correct RFC 7541 Huffman decoder (not a fail-closed stub). It
is validated against the RFC 7541 Appendix C.4 test vectors
("www.example.com", "no-cache", "custom-value"). It still fails closed on any
malformed/over-padded input so it can never return wrong data silently.

## Tests added

`http2.rs` (`mod http2_tests`):
- `test_hpack_huffman_decode_known_vectors` — RFC 7541 C.4.1/2/3 vectors.
- `test_hpack_huffman_decode_empty` — empty input → empty output.
- `test_hpack_huffman_decode_rejects_bad_padding` — `0x00` (non-all-ones pad) → Err.

`http_client.rs` (`mod http_client_tests`):
- `test_read_chunked_happy_path` — `5\r\nhello\r\n0\r\n\r\n` → `hello`.
- `test_read_chunked_rejects_oversized_chunk_no_alloc` — `ffffffffffffffff` size → Err, no alloc.
- `test_read_chunked_rejects_large_but_parseable_chunk` — `7fffffff` (~2 GiB) → Err.
- `test_decode_hpack_string_huffman_roundtrip` — H=1,len=12 "www.example.com".

All new tests use only patterns already present in the file (`std::io::Cursor`,
`MockNativeContext`, the `WireResponse`/`read_chunked`/`decode_hpack_string`
free functions reachable via `super::*`).

## Files touched

- `native-builtins/src/http_client.rs`
- `native-builtins/src/http2.rs`
- `docs/reviews/fable-2026-06-10/fixes/nb-sec-net.md` (this note)

## Follow-up & risk

- The HTTP/2 path in `http2.rs` (`http11_request_impl` + `read_limited` +
  `decode_chunked`) is already bounded, so no DoS change was needed there.
- `H2_MAX_FRAME_SIZE` is fixed at the 16 KiB HTTP/2 default. If a future change
  advertises a larger `SETTINGS_MAX_FRAME_SIZE`, raise this constant in lockstep.
- `huffman_lookup` is a linear scan (257 × ≤26 candidate lengths per symbol).
  Fine for header strings; if a hot path ever decodes large Huffman bodies,
  swap in a precomputed length-indexed lookup. Not a correctness concern.
- Risk: low. Changes are additive (new decoder + bounds checks) and reject paths
  return the module's existing `Result<_, String>` error type; the happy path
  for conformant servers is unchanged except that Huffman-coded headers now
  decode correctly instead of arriving garbled.
- Not done (other agents' files): B1/B2 (crypto_impl.rs DER/RSA overflow), S1
  (jca/key_factory.rs empty PQC keys), V1–V5 — all outside my owned files.
