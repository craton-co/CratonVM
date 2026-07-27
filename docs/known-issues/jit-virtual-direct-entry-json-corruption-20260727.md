# JIT: rare json-smart parse corruption on the compiled-callee direct-call path — needs a MOVING young generation

> **RESOLVED later the same day — the corruption is gone once the RBC.6
> precise-handler-frame relaxation is gated off.** A `git bisect` over the
> 27-commit merge range, using `-Xmx64m` (which turns "1 error per 1,500,000
> ops" into "first error by iteration 5,000" and makes each step a 2-minute
> test), lands on `83e078aa5` — not on `4f280090f`. That commit lets a method
> whose exception handler reads a non-parameter local be compiled, and the
> frame it reconstructs drops live locals; a reference dropped there is a root
> the collector never sees, which is exactly why the failure needs a MOVING
> young generation, as measured below. With the gate closed: 3 x 200,000 ops at
> `-Xmx64m` and 1,500,000 ops at the default heap, 0 errors. Evidence, the
> liveness bug fixed underneath it, and what is still open in that feature:
> `docs/known-issues/jit-precise-handler-frame-drops-live-locals-20260727.md`.
> The analysis below (especially the `CRATONVM_MOVING_YOUNG=0` row and the
> refutations) is what made that reading possible — it is kept as written.


**Status: OPEN**, but re-characterized on 2026-07-27. It is **not** a stale
inline-cache entry and **not** the NodeConnections SIGSEGV; both of those
hypotheses were tested and refuted (below). It is a GC defect: the corruption
disappears entirely when the young generation does not move.

