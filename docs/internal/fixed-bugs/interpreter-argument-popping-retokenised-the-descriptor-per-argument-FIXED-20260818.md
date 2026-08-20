# Popping N call arguments re-tokenised the descriptor N times — FIXED 2026-08-18

**Status:** FIXED, with a measured and deliberately modest claim: ~9% on wide
calls, neutral on narrow ones. `CRATONVM_JIT_NO_PARAM_TAG_SCAN=1` opts out.

**Reproducer / instrument:** `probes/InvokeFrameCostProbe.java`.

## How this was found, and what it is NOT

The 2026-08-18 interpreter audit fixed five instances of one shape — the
interpreter re-deriving per execution an answer fixed per call site — all of
them in single opcodes. `probes/InterpInvokeCostProbe.java` then established
that INVOCATION, which that audit never measured, is where the remaining gap
lives: against HotSpot's interpreter CratonVM runs `iadd` at 4.1x but
invocations at 12-29x.

The entry point to this investigation was a **structural** observation:
`InvokeCache` is an `FxHashMap` keyed by `(ClassId, u16, bool)` that clones its
entry on every hit, where the site caches the audit added are direct-mapped
arrays. That observation is still unproven and is **not** what this change
fixes. It was set aside because a decomposition measurement pointed elsewhere
first — and because a structure-based inference had already produced one
withdrawn claim that session.

## The decomposition

`InvokeFrameCostProbe` varies ONE property of the callee at a time, with
HotSpot as the control for callee-body cost (the arms are differenced against
their own base loop, which removes the loop but NOT the callee body, so the
absolute numbers are body-inclusive and only the ratios mean anything).

| arm | HotSpot `-Xint` | CratonVM `--nojit` | ratio |
|---|---|---|---|
| flat callee | 15.3 | 240.5 | 15.7x |
| deep callee (60 locals) | 152.4 | 1593.4 | 10.5x |
| wide operand stack | 72.2 | 773.5 | 10.7x |
| **per extra argument** | **~0.94** | **~32.3** | **34x** |
| `iadd` (control) | 4.2 | 17.0 | 4.1x |

**Two hypotheses rejected.** Frame locals-init: `deep/flat` is 6.6x on CratonVM
against HotSpot's 10.0x, so declaring 60 locals costs CratonVM proportionally
LESS — not a CratonVM-specific cost. Operand-stack depth: same reading.

**Two convicted.** A fixed per-call cost of ~206ns against HotSpot's ~6.9ns
(~30x), and ~32ns per argument against ~0.94ns (34x). On a 4-argument call the
arguments alone were ~38% of the invoke.

## The defect

`nth_param_tag_byte(descriptor, n)` answers for ONE parameter index and rescans
the descriptor from `(` every time. Every caller in the tree is a per-ARGUMENT
loop. Popping N arguments therefore cost **O(N^2)** tokenising of a string that
is fixed per call site.

Ten call sites: `dispatch_virtual` (3), `dispatch_static` (2), `field_access`
(2), `invoke` (1), `jit_bridge` (2).

The fix already existed one variant away. `CachedInvokeTarget::Intrinsic`
carries `param_descs`, described in its own field doc as "parameter
descriptors, split ONCE at IC-fill time … without re-parsing the descriptor
string". The bytecode variants never got it.

`ParamTags::of` does that job with one forward scan into an inline array,
hoisted out of each argument loop. Per call rather than per IC-fill, because
`CachedBytecodeMethod` cannot take a new field without touching its 38 struct
literals across four crates, none of which has a `..` tail — a constraint its
own `method_index` doc already records.

## Why this is safe

A wrong tag byte is **silent**. A category-2 `long`/`double` argument popped
down the category-1 path loses its high bits and produces a plausible number —
exactly the failure these call sites were written to prevent (BC safegcd
`0xFFFC_…` accumulators, `gaps/bc-ec-mod-mododdinverse-investigation.md`).

So `ParamTags::get` is pinned to `nth_param_tag_byte` by an equivalence test
over 37 descriptors x 64 indices: primitives, objects, nested arrays,
truncated/malformed descriptors, and 15/16/17/40-parameter shapes so the
overflow fallback is exercised rather than assumed. Indices are probed past the
parameter count because the dispatch arms index by argument SLOT.

**The test was verified by breaking what it guards.** Disabling the overflow
fallback fails it at `"(IIIIIIIIIIIIIIIII)V"` index 16 — the exact boundary.

## The first version was a pessimisation, and the totals hid it

Measured on one binary against its own kill switch, eight paired rounds:

* per-argument cost **DOWN 21%** (35.5 -> 27.9 ns/arg), **8/8 rounds**
* fixed per-call cost **UP ~11ns**; zero-argument calls regressed in **7 of 8**

Break-even at ~1.5 arguments — a pessimisation for the 0-2 argument calls that
dominate real Java. **The per-call totals looked like a wash precisely because
the two effects cancelled.** Reading the headline number alone would have
merged a regression for narrow calls with a plausible O(N^2) story attached.
Isolating the per-argument cost as `args8 - args0` — which cancels the fixed
cost — is what separated them.

The +11ns was the struct, not the scan: `[b'L'; 16]` plus a 24-byte return on
every call. `INLINE` 16 -> 8 and `#[inline]` on the accessors.

Re-measured, six paired rounds, arm order alternating:

| | verdict |
|---|---|
| per-argument cost | NEW better **6/6**, 39.9 -> 34.1 ns/arg (**-15%**) |
| `args8` total | NEW better **6/6**, ~547 -> ~499 (**-9%**) |
| `args4` total | wash (2 better, 3 worse, 1 same) |
| `args0` fixed cost | wash (3 better, 3 worse) — was 7/8 WORSE |

Stated deliberately as **~9% on wide calls, neutral on narrow ones**. This is
not a general invoke speedup, and the benefit sits in the higher-arity calls
that are the less common shape.

## A standing verdict re-tested rather than inherited

`MethodSiteCache` is opt-in and default-OFF, with a note that it "measures
nothing". That note was never re-tested, and the field-site cache had already
turned out the opposite way under a note of the same kind.

A/B'd here on the workload that should favour it most — a pure invoke
microbenchmark with varying arity — it is a wash or slightly worse in 3 of 4
rounds. Its engagement counter explains why: **2,753 hits across ~2.4M calls**,
because `pop_coerced_invoke_args_*` is not the hot argument path. The verdict
stands, now on evidence.

## What this leaves

The **fixed ~206ns per call** against HotSpot's ~6.9ns is untouched and is the
bigger target — it is paid by every invoke kind, including `invokestatic`,
which has no receiver, no vtable and no inline-cache receiver guard and still
costs 170ns. The `InvokeCache` hashmap-and-clone observation belongs to that
budget and remains unmeasured; it needs a one-binary A/B, not another reading
of the code.
