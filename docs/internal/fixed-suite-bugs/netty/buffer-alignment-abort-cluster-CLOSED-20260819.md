# 7 buffer classes reading `ABORTED` — measured on HotSpot, and taken off the re-investigation treadmill

**Status: CLOSED 2026-08-19.** Supersedes
`known-issues/netty/buffer-alignment-abort-cluster-not-a-cratonvm-bug-20260819.md`,
which reached the right verdict on one class and *inferred* it for the other
six. All seven have now been run on stock HotSpot 25 on the same host with the
same classpath, one fork per class, and all seven are byte-for-byte identical.

| class | found | ok | aborted | HotSpot 25 | CratonVM |
|---|---:|---:|---:|---|---|
| `AdvancedLeakAwareCompositeByteBufTest` | 506 | 497 | 9 | measured | identical |
| `AlignedPooledByteBufAllocatorTest` | 49 | 21 | **28** | measured | identical |
| `BigEndianCompositeByteBufTest` | 496 | 487 | 9 | measured | identical |
| `LittleEndianCompositeByteBufTest` | 496 | 487 | 9 | measured | identical |
| `PooledByteBufAllocatorTest` | 47 | 45 | 2 | measured | identical |
| `SimpleLeakAwareCompositeByteBufTest` | 506 | 497 | 9 | measured | identical |
| `WrappedCompositeByteBufTest` | 496 | 487 | 9 | measured | identical |

`failed=0` on both VMs for all seven. The predecessor page carried the
CratonVM column and only `AlignedPooledByteBufAllocatorTest`'s HotSpot column,
and said so ("flagged as inference, not independently measured, for whoever
revisits this"). The other six now agree exactly, which is the ordinary
outcome for the mechanism involved but was not a thing anyone had checked.

## Why the counts look arbitrary and are not

Every abort in all seven comes from one capability assumption:

```java
assumeTrue(PooledByteBufAllocator.isDirectMemoryCacheAlignmentSupported());
```

which answers `false` on this host for both VMs.
`AlignedPooledByteBufAllocatorTest` gates EVERY test method (and its
`newAllocator` override) on it, hence 28 of 49. The other six reach it only
through the subset of parameterizations that ask for an aligned pooled
allocator, hence 9 (or 2) out of ~500 — five of the seven pass 96-98% of their
sub-tests, and reading the class-level `ABORTED` label without reading
`aborted=` is the trap this cluster exists to illustrate.

## What was actually done about it

The predecessor page's disposition said "if a future doc-sweep wants to stop
these seven reading as `ABORTED` in a status table, the harness's classifier is
the thing worth revisiting, not the VM". The mechanism for that already
existed by the time this was picked up:
`apps/netty-suite-runner/known-benign-aborts.tsv`, force-added to git (the
whole of `apps/` is gitignored), consumed by `run-netty-suite.sh categorize`.
All seven were added to it with the measured counts:

```
io.netty.buffer.AdvancedLeakAwareCompositeByteBufTest	506	497	9
io.netty.buffer.AlignedPooledByteBufAllocatorTest	49	21	28
io.netty.buffer.BigEndianCompositeByteBufTest	496	487	9
io.netty.buffer.LittleEndianCompositeByteBufTest	496	487	9
io.netty.buffer.PooledByteBufAllocatorTest	47	45	2
io.netty.buffer.SimpleLeakAwareCompositeByteBufTest	506	497	9
io.netty.buffer.WrappedCompositeByteBufTest	496	487	9
```

That table is an EXACT-COUNT match, not a name match: a class is reclassified
only when a fresh run reproduces all three numbers. If the abort profile ever
shifts — a new failure, or a different number of self-skips because the
assumption logic changed — the class lands in `others.txt` like any other
residual, so this cannot mask a regression on a class it lists. That is
strictly safer than the alternative the predecessor page floated (a blanket
`aborted == assumed → PASS` rule in the classifier), which would have made the
seven invisible whatever their counts became.

Check the table is live with:

```bash
cd /data/cratonvm/apps/netty-suite-runner
./run-netty-suite.sh benign-aborts
```

## Disposition

No VM fix applies, and none was made. The `known-benign-aborts.tsv` rows are
the entire change.

## Related

- `known-issues/netty/fail-hang-crash-rerun-20260817.md` — where this cluster
  was first flagged as untriaged.
- `known-issues/netty/openssl-key-material-and-engine-residuals-20260813.md` —
  the same "don't read a class's `ABORTED` label without checking the
  `aborted=` count" trap, documented once already for a different class.
- `fixed-suite-bugs/netty/dnsnameresolvertest-windows-only-aborts-CONFIRMED-20260819.md`
  — the first entry in `known-benign-aborts.tsv`, and the page explaining why
  that table exists at all.
