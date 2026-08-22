# `ConfigurationPropertySourcesTests` is 245× HotSpot, and it is the native-collections floor

**Status: OPEN — 2026-08-21.** Split out of the 2026-08-21 Spring Boot
three-collector re-run triage (retired page
`springboot-3gc-fails-and-hangs-20260821-RETIRED`), where it was the one HANG
row that was genuinely slow rather than a missing timeout budget. Nothing here is a correctness defect: the class passes 11/11
(1 skipped), the same counts as HotSpot, and every assertion it makes about
*relative* speed holds.

## The number

Azure `vm1`, JDK 25, one process per class, serial, host load 1.0–1.5 at start,
ZGC (the default collector), `-Xmx2g`:

| | HotSpot | CratonVM | ratio |
|---|---|---|---|
| whole class | 2.4 s | **598.7 s** | **245×** |

## It is three tests, not the class

Per-method, same launch (host load ~8 for this table — treat the ratios, not
the absolutes):

| test | HotSpot | CratonVM |
|---|---|---|
| the other eight tests | 1.5–2.4 s each | **1.1–1.4 s each** (CratonVM is *faster*) |
| `environmentPropertyAccessWhenImmutableShouldBePerformant` | 3.2 s | 292.7 s |
| `environmentPropertyAccessWhenMutableWithCacheShouldBePerformant` | 3.3 s | 233.2 s |
| `descendantOfPropertyAccessWhenMutableWithCacheShouldBePerformant` | 2.6 s | 295.1 s |

The eight ordinary tests are startup-dominated and CratonVM wins them. The
whole gap is the three `gh-20625` / `gh-21416` throughput tests, each of which
does 1000 property lookups across 100 `MapPropertySource`s.

## Two readings that are wrong

Both were checked before this page was written, because both would have made
this a defect rather than a floor.

**"The Spring cache never hits under CratonVM."** The dispatch tally over one
run of the immutable test:

```
281 667  ConfigurationPropertyName.hashCode()
179 203  …adapt(PropertySource)
175 094  LinkedHashMap.get(Object)
101 870  SpringIterableConfigurationPropertySource.updateCache(Cache)
101 471  Instant.now()
101 388  SoftReference.<init>(Object)
100 071  …$Cache.add(Map, Object, Object)
 94 932  PropertiesPropertySource.lambda$getPropertyNames$0(Object)
 93 407  Object.clone()
 93 327  HashSet.<init>()
```

One cache rebuild per access looks damning until you read
`SoftReferenceConfigurationPropertyCache.get`: for a **mutable** source
`hasExpired()` returns true whenever `timeToLive == null`, which is the
default, so `refreshAction` runs on every call. That is Spring's own behaviour
and HotSpot does the same work — Spring's own
`environmentPropertyAccessWhenMutableShouldBeTolerable` is `@Disabled("for
manual testing")` and only asserts *under 5 seconds*. Not a defect.

**"`SoftReference.get()` is broken."** It is not: 300 000 reads of a
strongly-held referent, `nullsImmediate=0 nullsFresh=0 nullsWeak=0
identityOk=true`, identical on HotSpot and CratonVM.

## What it actually is

`perf record -F 199 --call-graph dwarf` over the immutable test:

* **96.09 % of samples are inside the `cratonvm` binary.** Almost nothing is in
  JIT-compiled code. Per `perf report --sort dso`, this is a *runtime* cost,
  not a code-quality one.
* No single leaf is worth more than ~8 %. The flat profile:

| share | symbol |
|---|---|
| 7.87 % | `ZgcRealHeap`/`ZObjectStarts::contains` |
| 6.99 % | `ZgcRealHeap::is_object_address` |
| 5.21 % | `NativeContextImpl::resolve_field_index_by_class_id` |
| 4.47 % | `VmHeap::load_and_forward_inner` |
| 3.96 % | `NativeContextImpl::read_native_pin` |
| 3.85 % | `vm_exec::resolve_field_descriptor_byte_cached` |
| 2.67 % | `ZgcRealHeap::alloc_raw_tlab` |
| 2.64 % | `heap::coerce_field_value_for_slot` |
| 2.27 % | `ZgcRealHeap::check_field_index` |
| 2.25 % | `native_collections::native_map_put_evict_pinned` |
| 2.19 % | `NativeContextImpl::pin_native_root` |
| 2.10 % | `heap::read_value_cell_checked` |
| 1.99 % | `NativeContextImpl::set_field_by_name` (1.70 % of it in `memcmp`) |
| 1.84 % | `NativeContextImpl::get_field` |
| 1.78 % | `__memcmp_evex_movbe` |
| 1.75 % | `NativeContextImpl::set_field` |
| 1.65 % | `NativeContextImpl::get_field_by_name` (1.55 % in `resolve_field_index_in_hierarchy`) |

That is one shape, spread thin: **native-builtin collections driving Java
objects through checked, sometimes by-name, field access, with an
`is_object_address` membership walk on the references.** `HashMap`, `HashSet`
and `LinkedHashMap` here are native builtins, and every element touch pays the
generic heap-access path that a compiled `HashMap.get` would not.

## Why there is no fix on this page

Because the profile forbids the usual move. Removing the entire by-name
field-access term — `get_field_by_name` + `set_field_by_name` +
`resolve_field_index_by_class_id` + `resolve_field_descriptor_byte_cached`,
about 12 % together — buys ~1.14×, against a 245× gap. There is no dominant
term to attack; the cost *is* the per-element floor of running collections as
natives over checked heap access.

So the honest next steps are structural, and are nominated rather than done:

1. **Bind field indices once per site, not per element.** The by-name half is
   already a known partial — see `docs/feature-designs/by-name-field-reads.md`
   ("Status: Partial", 2000+ `get_field_by_name` call sites). This workload is
   a good measuring stick for it, because a microbench of the accessor will
   over-state the win — a probe written because the real workload is too slow
   to iterate on is not that workload's profile.
2. **Price the `is_object_address` membership walk.** 14.9 % across
   `ZObjectStarts::contains` + `is_object_address`, on a workload whose
   references all come from the VM's own collections. Whether that walk is
   needed for a reference the native itself just produced is a separable
   question with a real number attached.
3. **Ask whether these collections should be native here at all.** The eight
   non-throughput tests in this same class are *faster* than HotSpot, so the
   native path is not uniformly bad; what is missing is a measurement of the
   crossover, per operation count.

## Reproducing

```bash
cd /data/cratonvm/apps/spring-boot/core/spring-boot
CP="$(cat <module>/build/cratonvm-test-cp.txt)"
cratonvm --java-home /data/toolchain/jdk-25 --Xmx 2g --XX:UseGc ZGC \
  -cp "$CP" SbRunner \
  org.springframework.boot.context.properties.source.ConfigurationPropertySourcesTests
```

The dispatch tally is `CRATONVM_DBG=dispatch-tally`. `perf` on this host needs
`sudo sysctl -w kernel.perf_event_paranoid=1` first.

**Read the host load before believing any number here** — this box is shared
and has been seen at load 178 on 8 cores.
