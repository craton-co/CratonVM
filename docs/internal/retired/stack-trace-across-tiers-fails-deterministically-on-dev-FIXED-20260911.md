# `stack_trace_across_tiers` fails deterministically on `dev`, and it is a ratcheted e2e target

**Status:** open, found 2026-09-09. Not diagnosed; attribution narrowed but NOT
bisected.
**Severity:** this target is listed in `tools/e2e-ratchet.txt`, so the CI step
"E2E prerequisites are real (CRATONVM_REQUIRE_E2E)" — `ubuntu-latest`,
`timeout-minutes: 15` — is RED on `dev` until it is fixed or the row is pulled.
**Found by:** timing the four cost-deferred candidates in `tools/e2e-ratchet.txt`
on Linux, which runs the whole 78-target list as a side effect. It was not
looked for.

## The failure

```
test a_warmed_up_stack_trace_keeps_every_frame_and_every_line ... FAILED
panicked at vm/tests/stack_trace_across_tiers.rs:654:

  CRATONVM_JIT_NO_INLINE_FRAME_MAP=1 still shows `mid` in after_main_osr, so it
  no longer reverts the inline-frame map. Either the switch stopped being read
  by both halves (the emitter in jit/src/x64/inlining.rs and the walk in
  vm/src/jit/conservative_roots.rs read the same name deliberately), or `mid`
  was never inlined in this run and the default arm's five frames prove nothing
  about the map.

  got:         len=5 [leaf:-1 mid:26 outer:27 probe:42 main:66 ]
  default arm: len=5 [leaf:25 mid:26 outer:27 probe:42 main:66 ]
```

Note the two rows differ in ONE cell: `leaf:-1` against `leaf:25`. The frame
count is the same. So the kill switch is not removing the inlined frame, and the
leaf has lost its line number.

## It is deterministic, not a JIT-timing flake

The test's own message offers "`mid` was never inlined in this run" as an
alternative reading, which would make it a coin flip — the same species as
`null_receiver_cached_invoke`, which `tools/e2e-ratchet.txt` records running 10
times for exactly this reason. It is not that:

```
5 consecutive runs, Linux, debug binary, idle-ish host:
  run1 FAILED 7.26s   run2 FAILED 7.26s   run3 FAILED 7.23s
  run4 FAILED 7.34s   run5 FAILED 7.18s
```

Seven failures in seven attempts including the original survey run, all at the
same ~7.2 s. A timing-dependent row does not do that.

## What it is NOT

**Not the 2026-09-09 reflection-caller change.** That was the change in flight
when this was found, so it was tested rather than argued away: with the
`ReflectionFactory` entries removed from `REFLECTION_INTERNAL_EXCEPTIONS` and
the VM rebuilt, the test still fails, twice, at 6.96 s and 7.01 s. The reflection
caller walk and the JIT inline-frame map are different subsystems and the
measurement agrees.

**Not a missing fixture.** The sibling failure in the same sweep
(`wp2_5_proxy`) IS one — `apps/proxy_probe/ProxyProbe.class` is gitignored and
absent from a fresh checkout, and `CRATONVM_REQUIRE_E2E=1` correctly converts
that skip into a failure. This one produces real frame data and fails an
assertion about it.

## Where to look

Not bisected, so this is a lead and not a verdict. `jit/src/x64/inlining.rs` —
one of the two halves the assertion names — was last touched by:

```
c798aae82  2026-09-08  jit: the optimizing tier spliced callee bodies and gave
                       their frames ...
3960a6a77  2026-09-09  jit: a `getstatic` in a callee cost the optimizing tier
                       the inline, and the calls a splice ...
```

Both are on `dev`, both are about splicing callee bodies and the frames that
result, and the failing assertion is about whether a spliced frame can be
suppressed. Whoever picks this up: build at `c798aae82^` and run the target
there first — that is one build and it converts this paragraph into an answer.

## Reproducing

```
JAVA_HOME=<jdk25> PATH=<jdk25>/bin:$PATH \
CRATONVM_BIN=target/debug/cratonvm CRATONVM_REQUIRE_E2E=1 \
cargo test -p cratonvm-vm --test stack_trace_across_tiers
```

`CRATONVM_REQUIRE_E2E=1` matters: it guards the BINARY only, and a missing
`javac` still turns e2e tests into a bare return that reports `ok` in 0.00 s.
Put a JDK's `bin` on `PATH` and check the clock — this target's real time is
~7 s.
