# JSONSMART-PARSER.1 (`net/minidev/json/parser/`) — RETIRED 2026-07-27: ban gone, package JIT-compiles clean, one real residual found and fixed

**Status:** ✅ CLOSED for the ban itself; see the post-merge addendum below for
a separate, newly-landed JIT dispatch-cache bug that this probe now catches.
The ban is removed from `vm/src/jit/skip_list.rs` (it was
removed on 2026-07-26 by a concurrent session and is still removed), the parser
package JIT-compiles, and 3,000,000 parse/serialize/re-parse operations under
JIT produce **0 errors**. The one real defect this re-investigation turned up
was **not** in the parser at all — it was a VM-wide JIT bug (a native-shadowed
constructor being elided) that json-smart merely exposed. That bug is fixed
here.

Supersedes `docs/known-issues/jit-bans/jsonsmart-parser-still-needed.md`
("CONFIRMED still needed: ~20% corruption rate under JIT", 2026-07-26).

## What the old doc claimed, and what is true now

The 2026-07-26 writeup reported a ~20% error rate (239,605 / 1,200,010 ops)
with `CRATONVM_JIT_ALLOW_PACKAGES=net/minidev/json/parser/`, with unstable,
input-independent `ParseException` messages — the signature of a live-state
miscompile. On 2026-07-27 dev (`a7a5d6ff5`) none of that reproduces:

