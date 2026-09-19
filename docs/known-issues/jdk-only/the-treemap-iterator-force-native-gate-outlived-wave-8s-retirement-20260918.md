# FIXED 2026-09-18 — `TreeMap`'s iterator force-native gate outlived wave 8's retirement, and `CRATONVM_REAL=all` is what could see it

## 1. The defect

`cargo test -p cratonvm-vm --test synthetic_diff synthetic_vs_real` diverges
on exactly one row, newly exposed once
[`the-system-properties-real-map-is-null-and-it-blocks-the-chm-retirement-20260909.md`](the-system-properties-real-map-is-null-and-it-blocks-the-chm-retirement-20260909.md)'s
fix let the program run to completion under `CRATONVM_REAL=all` at all:

```text
synthetic: r:Collectors.toMap.merge.sorted={a=111, b=22, c=3}
real:      r:Collectors.toMap.merge.sorted={}
```

`Map<String,Integer> merged` (a `HashMap` built by `Collectors.toMap(...,
Integer::sum)`) answers `merged.get("a")` etc. correctly in both arms. `new
TreeMap<>(merged)` reads as `{}` only in the real arm. A narrower probe found
the same shape one layer down: `sorted.size()` and `sorted.firstKey()` (both
walk the real `root`/`Entry` fields) read correctly, but iterating
`sorted.entrySet()` produces nothing, and repeating `it.hasNext()` at the
*same* call site flips `true` → `false` on the second call with no `next()`
or mutation in between.

## 2. Root cause

`java/util/TreeMap`'s whole family — the map, its view/iterator carriers, and
their producers — was retired together in L1 wave 8
(`native-api/src/retired_shadow.rs`'s `RETIRED_SHADOW_L1_TM_TRIPLES`,
2026-09-12), specifically so a producer and its consumer are never split
across native and real: real `TreeMap` bytecode maintains a real `root` of
real `TreeMap$Entry` nodes, and `native-collections/src/lib.rs`'s
`tm_pairs_from_real_root`/`tm_publish_real_root` are the mirror that keeps
that consistent with the pre-wave-8 native representation when a native still
needs to decode one.

Retirement alone does not make a triple stop dispatching to its native body —
`native-api/src/registry.rs`'s `register()` only re-tags a retired `Bridge` to
`SyntheticStub`; the interpreter still has to *choose* to yield to real
bytecode at each dispatch (`vm/src/runtime/interpreter/native_override.rs`'s
`synthetic_stub_should_yield_to_real_bytecode`, driven by
`env_cache::real_bytecode_selector().prefers_real(class_name)` under
`CRATONVM_REAL`). One gate stands *before* that arbitration and can override
it outright: `force_native_over_real_jdk_bytecode` unconditionally forces
`java/util/TreeMap$EntryIterator`/`$KeyIterator`/`$ValueIterator`'s
`hasNext`/`next`/`remove` onto the registered native, a rule joined
2026-08-22/29 — three weeks before wave 8 — to stop a *different* family
(`HashMap$KeyIterator` and siblings, whose CratonVM-minted carriers hold a
snapshot layout real bytecode cannot read) from reporting every collection
exhausted.

Under `CRATONVM_REAL=all`, `TreeMap.put`/`putAll`/`<init>(Map)` all yield to
real bytecode (confirmed via `CRATONVM_DBG_STUB_YIELD=1`) and write only
`root`/`Entry` fields — `native_tm_put` (the native the retirement made
optional) never runs, so its own `tm_array_table`/fast-mode side storage stays
empty. `TreeMap$EntrySet.iterator()` (itself force-native, but *correct*: it
seeds itself from `tm_pairs_from_real_root`, wave 8's own read mirror) mints a
real `TreeMap$EntryIterator` over the real entries. But that iterator's
`hasNext`/`next` are ALSO forced native by the same stale gate — reading the
empty side storage instead of the object's own `next` field the iterator was
just correctly seeded with. `size`/`firstKey` (real bytecode, not
force-gated) read the real fields and are right; `hasNext`/`next` (forced
native) read the never-populated side store and report the map empty.

## 3. The fix

`force_native_over_real_jdk_bytecode`
(`vm/src/runtime/interpreter/native_override.rs`) gains one early check,
scoped to exactly the three TreeMap iterator carriers and exactly their
`hasNext`/`next`/`remove` arm: when
`env_cache::real_bytecode_selector().prefers_real(class_name)` is true, skip
forcing and let the normal `SyntheticStub` yield arbitration decide — which,
per §2, is what already correctly seeds the iterator from the real root.

Narrower than it could be. Widening the same guard to
`TreeMap$EntrySet`/`$KeySet`/`$Values`'s own force-native block (their
`size`/`iterator`/`toString`/etc.) and to `TreeMap$Entry.setValue`
reproduced a boot-time failure under `CRATONVM_REAL=all`
(`jdk/internal/loader/ClassLoaders.<clinit>` throwing
`IllegalStateException: Not yet initialized` from
`BootLoader.getServicesCatalog`) — investigated and found to be a **separate,
pre-existing** regression (present with or without any change in this file;
confirmed by reverting this fix entirely and reproducing the identical crash
on the same tree). See
[`../../internal/jdk-only/the-classloaders-clinit-not-yet-initialized-boot-crash-FIXED-20260918.md`](../../internal/jdk-only/the-classloaders-clinit-not-yet-initialized-boot-crash-FIXED-20260918.md)
for that one; it independently blocked `synthetic_vs_real` from reaching its
`SYNTHETIC_DIFF_OK` marker and was not fixed here — it has since been fixed
(one line: `saveProperties` stored into an uninitialised `VM`).

## 4. Verified

`CRATONVM_DBG_STUB_YIELD=1` under `CRATONVM_REAL=all` on the collectors/TreeMap
program: `TreeMap.<init>(Map)`, `putAll`, `put`, `toString`, `entrySet` all
`yield=true — real bytecode wins` (unchanged by this fix), and
`TreeMap$EntryIterator.hasNext()` now also appears in that trace (it did not
force-skip the arbitration before this fix), with the entries iterating
correctly. `cargo test -p cratonvm-vm --test synthetic_diff synthetic_vs_real`
itself cannot complete end to end while §3's boot crash stands; it is not the
instrument for this fix's own correctness for that reason, and the
STUB-YIELD trace plus the direct-repro probes above are.

## 5. What this does NOT claim

* Not that `TreeMap$EntrySet`/`$KeySet`/`$Values`'s own force-native block, or
  `TreeMap$Entry.setValue`, are correct under `CRATONVM_REAL=all` — only that
  widening this fix to them needs the boot crash in §3 understood first, since
  that is what widening surfaced.
* Not that the `HashMap`/`LinkedHashMap`/`ConcurrentHashMap` iterator carriers
  in the same `force_native_over_real_jdk_bytecode` block have an analogous
  defect — they were not measured here, and the doc comment beside them
  describes a still-current reason (a CratonVM-minted snapshot layout, not a
  wave-8-style whole-family retirement) that this page's argument does not
  apply to by default.
