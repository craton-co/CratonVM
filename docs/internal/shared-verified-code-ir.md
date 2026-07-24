# Shared verified-code foundation

Status: implemented

The verifier and optimizing JIT now consume one canonical method analysis from
`cratonvm-reader`.

`VerifiedCode` owns:

- the canonical decoded instruction stream and bytecode boundaries;
- validated direct, wide, and switch branch targets;
- merge targets, including conditional fallthroughs;
- loop headers derived from backward control-flow edges.

Every target is range checked and must land on a decoded instruction boundary.
The class verifier requests this analysis before type-state verification, and
the IR builder requests the same analysis before creating merge and loop
nodes. This removes instruction-length and CFG drift from the verifier/JIT
trust boundary. The single-pass backend remains the fallback for bytecodes the
optimizing IR does not lower.

Analyses are content-addressed. Hash collisions are resolved by comparing the
complete bytecode slice, so the hash is not a correctness boundary. The cache
is split into 16 mutex shards; decoding happens outside shard locks and
publication uses a second lookup to coalesce races. Each shard is bounded to
256 methods and 4 MiB of source bytecode, clearing the shard when either limit
is reached.

This phase intentionally does not change any JIT exclusion policy. The
exclusion-by-exclusion differential campaign, compiler-pass repairs, and skip
deletion were explicitly excluded from this session.
