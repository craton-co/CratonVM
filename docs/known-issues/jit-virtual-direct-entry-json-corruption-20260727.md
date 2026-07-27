# JIT: enabling the compiled-callee direct entry re-introduced rare parse corruption (~1 per 500k json-smart parses)

**Status:** OPEN. Attributed to `4f280090f`
("fix(jit,concurrent,nio): compiled callers never reached compiled callees"),
which flipped `direct_virtual_compiled_callee_entry_enabled()` to default-ON on
2026-07-27. Workaround:
`CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0`.

Found while retiring the json-smart JIT ban
(`docs/internal/jsonsmart-parser-jit-retired-20260727.md`); the ban itself
stays retired — the corruption is in the JIT's virtual-dispatch inline cache,
not in `net/minidev/json/parser/`.

## Evidence

`docs/known-issues/repros/jsonsmart/JsonSmartProbeWarmed.java`, json-smart
2.6.0 + real JDK 25, 10 documents per iteration, parse -> serialize -> re-parse
round trip. Each 150,000-iteration run is 1,500,000 parse operations.

| Build | Config | Ops | Errors |
|---|---|---|---|
| `871fb7563` (dev `a7a5d6ff5` + the elidable-ctor fix) | default | 1,500,000 | **0** |
| same | `CRATONVM_JIT_THRESHOLD=1` | 300,000 | 0 |
| `92cd37b78` (that merged with dev `a80673ad0`) | default | 1,500,000 | **3** |
| same | default, second run | 1,500,000 | **2** |
| same | `CRATONVM_JIT_THRESHOLD=1` | 300,000 | 1 |
| same | `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0` | 1,500,000 | **0** |
| same | `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0`, second run | 1,500,000 | **0** |
| same | `CRATONVM_JIT_DENY=net/minidev/json/parser/` | 1,500,000 | **0** |

5 errors in 3,000,000 operations with the direct entry on, 0 in 3,000,000 with
it off (Poisson p ~ 0.007), and 0 in 1,500,000 with the parser package kept
interpreted — so the corruption needs BOTH compiled parser code and the direct
entry.

Two failure shapes, both from `JSONParser.parse(String)` on a document it
parses correctly millions of other times:

```
ROUNDTRIP MISMATCH at iter=47981
  doc={"a":1,"b":2.5,"c":"hello","d":true,"e":null,"f":[1,2,3]}
  rt1={"a":1,"b":2.5,"c":"hello","d":true,"e":null,"f":[1,2,3]}
  rt2="d"                       <-- re-parse returned a KEY STRING, not the map

EXCEPTION at iter=96059 doc={"arr":[...],"count":3,"ok":true}
  net.minidev.json.parser.ParseException: Unexpected token  at position 100.
```

`rt2="d"` / `rt2="e"` / `rt2="ok"` is the informative one: the parse returned
one of the document's own keys — i.e. a call inside the parser returned another
method's result. That is the shape of a virtual call reaching the wrong
compiled body, not of a corrupted character cursor.

## Why this is the direct-entry path

`direct_virtual_compiled_callee_entry_enabled()` gates the only write of
`JitMICSlot::cached_entry_ptr` (and the megamorphic PIC secondary cache). Until
`4f280090f` it was default-OFF, so every virtual call out of compiled code fell
back into the interpreter and the cached entries were never used. Flipping it
on is a large, real throughput win (the commit measures 5582ms -> 169ms on an
H2 loop) — and it also made a latent inline-cache defect reachable.

The parser's hot virtual sites are exactly the megamorphic kind this cache
targets: `JSONParserBase` calls `mapper.createObject()`, `mapper.addValue()`,
`mapper.convert()` … on `net/minidev/json/writer/JsonReaderI` receivers with
many concrete subclasses.

Prime suspect (read, not yet proven): a cached entry pointer that outlives the
artifact it points into.

`JitCache::invalidate_matching` is careful — it computes the transitive reverse
closure of removed bodies and walks every cached artifact's
`_jit_mic_slots`/`_jit_pic_slots` to clear entries pointing at them — and both
slot kinds keep a strong owner (`resolve_jit_entry_owner(entry_ptr)` stored in
`compiled_owner` / `compiled_owners[i]`) so a cached callee stays alive. Two
gaps in that argument are worth checking first:

1. ~~`resolve_jit_entry_owner` returning `None` at install time (the entry is
   not findable in the cache at that instant) leaves the slot holding a RAW
   entry pointer with no owner.~~ **MEASURED AND REFUTED.** A temporary
   instrumentation of both install sites (`JitMICSlot` and `JitPICSlot`) over a
   3,000-iteration probe run logged 24 owner-less installs, and every one of
   them had `entry=0x0` — i.e. they are the documented receiver-class HINT
   installs that carry no compiled entry at all. No install with a real entry
   pointer was unowned.
2. Tier-up supersession that replaces a cache entry in place, rather than going
   through `invalidate_matching`, never runs the slot-clearing walk — callers
   keep dispatching to the superseded body.

Either way the failure needs the pointed-to memory to be reused by a DIFFERENT
method to produce "call returned another method's result"; note that the file's
own round-8 plan (`jit/src/lib.rs`, `JitCache` docs) describes a code arena
with a free-list "keyed by 256-byte-rounded size buckets so deopt-invalidated
methods return space for reuse", i.e. exactly that reuse. The observed rate
(~1 in 500k operations, timing-dependent rather than input-dependent) fits a
tier-up/recompile race, not a deterministic lowering bug.

## Reproduction

```bash
JS=<...>/json-smart-2.6.0.jar; AS=<...>/accessors-smart-2.6.0.jar; ASM=<...>/asm-9.10.1.jar
TMPDIR=/data/tmp <cratonvm> --java-home <jdk25> -Xmx1g \
  -cp "<classdir>:$JS:$AS:$ASM" JsonSmartProbeWarmed 150000
```

Expect 2-3 failures per run today, 0 with
`CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0`. Budget at least
1,500,000 operations per configuration: at ~1 error per 500,000 ops, a
300,000-op run proves nothing.

## Next steps

1. Audit every `cached_entry_ptr` / PIC `mega_entry_ptrs` install and lookup
   against the JIT generation + supersede epoch, the same way the Rust-side
   dispatch caches already are.
2. Add the json-smart round trip (or an equivalent megamorphic-interface
   workload) to whatever suite guards this feature; the existing
   `vm/tests/jit_interp_differential.rs` kernels do not exercise a
   many-subclass interface call site under tier-up.
3. Until then, treat `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0` as
   the correctness-first setting for JSON/interface-heavy workloads.
