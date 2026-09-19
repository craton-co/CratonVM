# Inline allocation and reference publication

## TLAB allocation

A TLAB has one owning mutator. Allocation reserves an aligned private span,
initializes the complete body and walker-visible header, executes a release
fence, and commits the cursor last. The cursor is the allocation linearization
point.

The collector reads TLAB boundaries and object headers only after the
stop-the-world handshake (acquire side). It can therefore observe either the
old cursor, in which case the in-flight span remains part of the reserved tail,
or the new cursor and the complete initialized object. It cannot observe a
committed zero/malformed header.

The Rust TLAB allocator and x64 inline allocator implement the same sequence:

`reserve -> zero body -> initialize header -> Release -> cursor commit`

Rust expresses the release edge with `atomic::fence(Ordering::Release)`.
Generated x64 uses an ordinary cursor store: x86-64 TSO already preserves
store-store order, so an `SFENCE` would add latency without strengthening this
contract. The acquire side is the completed stop-the-world handshake.

## Reference stores

Every heap reference update uses this ordered triad:

`SATB(old) -> slot.store(new) -> Release(card or remembered-set edge)`

SATB runs before overwriting a non-null old value. No safepoint or helper call
is permitted between the slot store and its post barrier. At collection, the
acquire read of the card/remembered-set state makes the preceding slot value
visible before the collector follows the edge.

For the generational collector, card bytes are stable `AtomicU8` cells. The
x64 backend range-checks source and target, computes
`(source - old_base) >> 9`, and release-stores `CARD_DIRTY` directly. On x64,
the release is the naturally atomic byte store ordered by TSO; no `SFENCE` is
needed. This is
used by inline reference-array stores and eligible compact/legacy field
stores. The STW consumer acquire-scans the atomic bitmap and merges direct JIT
marks with helper-buffered marks.

G1 and ZGC expose no generational card metadata. Their JIT paths continue to
call the collector-specific post-barrier helper, preserving G1 remembered-set
and ZGC barrier semantics.