| Run (json-smart 2.6.0, real JDK 25) | Result |
|---|---|
| `JsonSmartProbe` (the old doc's own repro), 300k iterations = 3,000,000 ops | **0 errors** |
| `JsonSmartProbeWarmed`, 300k iterations = 3,000,000 ops, parser fully JIT-compiled | **0 errors** |
| Same, `CRATONVM_JIT_THRESHOLD=1` (compile everything immediately) | **0 errors** |
| 40 × short runs (60 iterations each, fresh VM, hits the tier-up window) | **0 failures** |

`JSONParserString.read()`, `JSONParserString.readS()` and
`JSONParserBase.skipSpace()` — the three methods the ban comment named — are
confirmed JIT-compiled in these runs (`CRATONVM_DBG_DUMP_JIT=LIST`), so this is
not a "no corruption because nothing compiled" result for them. Between the old
doc and today, dev gained the two `java.lang.String` intrinsic layout fixes
(`13055f75c`, `234a45b98`, both 2026-07-26 — compact-layout primitive offsets:
`coder` was being read out of `hash`'s slot), which is the most likely reason
the parser's character-cursor methods stopped miscompiling.

## The residual that WAS real: JIT-created HashMaps had 32 buckets, not 16

The stress probe still tripped roughly once per 250,000 parses — always the
same shape, and never a corrupted value:

```
ROUNDTRIP MISMATCH at iter=20
  doc={"empty_str":"","empty_arr":[],"empty_obj":{}}
  rt1={"empty_obj":{},"empty_str":"","empty_arr":[]}
  rt2={"empty_obj":{},"empty_arr":[],"empty_str":""}
```

Both maps hold the same three keys inserted in the same order, so HotSpot
iterates them identically (3,000,000 ops: 0 mismatches). The two CratonVM
orders are exactly the 16-bucket and the **32**-bucket orders for those keys —
i.e. one of the maps had a table twice the size it should have.

Bisection (no rebuilds, `CRATONVM_JIT_DENY` substring bans, 40 short runs per
configuration):

| Configuration | failures / 40 runs |
|---|---|
| default | 3 |
| `CRATONVM_DISABLE_JIT=1` | 0 |
| `CRATONVM_JIT_DENY=java/util/HashMap` | 5 |
| `CRATONVM_JIT_DENY=net/minidev/` | 0 |
| `CRATONVM_JIT_DENY=net/minidev/json/parser/` | 0 |
| `CRATONVM_JIT_DENY=JSONParserBase` | 0 |
| `CRATONVM_JIT_DENY=JSONParserBase.readObject` | 0 |

`readObject` is the method that allocates the `JSONObject`. Parsing ONE
document in a loop turned the "1 in 250k" into a 100%-deterministic repro
(`MiniJsonProbe`): correct order for iterations 0..~500, wrong order for
**every** iteration after that — i.e. from the moment `readObject` tiered up.
The full probe only misfired occasionally because it needs one map from before
the tier-up and one from after in the same round trip.

Reduced to plain JDK collections (`MapCapProbe`, no json-smart at all):

```java
static Map<String,Object> build() { Map<String,Object> m = new HashMap<>(); ...puts...; return m; }
```

flips order at iteration ~501 under CratonVM and never under HotSpot;
`new HashMap<>(16)`, `new LinkedHashMap<>()`, `new HashSet<>()` are all
unaffected. `CRATONVM_DBG=jit-dispatch` then showed the mechanism directly:
`java/util/HashMap.<init>(I)V` and `java/util/LinkedHashMap.<init>()V` are
dispatched from JIT'd code, while **`java/util/HashMap.<init>()V` never is**.

### Root cause (two defects, both fixed here)

1. **`is_elidable_construction` ignored native shadowing**
   (`vm/src/runtime/interpreter.rs`). It judged an `invokespecial C.<init>()V`
   elidable from the *bytecode body* alone (`aload_0; invokespecial
   Object.<init>; return`). But `invokespecial` prefers a registered native
   over bytecode, and `java/util/HashMap.<init>()V` is exactly that shape: an
   empty bytecode constructor plus `native_map_init`, which allocates the
   16-bucket table and initialises size/threshold. The JIT rewrote the site to
   `java/lang/Object.<init>` and the codegen dropped it, so a JIT-compiled
   `new HashMap<>()` produced a map with **no table**.
   *Fix:* a class whose `<init>()V` has a registered native is never elidable.

2. **`map_resize` doubled the capacity when MATERIALISING a table**
   (`native-collections/src/lib.rs`). `native_map_put` routes a
   `buckets == None` map through `map_resize`, which computed
   `new_cap = old_cap * 2` — and `map_state` reports `old_cap = 16` for a map
   with no table, so first-touch gave 32 buckets. HotSpot's `resize()` on a
   null table allocates DEFAULT_INITIAL_CAPACITY (16). This path is reachable
   without defect 1 too: the code's own comment cites JDK-bytecode
   `LinkedHashMap.<init>()`/`AnnotationAttributes`, which also leave `table`
   null until the first put.
   *Fix:* first-touch materialisation allocates `max(old_cap, 16)`, not
   `old_cap * 2`.

Defect 1 is the root cause of the JIT divergence; defect 2 is what turned it
into wrong iteration order rather than a silent no-op. Both are VM-wide — any
JIT-compiled method allocating a `HashMap` was affected, in synthetic-JDK mode
as well as real-JDK mode. Nothing about this was specific to json-smart; the
parser was simply a workload that builds many small maps and then re-reads them
in order.

### Regression net

`vm/tests/jit_collection_ctor_identity.rs` +
`vm/tests/resources/cratonvm/JitCollectionCtorIdentity.java`: runs the same
fixture interpreted and JIT-compiled and requires byte-identical iteration
orders for `HashMap()`, `HashMap(16)`, `LinkedHashMap()`, `HashSet()`,
`ArrayList()`, plus a round-trip order check. Verified to FAIL on the
pre-fix binary (`hashMapNoArg` order differs, `roundTripEqualOrder=false`) and
pass after.

## Post-merge addendum (dev `a80673ad0`): a NEW corruption, from the JIT dispatch cache

Everything above was measured on this branch before merging the day's dev
(`a7a5d6ff5` + the elidable-ctor fix: 1,500,000 operations, 0 errors). After
merging dev `a80673ad0`, the same probe fails again — 3 errors in 1,500,000
operations, and 2 in a second run:

```
ROUNDTRIP MISMATCH at iter=47981
  doc={"a":1,"b":2.5,"c":"hello","d":true,"e":null,"f":[1,2,3]}
  rt1={"a":1,"b":2.5,"c":"hello","d":true,"e":null,"f":[1,2,3]}
  rt2="d"          <-- the re-parse returned one of the KEYS, not the map
```

This is NOT the ban's bug returning, and not the HashMap-capacity bug fixed
here (`MiniJsonProbe`, its deterministic repro, stays clean).

**Root-caused and fixed** (same session, after the first attempt blamed the
wrong commit): running the probe under `-Xmx64m` turns "1 error per 1,500,000
operations" into "first error by iteration 5,000", and a `git bisect` over the
merged range with that repro names `83e078aa5` — which relaxed the RBC.6
admission gate so methods whose exception handler reads a non-parameter local
are compiled on the promise of a precise exceptional frame. The frames drop
live values (an unrelated object appears where a live one was), so the gate is
closed again by default. Full evidence, including the liveness bug fixed
underneath it, is in
`docs/known-issues/jit-precise-handler-frame-drops-live-locals-20260727.md`.

Disposition is unchanged: the corruption was in shared JIT frame
reconstruction, not in `net/minidev/json/parser/` — re-banning the package
would have hidden one victim of it, not fixed it.

## Known, unrelated coverage gap seen in the same runs

With no parse error ever thrown, `net/minidev/json/parser/ParseException` is
never loaded, and every `JSONParserBase`/`JSONParserMemory`/`JSONParserString`
method that constructs one bails out of compilation at
`cp_new_resolver -> None` (three attempts, then the method interprets forever —
`readMain` accumulated 293,940 interpreted invocations). This is a general JIT
limitation, not a json-smart one: any hot method with a cold
`throw new SomeNotYetLoadedException(...)` is uncompilable. Documented
separately in
`docs/known-issues/jit-bans/jit-compile-bail-unresolved-new-cold-class.md`.
It does not affect this ban's disposition — the three methods the ban named
contain no `new` and compile regardless — but it is why `JsonSmartProbeWarmed`
drives a few failing parses first: without that warm-up, most of the package
never reaches the JIT and a clean result would prove much less.

## Reproduction

Repro sources: `docs/known-issues/repros/jsonsmart/` — `JsonSmartProbe.java`
(the original), `JsonSmartProbeWarmed.java` (same body, plus the
ParseException-loading warm-up), `MiniJsonProbe.java` (one document, prints the
first iteration whose key order changes), `MapCapProbe.java` (plain JDK
collections, no json-smart).

```bash
JS=<...>/json-smart-2.6.0.jar
AS=<...>/accessors-smart-2.6.0.jar   # required by JSONValue.toJSONString
ASM=<...>/asm-9.10.1.jar
TMPDIR=/data/tmp <cratonvm-binary> --java-home <jdk25> -Xmx1g \
  -cp "<classdir>:$JS:$AS:$ASM" JsonSmartProbeWarmed 300000
```

`MapCapProbe` needs no jars at all and shows the (now fixed) capacity bug in
under a second: flips at iteration ~501 on a pre-fix binary.

## Disposition

- **Ban stays removed.** `vm/src/jit/skip_list.rs` keeps the
  `json_smart_parser_is_jit_eligible_after_jsonsmart_parser_1_removal` test
  asserting the three named methods are JIT-eligible under both skip policies.
- The old "CONFIRMED still needed" writeup is superseded by this document; the
  cross-session tracking docs
  (`docs/known-issues/jit-bans/jit-skip-list-open-bans-20260725.md`,
  `jit-ban-sweep-consolidated-status-20260726.md`,
  `full-ban-inventory-status-20260726.md`) have been updated to point here.
- Bonus finding, fixed: the trivial-constructor elision / lazy-table capacity
  pair above.
