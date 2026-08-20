# RETIRED — a relocating young collection nulled live `String` references

## Status

**RETIRED 2026-08-18, FIXED.** The lost reference was never a `String` and never
a precise-map edge. It was `SharedVm::classes::proxy_method_cache` — the cache of
VM-synthesized `java.lang.reflect.Method` objects handed to every
`InvocationHandler.invoke` — which `memory/roots.rs` §6b **scanned as a root**
and `memory/gc::update_all_roots` **never remapped**. Fixed by
`remap_proxy_method_cache` (`memory/gc.rs` §6a).

Retired from `docs/known-issues/gc/`.

## The mechanism, end to end

1. `proxy_invoke_handler_shared` (`vm/src/vm/vm_exec.rs`) builds one
   `java.lang.reflect.Method` per `(proxy class, method name, descriptor)` and
   memoises it in `proxy_method_cache`. Real JDK proxies build that object once
   per interface method, so the cache is a correctness-neutral throughput fix.

2. `roots.rs` §6b pushes every cached value as an **unconditional** root, with
   the comment "exactly like `class_mirrors` above."

3. `gc.rs` remaps `class_mirrors` — and `primitive_mirrors`, `module_mirrors`,
   `statics`, `class_locks`, `resolution_cache`, `class_mirrors_reverse`,
   `var_handle_roots`, `singleton_oom`, `system_out/err/in`,
   `main_thread_group`. It did **not** remap `proxy_method_cache`.

Rooting without remapping is the worst of the two failure modes, not the safe
one. Because the entry is rooted, a moving young cycle **does** evacuate the
`Method` — and then the cache still holds the from-space address. Every later
dispatch on that key hands running Java a pointer into reset from-space memory.
`Method.name` reads `0`, so `Method.getName()` returns `null`.

`getName()` returning null is what produced *both* NPE shapes on the original
page, from one line of Spring:
`SynthesizedMergedAnnotationInvocationHandler.invoke` switches on
`method.getName()`, and javac lowers a `String` switch to
`astore <localN>; aload <localN>; invokevirtual String.hashCode()`. So the
synthetic switch local is the `<local4>` in

```
NullPointerException: Cannot invoke "String.hashCode()" because "<local4>" is null
```

and the sibling `isAnnotationTypeMethod` test is

```
NullPointerException: Cannot invoke "String.equals(Object)" because
    the return value of "java.lang.reflect.Method.getName()" is null
```

Same object, same field, two call sites.

## Answering the page's three "NOT established" items

* **Which root class is lost.** The proxy-dispatch `Method` cache — a `shared.*`
  side table, not a precise map and not frame metadata. A diff of every
  `shared.*` root source in `roots.rs` against every remap target in `gc.rs`
  returns exactly one name, `proxy_method_cache`. It was the only channel in the
  VM with a scan half and no remap half.

* **Nulled or stale.** Both, in sequence, and the distinction is what hid it.
  The *cache entry* goes **stale** (a from-space address). The *field Java
  reads* comes back **null**, because the from-space body is reset. Looking for
  a nulled root would never have found it; looking for a stale root would have
  found it immediately.

* **Whether G1 shows it.** It does not — measured, not assumed. G1 pins the
  conservative root array rather than evacuating through it (see
  `the root array is over-approximate; a MOVING collector must PIN a
  non-object root`), so the cached `Method` never moves. ZGC never relocates at
  all. Generational was the only arm that both roots the entry and moves the
  object, which is exactly why the page read as collector-specific.

## Measured

### Witness probe — `probes/ProxyMethodNameProbe.java`

A `Proxy` whose `InvocationHandler` switches on `method.getName()`, driven with
enough young churn to force minor collections. 60000 rounds, `--Xmx 256m`, Azure
Linux, **the same three moving young cycles in both arms**
(`decision histogram: moving=3 non_moving=0`).

| binary | collector | calls | `null_getName` | verdict |
|---|---|---:|---:|---|
| pre-fix `ca7aaf4e` | Generational | 180000 | **122751** | FAIL |
| post-fix `8d5b06dd` | Generational | 180000 | 0 | PASS |
| pre-fix `ca7aaf4e` | ZGC (default) | 180000 | 0 | PASS |
| pre-fix `ca7aaf4e` | G1 | 180000 | 0 | PASS |
| HotSpot 25 | — | 180000 | 0 | PASS |

