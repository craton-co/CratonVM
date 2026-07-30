# Concurrent serialization divergence under JIT — resolved 2026-07-29

## Root cause

Several native collection paths retained `ObjectRef`s only in Rust vectors or
locals while invoking Java code or allocating a destination collection. A
moving collection could therefore relocate those objects, after which a later
comparison, hash, iterator store, or wrapper publication used the stale
address. The issue surfaced as intermittent concurrent wire-serialization
inequality in Elasticsearch snapshots and had previously been hidden by a
broad Elasticsearch JIT restriction.

The repair roots and refreshes all affected references across re-entry in the
interpreter/native invocation boundary and native collection operations:

- native map and list hash/equality snapshots;
- generic list comparison snapshots while both iterators run;
- set hash/equality snapshots and set iterator array materialization;
- unmodifiable-wrapper allocation; and
- `Arrays.hashCode(Object[])` array traversal.

## Validation

All validation used b12, SHA-256
`6bbf1cfd02447e3ad8eb0fe0631395b0527c5ab9793e9af9a87a6ca1bc58bead`,
from the isolated Azure worktree and target directory.

- `SnapshotsInProgressSerializationTests`: 3/3 JIT and 3/3 `--nojit`, each
  `OK (10 tests)`.
- `StringRareTermsTests`: 6/6 JIT and 6/6 `--nojit`, each `OK (7 tests)`.
- `LongRareTermsTests`: 2/2 JIT and 2/2 `--nojit`, each `OK (7 tests)`.
- `cargo test -p cratonvm-native-collections --lib`: 85/85 passed.

After merging current `origin/dev`, b13 (SHA-256
`31bffd4d7a46fca5393e5b503b3485acbde4e390eeb6a1ead6164660446d41b3`)
re-ran the exact snapshot class: JIT 3/3 and `--nojit` 4/4 clean consecutive
repetitions (r2-r5), each `OK (10 tests)`.


After the final current-`origin/dev` merge, b14 (SHA-256
`cf044b03c2ae652a56686b12ab2d2c869e8a7d78c47564e3f31bacaa110ec4ac`)
re-ran that exact class: 1/1 JIT and 1/1 `--nojit`, each `OK (10 tests)`.

The issue is retired from `docs/known-issues` because the exact reproducer and
the linked residual classes are green in both execution modes.