Still attributed to `4f280090f` ("fix(jit,concurrent,nio): compiled callers
never reached compiled callees"), which flipped
`direct_virtual_compiled_callee_entry_enabled()` default-ON — that is what makes
the dispatch helper compile a callee and CALL its entry directly instead of
routing through `invoke_or_native`. Correctness-first workaround remains
`CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0`.

Found while retiring the json-smart JIT ban
(`docs/internal/jsonsmart-parser-jit-retired-20260727.md`); the ban itself stays
retired — nothing here is specific to `net/minidev/json/parser/`.

## Symptom

`docs/known-issues/repros/jsonsmart/JsonSmartProbeWarmed.java`, json-smart 2.6.0
+ real JDK 25, 10 documents per iteration, parse → serialize → re-parse. Each
150,000-iteration run is 1,500,000 parse operations.

```
ROUNDTRIP MISMATCH at iter=47981
  doc={"a":1,"b":2.5,"c":"hello","d":true,"e":null,"f":[1,2,3]}
  rt1={"a":1,"b":2.5,"c":"hello","d":true,"e":null,"f":[1,2,3]}
  rt2="d"                       <-- re-parse returned a KEY STRING, not the map

EXCEPTION at iter=96059 doc={"arr":[...],"count":3,"ok":true}
  net.minidev.json.parser.ParseException: Unexpected token  at position 100.
```

`rt2` being one of the document's own keys reads as "a reference that should
point at the result map points at some other live object instead" — an
aliasing/relocation failure, which the evidence below now supports directly.

## What it needs, measured

All rows on the 2026-07-27 build that already carries the NodeConnections
callee-artifact pin fix, so none of this is the stale-entry bug.

| config | ops | errors |
|---|---|---|
| default (`-Xmx1g`) | 1,500,000 | 2 |
| default (`-Xmx1g`), second run | 1,500,000 | 1 |
| **`-Xmx8g`** | 1,500,000 | **0** |
| **`-Xmx8g`**, second run | 1,500,000 | **0** |
| **`CRATONVM_MOVING_YOUNG=0`** (`-Xmx1g`) | 1,500,000 | **0** |
| `-Xmx256m` | 600,000 | 0 |
| `CRATONVM_JIT_POISON_FREE=1` | 1,500,000 | 3, **no SIGSEGV** |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` | 1,500,000 | 1 |
| `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0` (earlier build) | 3,000,000 | 0 |
| `CRATONVM_JIT_DENY=net/minidev/json/parser/` (earlier build) | 1,500,000 | 0 |

Reading those together:

* **It needs a moving young generation.** 3 errors per 3M ops at `-Xmx1g`
  versus 0 per 3M at `-Xmx8g` and 0 per 1.5M with `CRATONVM_MOVING_YOUNG=0` at
  the *same* 1 GiB heap. The `MOVING_YOUNG=0` row is the load-bearing one: at an
  unchanged heap it isolates *relocation* rather than *collection*. The rate is
  NOT monotonic in collection frequency — `-Xmx256m`, which collects far more
  often, is also clean over 600,000 ops — so the trigger is a particular
  young-gen regime (big enough to evacuate rather than fall back), not simply
  "more collections". Whatever narrows that regime is also the cheapest
  amplifier available.
* **It is not the inline machine-code MIC/PIC cascade.**
  `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` stops that cascade from being emitted at
  all and the corruption survives. What remains on that path is the *dispatch
  helper* calling a compiled callee directly via
  `try_call_compiled_entry_reentrant`.
* **It is not a stale or recycled code buffer.** See the refutation below.

So the shape is: a compiled caller → `jit_invoke_virtual_mic` →
`try_call_compiled_entry_reentrant` → compiled callee, with a young-gen
relocation somewhere inside the callee, after which the caller (or the helper)
uses a reference that was not updated.

`try_call_compiled_entry_reentrant` already registers a
`JitEntryGuard::enter_with_compiled` for the nested call — but only when
`lookup_jit_code_range(entry)` resolves, i.e. only when the code-range registry
is populated (`precise_jit_maps_enabled() || xt_jit_root_scan_enabled()`, both
default-on today). Whether that guard actually publishes the *helper's own*
Rust-frame references, and what the collector does with the caller frame
underneath it, is the first thing to audit.

## Refuted — do not re-litigate without new evidence

**"Almost certainly the same bug as the NodeConnections SIGSEGV."** That doc
(now closed: `docs/internal/nodeconnections-retired-jit-code-jump-20260727.md`)
proposed the deciding experiment itself — run this probe under
`CRATONVM_JIT_POISON_FREE=1`, which retires code with `mprotect(PROT_NONE)` and
never unmaps or recycles an address, so a call into a retired body becomes an
immediate SIGSEGV instead of a silent wrong answer. **It does not crash**, and
the error rate is unchanged (3 per 1.5M). The corruption is therefore not a call
into retired or recycled code. It also survives the fix that closed that crash
(handing `try_jit_compile_callee`'s callers the `Arc<CompiledMethod>` instead of
a bare entry address) at an unchanged rate.

**Inline-cache slots holding an unowned entry.** Both `JitMICSlot::update` and
`JitPICSlot::write_entry`/`install_megamorphic` now refuse to publish an entry
they cannot retain (`jit_entry_publishable`), and
`CRATONVM_DBG_JIT_STALE_IC=1` reports zero unowned publications and zero live
slots pointing at a retiring body across full ElasticSearch test runs.

**A cached entry pointer that outlives its artifact** (this doc's original prime
suspect) — same evidence as above.

## Next steps

1. Audit the GC root publication across
   `jit_invoke_virtual_mic` → `try_call_compiled_entry_reentrant` → compiled
   callee under a *moving* young generation: which frame's oop map covers the
   helper's live references (`class_name`, the decoded receiver, `args_slice`)
   while the nested callee runs, and whether the caller's spill slots are
   remapped when the callee triggers a young collection.
2. Amplify: the probe reproduces at ~1 per 750,000 operations at `-Xmx1g`.
   A smaller heap (or `CRATONVM_DBG_FORCE_MOVING`) should raise the rate enough
   to make single-run A/Bs meaningful; budget at least 1,500,000 operations per
   configuration otherwise.
3. Until then, `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0` remains the
   correctness-first setting for interface-heavy workloads, and
   `CRATONVM_MOVING_YOUNG=0` is an equally effective (and more targeted)
   workaround.

## Reproduction

```bash
JS=<...>/json-smart-2.6.0.jar; AS=<...>/accessors-smart-2.6.0.jar; ASM=<...>/asm-9.10.jar
TMPDIR=/data/tmp <cratonvm> --java-home <jdk25> -Xmx1g \
  -cp "<classdir>:$JS:$AS:$ASM" JsonSmartProbeWarmed 150000
```