The failure fraction is the signature: 68% of calls, not a handful. A race in
the build window would null a few `Method`s; a permanently stale cache entry
nulls every dispatch after the first moving cycle, forever.

### The two Spring Boot classes, interleaved A/B

One class per process, `--Xmx 2g`, JIT on, real JDK 25 backend, the three
load-bearing suite env vars, 400 s cap, `-XX:+UseGenerationalGC`. Arms
interleaved base/fix within each round, two rounds.

| Class | round | pre-fix | post-fix |
|---|---|---|---|
| `FlywayAutoConfigurationTests` | 1 | **18 FAIL** / 73 | ✓ **73/73** |
| `IntegrationAutoConfigurationTests` | 1 | **18 FAIL** / 34 | ✓ **34/34** |
| `FlywayAutoConfigurationTests` | 2 | **45 FAIL** / 73 | ✓ **73/73** |
| `IntegrationAutoConfigurationTests` | 2 | **17 FAIL** / 34 | ✓ **34/34** |

The pre-fix failure count is not stable (18/45 on Flyway, 18/17 on Integration)
because it depends on which context refresh the first moving cycle lands in.
The post-fix count is 0 in every arm of every round. HotSpot is 73/73 and 34/34
on the same host.

### The remaining arms, post-fix

Same harness. `Log4J2LoggingSystemTests` is the page's control class (green on
both collectors before the fix, and still green); the ZGC rows check the shipped
default did not regress; the G1 rows answer the page's third open item.

| Class | collector | binary | result |
|---|---|---|---|
| `Log4J2LoggingSystemTests` | Generational | post-fix | ✓ 63/63, 282 s |
| `FlywayAutoConfigurationTests` | ZGC (default) | post-fix | ✓ 73/73, 165 s |
| `IntegrationAutoConfigurationTests` | ZGC (default) | post-fix | ✓ 34/34, 131 s |
| `FlywayAutoConfigurationTests` | G1 | post-fix | ✓ 73/73, 187 s |
| `FlywayAutoConfigurationTests` | G1 | **pre-fix** | ✓ 73/73, 204 s |

The last row is the one that matters for triage: G1 is a *moving* collector and
was green on the *broken* binary. "The collector moves objects" was never
sufficient — the object also has to be reachable only through a table nobody
remaps.

## What the original page got right, and the one thing it got wrong

It was right that the failure is a lost reference across relocation, right that
the fallback spiral closing is what exposed it, and right to refuse to name a
mechanism it had not measured. Its "What is NOT established" section is the
reason this took one session instead of several: it did not send anyone chasing
precise maps.

The one wrong turn is in the title and the framing: **"the collector"**. The
collector did exactly what it was told. The bug is one missing loop in
`update_all_roots`, in `vm/`, not in `gc/`. A page that names the *arm that goes
red* as the *component that is broken* points the next reader at 23k lines of
`gen_heap.rs`. The discriminating question was cheaper: *what does the
Generational arm do that ZGC does not, and who is holding a pointer across it?*

## Guards left behind

* `remap_proxy_method_cache` in `vm/src/memory/gc.rs`, factored out of
  `update_all_roots` in the house style so the pairing is unit-testable without
  standing up a VM (same as `remap_handle_slots` / `remap_thread_object_slots`).
* `moving_gc_rewrites_the_proxy_method_cache_in_place` — unit test covering the
  mapping, including that an entry absent from the pointer map is left alone.
* `probes/ProxyMethodNameProbe.java` — the wiring guard, and the honest one: the
  unit test cannot fail if the *call site* is deleted, and this probe can. It
  went from `null_getName=122751` to `0` across the fix on otherwise identical
  binaries.
* The `proxy_method_cache` doc comment in `class_realm.rs` now names **both**
  halves and says what breaks when only one ships.

## Related

- `docs/internal/retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md`
  — the page this was found underneath. Its own closing warning ("the fix might
  just move the failure") was correct, and this is where the failure moved to.
- `docs/known-issues/perf/quartz-stackwalker-walk-is-38x-hotspot-20260818.md`
  — the other finding from the same re-measurement (renamed from
  `known-issues/springboot/quartz-endpoint-web-jit-only-spin-loop-20260818.md`
  on `dev` while this was in flight). Unrelated, still open.
