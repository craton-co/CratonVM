# HIB-BYTEBUDDY blanket JIT ban — fully lifted after 302-class closure

**Status:** FIXED. The `net/bytebuddy/` blanket guard is absent, Byte Buddy is
JIT-eligible, and every class in the historical 302-crash corpus has a complete
PASS execution under both JIT and `--nojit`.

## History and root cause

1. On 2026-06-13, `net/bytebuddy/` was blanket-banned after
   `SimpleEnhancerTests` hung in `TypeDefinition$Sort.describe` and
   `TypeDescription.represents`.
2. On 2026-07-28, the ban was removed after the named witness and a 15-class
   Hibernate enhancement sample passed. That sample was too narrow to validate
   a dependency used transitively across the suite.
3. A later 4,548-class run using the older `77389fa06` runtime with the ban
   absent recorded 302 CRASH rows. Reinstating the guard on that old runtime
   changed the corpus to 298 PASS, three GRAPH-queue assumption aborts, and one
   300-second timeout.
4. The exact no-ban crash report names
   `net/bytebuddy/description/ModifierReviewable$AbstractBase.matchesMask(I)Z`
   and faults while fetching an instruction in the middle of its JIT body.
   This is the signature of a compiled body being unmapped while a live frame
   still executes it, not a Byte Buddy bytecode miscompile.
5. The crash binary predates the JIT code-lifetime fixes now on `dev`:
   `3fe14734a` defers superseded artifacts until JIT execution is quiescent,
   `ac300e6f6` ties code-buffer release to live JIT frames, and `463bd32e2`
   routes every cache retirement through that ownership protocol.

The temporary conclusion that the ban itself was the fix was therefore wrong:
it only hid the stale-code lifetime defect in the older binary.

## Current-dev no-JIT regression closed during verification

Reconciliation with current `dev` exposed a separate failure in
`ASTParserLoadingTest.testComponentQueries`: `--nojit` intermittently corrupted
the valid tuple query `('John','X','Doe') = h.name` into an invalid `*=` token
sequence.

The cause was the newly added stackless
`try_execute_cached_trivial_instance_getter` shortcut. It bypassed the ordinary
`getfield` interpreter path for generated accessors and did not preserve all of
its runtime semantics. Removing that unsafe shortcut restored the canonical
field-access path. The parser class then passed 106/106 in both modes, while the
three focused GRAPH-queue regression classes remained fully green in both
modes.

## Final validation

Worktree:
`C:\craton\CratonVM-bytebuddy-complete-20260729-019fadd3`

No-ban release artifact:
`C:\craton\bytebuddy-complete-results-20260729-019fadd3\cratonvm-bytebuddy-final5-019fadd3.exe`

SHA-256:
`EFCB3645A855B3B1D88112098A901930F5ACCCB414E21D06E7CE402EC2A21886`

Authoritative manifest:
`C:\craton\bytebuddy-complete-results-20260729-019fadd3\bytebuddy-crash302-20260728.txt`

Canonical results:

| Mode | Classes | Found | Started | OK | Failed | Aborted | Skipped |
|---|---:|---:|---:|---:|---:|---:|---:|
| JIT | 302/302 | 1,281 | 1,275 | 1,275 | 0 | 0 | 6 |
| `--nojit` | 302/302 | 1,281 | 1,275 | 1,275 | 0 | 0 | 6 |

The six skips are the same fixture-declared skips in both modes, across four
classes. Every discovered non-skipped test started and passed.

The three action-queue classes that abort under Hibernate's legacy queue default
were not accepted as green. The canonical runner applies
`-Dhibernate.flush.queue.type=graph` to those classes in both modes:

- `UpdateDecomposerTest`: 13/13
- `NewEntityOrderColumnTest`: 3/3
- `MixedOperationsTest`: 9/9

`ManyToManyAssociationClassGeneratedIdTest`, whose three legacy-ordering tests
are intentionally incompatible with the GRAPH queue, was run with
`-Dhibernate.flush.queue.type=legacy` and passed 6/6 in both modes. Thus no
assumption-only row was counted as green.

`ASTParserLoadingTest` is also fully executed, not waived as a timeout:

- JIT: 106/106 in 320,211 ms
- `--nojit`: 106/106 in 259,371 ms, serially at the documented 2 GiB heap

Per-class selected markers and their source logs are preserved in:

- `C:\craton\bytebuddy-complete-results-20260729-019fadd3\final5-canonical-jit-results.tsv`
- `C:\craton\bytebuddy-complete-results-20260729-019fadd3\final5-canonical-nojit-results.tsv`

Both files contain exactly 302 unique manifest classes. Their audit requires
`found > 0`, `started == ok`, `started + skipped == found`, and zero failed or
aborted tests for every selected row.

## Permanent gate

`vm/src/jit/skip_list.rs` now tests that the exact historical faulting method
and both original hang-witness methods remain JIT-eligible under Conservative
and Aggressive policies. A future blanket `net/bytebuddy/` guard therefore
fails the unit suite instead of silently returning.

## Conclusion

HIB-BYTEBUDDY is closed with the ban lifted. The broad Hibernate corpus proves
the current JIT code-ownership fixes, rather than an interpreter-only package
exception, are the durable resolution.
