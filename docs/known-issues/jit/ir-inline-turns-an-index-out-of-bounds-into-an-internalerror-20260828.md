# `CRATONVM_JIT_IR_INLINE` turns an `IndexOutOfBoundsException` into an `InternalError`

## Status

**OPEN, 2026-08-28, and it blocks the flag's default-on flip.** Found by the
gauntlet soak (`performance/ir-inline-gauntlet-soak-20260828.md`).

Deterministic — 5 reps per arm, serial, one binary, nothing but the flag
changing:

| arm | result |
|---|---|
| `CRATONVM_JIT_IR_INLINE` unset | `found=416 started=416 ok=416 failed=0` ×5 |
| `CRATONVM_JIT_IR_INLINE=1` | `found=416 started=416 ok=415 **failed=1**` ×5 |

```
@@TESTFAIL io.netty.buffer.DuplicatedByteBufTest getMediumBoundaryCheck2() FAILED
org.opentest4j.AssertionFailedError: Unexpected exception type thrown,
  expected: <java.lang.IndexOutOfBoundsException> but was: <java.lang.InternalError>
    at io.netty.buffer.AbstractByteBufTest.getMediumBoundaryCheck2(AbstractByteBufTest.java:335)
```

The test asks for a 3-byte read past the end of the buffer and asserts it throws
`IndexOutOfBoundsException`. With the flag on it throws this instead:

```
java.lang.InternalError: JIT dispatch into
  io/netty/buffer/UnpooledHeapByteBuf._getUnsignedMedium(I)I failed:
  internal error: precise deoptimization unavailable for
  io/netty/buffer/UnpooledHeapByteBuf._getUnsignedMedium(I)I at bci 36
  (can_deopt_resume=false (no deopt points, or an elided monitor),
   stashed key "io/netty/buffer/UnpooledHeapByteBuf._getUnsignedMedium:(I)I",
   inline callers 0, reason UnreachedCode); refusing side-effecting replay
```

## Why it is the inliner

The method the error names is the method the flag splices into, and the flag is
the only thing that puts it there:

```
[ir] inline-plan io/netty/buffer/UnpooledHeapByteBuf._getUnsignedMedium:
       1 site(s), 1 spliced body, 34 bytes appended
```

With the flag off there is no `inline-plan` line for it and the test passes.

## The mechanism, as far as the message names it

`reason UnreachedCode` — the compiled body reached a path it was compiled to
treat as unreachable and asked to deoptimize. `can_deopt_resume=false` — the
artifact offers no precise resume, so the VM refuses to replay a
possibly-side-effecting method and raises `InternalError` rather than letting
the interpreter run the path and produce the ordinary Java exception.

That combination is the bug: the spliced body has a live path that requests a
deopt, and the artifact it lives in cannot service one. Either the path should
not be reachable, or the artifact must be able to resume at it.

Note which half is NOT implicated. The design note's standing caveat — "a call
left inside a spliced body is re-executed on a deopt" — is fenced twice and
cannot fire: `resolve_ir_inline_site` refuses `idiv`/`irem`/`ldiv`/`lrem`, the
div-zero guard is the only guard `IrBuilder` emits, and `splice_guard_seen`
refuses the whole graph if a guard is ever built inside a splice
(`ir.rs`, checked at the end of `build`). So no `Op::Guard` deopt point exists
inside a spliced region. This deopt is not one of those; it is an
`UnreachedCode` request from the ordinary lowering, in a method that happens to
have been given a spliced body.

## Reproducing

On the Azure host, where the netty reactor is built:

```bash
cd /data/cratonvm/apps/netty-suite-runner
echo io.netty.buffer.DuplicatedByteBufTest > /tmp/dup.txt

# green
./run-netty-suite.sh --list /tmp/dup.txt --bin <cratonvm> --out /tmp/dup-off --shards 1

# red, every time
CRATONVM_JIT_IR_INLINE=1 \
  ./run-netty-suite.sh --list /tmp/dup.txt --bin <cratonvm> --out /tmp/dup-on --shards 1

grep -rh '@@RESULT' /tmp/dup-off /tmp/dup-on
grep -rh 'InternalError' /tmp/dup-on | grep -v '^\s*at '
```

`--shards 1` matters. Under 5-way sharding this class fails in BOTH arms
(contention), and the difference is invisible: the 200-class sharded A/B
reported PASS 112 / FAIL 58 in both arms and *zero* verdict differences. The
serial run is what separated them.

## Scope

One test in 200 netty classes, and the only correctness difference the soak
found. Everything else was clean — see the soak record for the full table. But
it is a user-visible wrong exception type on an ordinary bounds-check path, it
is deterministic, and `_getUnsignedMedium` is not an exotic shape: it is a
three-`getByte` accessor, which is exactly the shape the inliner admits.

## Related

- `performance/ir-inline-gauntlet-soak-20260828.md` — the soak
- `docs/jit/ir-tier-inlining.md` — the design note, its admission set and opens
