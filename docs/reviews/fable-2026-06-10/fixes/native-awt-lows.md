# Fix note — native-awt-lows

Agent id: `native-awt-lows`
Owned files: `native-awt/src/renderer.rs`, `native-awt/src/natives.rs`
Scope: the two remaining low/latent findings from `native-awt.md` (V2, V3).
B1/B2/B3/B4/V1 were fixed in earlier rounds and were NOT touched.

## V2 (low, latent) — `bilinear_sample` / `bicubic_sample` OOB read on short slice

`renderer.rs`. Both free-standing samplers computed `src[(y*w + x)]` with
`x`/`y` clamped only to `w-1`/`h-1`, never to `src.len()`. The current caller
(`blit_image_scaled` fed by `BufferedImageData`, whose `pixels.len() == w*h`)
is sound, but the functions are public-to-the-module and free-standing, so any
future caller (or a corrupt/short buffer reaching here) passing
`src.len() < w*h` would index past the slice and panic.

Fix: in each function, replace the direct `src[...]` indexing with a small
`texel` closure that does `src.get(idx).copied().unwrap_or(0)`. The geometric
clamp is preserved (so in-range behaviour is byte-identical); only out-of-range
indices — which previously panicked — now fail closed by reading transparent
black (`0`). This means a short/untrusted buffer can never read past the slice.

- `bilinear_sample`: the four corner reads (`c00`/`c10`/`c01`/`c11`) now go
  through `texel(i32, i32)` (`renderer.rs` ~1668).
- `bicubic_sample`: the 4x4 neighbourhood read (`px`) now goes through
  `texel(u32, u32)` (`renderer.rs` ~1730).

Behaviour-neutral for every existing caller: when `src.len() >= w*h` (the
documented invariant), every index `texel` produces is in range, so
`get(idx).unwrap()` yields exactly what the old `src[idx]` yielded.

Test added: `test_sample_short_slice_no_oob` (renderer.rs test module) drives
both samplers with a 1-element slice declared as 4x4 and with an empty slice;
asserts no panic and `0` for fully out-of-range samples.

## V3 (low, by-design) — `lookup_peer_source` `ObjectRef::from_raw` resurrection

`natives.rs`. A raw heap pointer cached in the peer-source side-table is rebuilt
into an `ObjectRef` via `ObjectRef::from_raw`. The existing GC-generation gate
(`entry.gc_gen != current_gc_gen -> None`) already fails closed across any
collection, and the module comment documents the use-after-free risk. The
residual flagged in V3 is the *soundness assumption* plus the lack of a real
validity check matching `from_raw`'s preconditions.

Two changes (both fail-closed, consistent with the existing null/GC-gen guards):

1. Added an alignment validity guard before `from_raw`. `ObjectRef::from_raw`
   requires a non-null, 8-byte-aligned pointer (it `debug_assert!`s both).
   `lookup_peer_source` already rejected `ptr == 0`; it now also rejects
   `entry.ptr & 0b111 != 0`. Every CratonVM heap object is 8-byte aligned, so a
   non-aligned cached pointer is provably bogus (corrupt/forged side-table
   entry) and is refused rather than handed to `from_raw`. This mirrors the two
   preconditions `from_raw` documents, giving a real liveness/validity check on
   top of the GC-gen gate.

2. Tightened the SAFETY comment to state the precise load-bearing invariant: the
   GC-gen gate is sound **iff** `NativeContext::gc_collection_count()` increments
   on *every* collection that can free or relocate a heap object — moving (G1
   evacuation / full compaction) AND any non-moving sweep that reclaims dead
   objects. If a collector path freed `ptr`'s object without bumping the count,
   the gate would let a freed pointer through. The comment now calls this out so
   a future collector that frees without bumping the counter is flagged as
   breaking this invariant.

Test added: `peer_source_lookup_rejects_misaligned_pointer` (natives.rs test
module) inserts a misaligned cached pointer (`0x1001`) and a null pointer
directly into the side-table at the matching GC generation and asserts
`lookup_peer_source` returns `None` for both — i.e. neither reaches `from_raw`.

## Compile / config safety

Both files are in the `native-awt` crate, which is not feature-gated by
`app-stubs` / `synthetic-jdk`. The edits use only already-imported types
(`ObjectRef`, `PeerSourceEntry`, `PeerId`, `peer_source_table`) and std slice
methods (`get`/`copied`). No new imports, no new public API, no behaviour change
on the existing in-range paths. No `cargo`/`git` was run (per task rules);
confidence in compilation is high based on type/borrow analysis.
