# Interpreter operand-stack slot reads stale after a nested allocating call — the residual core of the WildFly cid=0 family

Status: OPEN — precisely characterized 2026-07-22 with live slot-level captures; non-fatal in every
current observation (the all-zero-header invokevirtual detector's CP fallback heals the dispatch and
boots complete). This is the REMAINING mechanism after
`fix/wildfly-remoting-cce-close-20260722` closed every native-side producer of the
`parallel-extension-add` stale-ref family (10 producers; see
`docs/known-issues/wildfly-remoting-classcastexception-parallel-extension-add.md`).

## The discriminating capture

fix6 campaign (all native producers fixed, binary md5-verified), `CRATONVM_DBG_STALE_RECV=1`:

```
[stale-recv] ptr=0x20019dd2510 method=java/lang/StringBuilder.append(Ljava/lang/String;) — Java frames:
  [64] org/infinispan/marshall/core/impl/ClassToExternalizerMap.toString() pc=118
      LOCAL[0] -> 0x2001e08b4a8 all_zero_header=false
      LOCAL[1] -> 0x2001e08b520 all_zero_header=false        ← every LOCAL healthy
  ...
[stale-recv] ptr=0x20019dd2510 method=java/lang/StringBuilder.append(I) — same frame pc=122
```

- The stale receiver appears in NO local of any frame — it exists only as the operand-stack value
  consumed by the dispatch.
- The SAME stale address is consumed at two consecutive append call sites (pc 118 then 122) in a
  javac `sb.append(x).append(y)` chain — i.e. the value was pushed once (aload/chained return),
  survived a nested GC-capable evaluation (argument expressions allocate), and both consumers saw
  the pre-move address.
- Every native producer that could have handed the value back stale is fixed and verified on this
  binary (the append natives return pin-refreshed `this`; `cargo test` clean; the doc's campaign
  history shows each earlier producer's signature at 0 post-fix).

Conclusion: a reference sitting on an INTERPRETER OPERAND STACK across a nested allocating call can
read back stale — the frame's stack slot missed the moving-GC root scan/remap in some window. Locals
in the same frame were remapped correctly in every capture, so the gap is specific to stack slots
(or to a stack-slot tagging state — e.g. a CompactValue variant the scanner classifies as
non-reference).

## Why this is the old "cid=0 menagerie" core

This mechanism produces exactly the historical symptom set the retired
`wildfly-standalone-boot-attributeaccess-cce-register-invisible-root-RETIRED.md` family chased for
weeks: an arbitrary consumer (checkcast/invoke on whatever type that code expected) observing a
zeroed/reused block that identifies as bare `java.lang.Object` (ClassId 0), at ~0.5-2%/boot rates
under parallel-extension-add's allocation storm, JIT-independent, per-site pin fixes never moving
the rate. Every NATIVE-side member of the family is now closed; what remains is this interpreter
frame-scan window.

## Current impact

Non-fatal in all fix5/fix6 observations: the invokevirtual stale-receiver detector heals via CP
fallback and boots reach completion (`WFLYSRV0025` with the 2026-07-22 console-logging fix). The
residual risk is (a) the healed path still OPERATES on the stale object (writes land in reclaimed
memory — the plausible mechanism of the open h2 `StringBuilder.append(long)` NaN corruption), and
(b) consumers without a healing path (checkcast) turn it into the family's fatal CCE at very low
residual rates.

## Pickup

- Tooling is all landed and env-gated: `CRATONVM_DBG_STALE_RECV=1` (frame + slot provenance dump,
  optionally + `CRATONVM_DBG_A2` allocation history), `CRATONVM_DBG_CCE_BT=1` (checkcast +
  nsme_dispatch stack dumps). The `xargs -P 10` isolated-boot harness in
  `/data/wt-remoting-cce-close-20260722/probes/` reproduces captures within ~30-100 attempts.
- Start from the interpreter frame stack scanner (the moving young collector's root walk over
  `thread.frames[*].stack`) and audit which slot states are classified non-reference; correlate
  with the CompactValue tag transitions used by the K2/T10.9.E direct-push fast paths.
- This belongs with the precise-maps/frame-scan roadmap
  (`docs/feature-designs/precise-jit-maps-default.md`), not with another native sweep — the native
  surface is done.
