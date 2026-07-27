# JIT: rare json-smart parse corruption on the compiled-callee direct-call path — needs a MOVING young generation

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

All rows below are on builds that already carry the NodeConnections
callee-artifact pin fix, so none of this is the stale-entry bug. The rate is
about **1.3 errors per 1,000,000 parse operations**, so a single 1,500,000-op run
is worth ~2 expected errors and observing 0 in one run means almost nothing
(p ≈ 0.13). **Budget 4.5M–6M operations per configuration**; the earlier
"1 per 500k" figure came from a higher-rate build and over-promised.

| config | ops | errors |
|---|---|---|
| default (`-Xmx1g`), 2026-07-27 branch build | 6,000,000 | 8 |
| default (`-Xmx1g`), same day's `dev` tip | 6,000,000 | 2 |
| **`CRATONVM_MOVING_YOUNG=0`** (`-Xmx1g`) | **4,500,000** | **0** |
| `-Xmx8g` | 3,000,000 | 0 |
| `-Xmx256m` | 600,000 | 0 |
| `CRATONVM_JIT_POISON_FREE=1` | 1,500,000 | 3, **no SIGSEGV** |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` | 1,500,000 | 1 |
| `CRATONVM_NO_PRECISE_JIT_MAPS=1` | 1,500,000 | 0 — *underpowered, inconclusive* |
| `CRATONVM_NO_PRECISE_JIT_MAPS=1 CRATONVM_XT_JIT_ROOT_SCAN=0` | 1,500,000 | 0 — *underpowered, inconclusive* |
| `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0` (earlier build) | 3,000,000 | 0 |
| `CRATONVM_JIT_DENY=net/minidev/json/parser/` (earlier build) | 1,500,000 | 0 |

Reading those together:

* **It needs a moving young generation.** `CRATONVM_MOVING_YOUNG=0` is clean
  over 4,500,000 operations on the *same binary and the same 1 GiB heap* whose
  control rate predicts ~6 errors (Poisson p ≈ 0.003). That row, not the
  heap-size rows, is the load-bearing one: it isolates *relocation* rather than
  *collection*. The rate is not monotonic in collection frequency either —
  `-Xmx256m` collects far more often and is clean — so the trigger is a
  particular young-gen regime (large enough to evacuate rather than fall back),
  which is also the cheapest amplifier available for further work.
* **It is not the inline machine-code MIC/PIC cascade.**
  `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` stops that cascade from being emitted at
  all and the corruption survives. What remains on the path is the *dispatch
  helper* calling a compiled callee directly via
  `try_call_compiled_entry_reentrant`.
* **It is not a stale or recycled code buffer.** See the refutation below.
* The two root-scan rows are listed only so nobody re-runs them at the same
  size and mistakes the result for a signal — at 1.5M ops each they cannot
  distinguish "fixed" from "unchanged".

So the shape is: a compiled caller → `jit_invoke_virtual_mic` →
`try_call_compiled_entry_reentrant` → compiled callee, with a young-gen
relocation somewhere inside the callee, after which a reference that was not
updated is used. Returning one of the document's own keys instead of the parsed
map is what an aliased-after-evacuation reference looks like.

`try_call_compiled_entry_reentrant` already registers a
`JitEntryGuard::enter_with_compiled` for the nested call — but only when
`lookup_jit_code_range(entry)` resolves, i.e. only when the code-range registry
is populated (`precise_jit_maps_enabled() || xt_jit_root_scan_enabled()`, both
default-on today). Whether that guard publishes the *helper's own* Rust-frame
references, and what the collector does with the compiled caller's frame
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
2. Amplify before anything else. At ~1.3 per 1,000,000 operations every A/B
   here costs half an hour to reach even marginal significance. `-Xmx256m` is
   *not* the amplifier (it is clean); find the young-gen sizing that maximises
   moving evacuations at a 1 GiB heap, or use `CRATONVM_DBG_FORCE_MOVING`.
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
