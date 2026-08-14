# ZGC compaction slid survivors over live objects on an unselected page — FIXED

**Status: FIXED 2026-08-14.** Supersedes
`known-issues/zgc-relocate-cursor-panic-on-netty-tls-20260814.md`, which was
opened the same day off a netty TLS gauntlet run and bisected the fault to the
compaction half of the 2026-08-13 default flip but not to a line.

## The report

```
thread 'main-vm' panicked at gc/src/arena.rs:1884:9:
compaction must not raise the cursor: 15348137664 > 168782728
```

5 of 5 runs on `io.netty.handler.ssl.SslErrorTest`, Azure host 2, ZGC only.
One run presented as a bare SIGSEGV instead. `CRATONVM_ZGC_RELOCATE=0` passed
72/72, which is what put it on the compaction path.

## The two numbers were the diagnosis

`15348137664 = 1,918,517,208 × 8`, and 1,918,517,208 fits in an `i32`. That is
`array_data_size(length, elem)` for an 8-byte element type and a length read
out of an `ObjectHeader`. So the cursor was not being nudged past the top of a
168 MB arena by an arithmetic slip — **it was being computed from a header that
was not an object header**, i.e. from memory something had already overwritten.

The panic is therefore two steps downstream of the defect, and the assertion
that fired is a guard that caught a corruption rather than the corruption
itself. That is why the same run could also present as a SIGSEGV, and why a
third presentation — clobbered bytes that happen to decode plausibly — is
possible and would have been silent.

## Root cause

`ZRelocationSet::select` ranks candidate pages by **descending garbage ratio**
and takes a prefix under a byte budget, skipping any page at or above
`max_live_occupancy` (25%). The selected page ids are therefore an arbitrary
and **non-contiguous** set.

The slide in `ZgcRealHeap::relocate_stw` marched a single `dest` cursor upward
from the first selected page and placed every survivor consecutively:

```rust
let mut dest = slide_floor;
for from in survivors {
    let to = (dest + 7) & !7;
    if to < from { unsafe { std::ptr::copy(from as *const u8, to as *mut u8, size) }; }
    dest = to + size;
}
```

Nothing in that loop knows about unselected pages. Once the selected pages'
live bytes exceeded the gap between `slide_floor` and the first dense page,
survivors from the pages **above** the dense page were memmoved straight over
the live objects **on** it.

`SslErrorTest` is 72 TLS handshakes against a BoringSSL-backed context, which
is exactly the shape that produces a mixed occupancy profile — sparse
handshake-buffer pages interleaved with dense session/context/certificate
pages. The two allocation-heavy non-TLS classes in the report
(`PooledByteBufAllocatorTest`, `UnpooledTest`) do not trip it because uniform
allocation gives uniform occupancy, so the selection comes out contiguous.

**The comment eight lines above the loop stated the correct rule** — *"An
object on an unselected page must not move, so the slide's destination cursor
has to start above the highest unselected survivor"* — and the code implemented
something else. The rule was known; only the code disagreed.

## The fix

The destination span must lie entirely inside selected pages. Before placing a
survivor, probe the candidate address: if `[cand, cand+size)` touches any
unselected page, restart the probe at the page after the blocking one. If no
placement strictly below `from` exists, the object stays put and `dest`
continues above it so a later survivor cannot be placed on top of it.

Skipping rather than clamping is deliberate: clamping the slide to the pages
below the first dense page would also have stopped the corruption, and would
have quietly turned every non-contiguous selection into a near-no-op. The
regression test asserts both halves for that reason.

**Defence in depth, added with it.** `highest_pinned_end` now refuses an extent
that runs past the bump cursor. `alloc_size` has no arena to check against and
its array arm is deliberately unscreened (a 4 GB `char[]` is legal and is what
lets ZGC pass `TestCharChunkLargeHeap`), so the check belongs at the caller that
does have one. A survivor claiming more bytes than the heap holds now pins the
cursor and reclaims nothing for that cycle, with a warning — conservative, and
strictly better than either trusting the number or aborting.

## Regression test

`zgc::tests::compaction_must_not_slide_survivors_over_an_unselected_dense_page`
builds six 2 MiB logical pages with page 1 held at 75% live (never selected)
and the rest at 20% (always selected), so the selected pages' live bytes exceed
the 2 MiB below the dense page. Before the fix: **3 of 196 live objects on the
dense page were overwritten**. After: 0.

It also asserts `objects_relocated > 0`, because "nothing was corrupted" is
satisfied by "nothing was moved" and the difference between those two is the
whole value of the change.

## What this says about the flip

The 2026-08-13 default flip shipped two kill switches specifically so a
gauntlet failure could be bisected to a flag in one run, and that is what
happened: `CRATONVM_ZGC_RELOCATE=0` isolated it before anyone read a line of
allocator code. The defect itself predates the flip — it was in the compaction
path from the day that path landed — and was unreachable only because
compaction was opt-in and nothing opted in.

**A latent memory-corruption bug in a default-off feature is not a safe state,
it is an unmeasured one.** Turning it on is what produced the report.
