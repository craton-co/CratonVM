# `SimpleClientHttpResponseTests` hangs — and it is NOT arena fragmentation

## Status

**OPEN, re-attributed 2026-08-29.** This class was Occurrence 1 on
`zgc-arena-fragmentation-occurrences-to-reverify-20260829.md`. The
re-verification that page asked for has been run, and it takes the class OFF
that page: the hang survives with the fragmentation gauge firing **zero** times,
and the signature the page matched on belongs to a different guard.

The full matrix is in
`internal/fixed-suite-bugs/gc/zgc-frag-occurrences-reverified-20260829.md`.

## The failure

`org.springframework.http.client.SimpleClientHttpResponseTests`, Azure Linux
(`20.80.105.49`), JDK 25 Temurin, CratonVM `dev@96e07ca86`, through the suite's
own `one.sh` so it inherits Spring's Gradle test-JVM flags:

| VM | result |
|---|---|
| real HotSpot | **PASS** `found=5 succ=5 fail=0`, **9 s** |
| CratonVM, all four arms | **rc=124** — killed at the 500 s cap |

| arm | rc | wall | `zgc frag gauge` lines |
|---|---:|---:|---:|
| default | 124 | 500 s | 1 |
| `CRATONVM_ZGC_HIGH_COMPACTION=0` | 124 | 501 s | **0** |
| `CRATONVM_ZGC_TLAB_STARVED_RECYCLE=0` | 124 | 500 s | 1 |
| both `=0` | 124 | 500 s | 1 |

## Three reasons this is not the fragmentation defect

**1. It hangs identically in a run where the gauge never fires.** The `hi0` arm
timed out with `zgc frag gauge` count **0**, and a separate 240 s run under
`CRATONVM_GC_STATS=1` also logged the gauge **0** times while the class hung the
same way. A symptom that is absent while the failure is unchanged is not the
cause.

**2. Neither 2026-08-29 repair moves it, in either direction.** All four arms
time out, including `both=0`, which is the pre-2026-08-29 behaviour byte for
byte. Whatever this is, it predates and outlives that work.

**3. The signature the parent page matched on belongs to a different guard.**
That page's "symptom to match on" was the fragmentation gauge *"followed by …
a livelock in which the guard's own `occurrence` counter doubles on every firing
(32768 -> 1048576 -> …)"*. Measured, with the ANSI escapes stripped so the
fields are visible:

```text
occurrence= carriers in the failing log:
     23  "…that one boxes and fires only for the class mirror; this one nulls…"
     15  "a non-reference value was stored into a slot the class declares as a REFERENCE…"
      0  zgc frag gauge
```

The doubling counter — `… 32768 65536 131072 262144 524288 1048576` — is on the
**descriptor-coercion guards**, `G30-1-the-silent-reference-slot-coercion` and
its W7-84 autobox sibling. **The `zgc frag gauge` line carries no `occurrence`
field at all.** Both guards live in `cratonvm::gc::guard`, which is how the two
came to be read as one symptom.

## The coercion storm is NOT the cause either — censused 2026-08-30

The first version of this page stopped at "at least 1,048,576
descriptor-coercion events" and offered that as what the hang looks like. The
census it listed as not-yet-done has now been taken, and it retires that
reading. **The storm is designed behaviour, and every workload has it.**

### What the storm is, exactly

289 backtraces in a 3 MB sample, and they are **one shape**:

```text
species="primitive-into-reference"  access="read"  descriptor=[  value=Int(0)
class_id=64 index=2                        289 of 289
[layout] java/util/HashMap cid=64
```

`java.util.HashMap`, slot 2 — the `table : HashMap$Node[]` field — read back as
`Int(0)`. Every one of them arrives through `native-collections`'s `map_state`:

| count | path into `map_state` |
|---:|---|
| 184 | `map_resize_inner <- map_resize <- native_map_put_evict_pinned` |
| 93 | `native_map_put_evict_pinned <- native_map_put <- native_hs_add` |
| 12 | `map_collect_keys <- collect_view_snapshot_ordered <- native_hs_iterator` |

### Why that is not a defect

`known-issues/jdk-only/G30-1-the-silent-reference-slot-coercion-20260817.md`
already has this row, and marks it **DESIGNED-DEGRADE**:

> `HashMap.table` receiving `Int(capacity)` | degrades to null | **degrades to
> null**, pinned by a test (§5)

and, in its §1: *"The rule is deliberate, tagged `S111r29`, and load-bearing for
`HashMap`."* The VM's own `native_map_init` writes the capacity as an `Int` into
the JDK `table` slot; the descriptor-aware read turning that into `null` is the
wanted answer, not a loss.

MEASURED, and this is what settles it — a twenty-line program on the same
binary:

```java
Map<String,String> m = new HashMap<>();
for (int i = 0; i < 20000; i++) m.put("k"+i, "v"+i);
for (String k : m.keySet()) { }
Set<String> s = new HashSet<>();
for (int i = 0; i < 20000; i++) s.add("s"+i);
```

**2,718 coercion-loss events, all `class_id=64 index=2`** — the identical
species and slot — and the answers match HotSpot exactly
(`size=20000 iter=20000 set=20000`). Any program that uses a `HashMap` produces
this. It is not a property of this test, and a counter that climbs into the
millions over 500 s is a counter on a hot path, not a smoking gun.

### So the hang is still undiagnosed — but two candidates are now excluded

* **not arena fragmentation** — the gauge fires zero times in arms that hang
  identically (above);
* **not the coercion storm** — designed, universal, and present in a program
  that finishes instantly.

Also measured: the storm is **not JIT-specific**. A `--nojit` arm produces it at
the same rate (both arms saturated a 150 MB capped log inside a 90 s cap).

**The lesson this page has now taught twice.** The loudest signal in the log was
first read as the fragmentation guard's (it belongs to a different guard), and
then as a defect (it is designed). A rate is not evidence of a cause until
something without the failure has been measured for the same rate.

## Not yet done

- **What the class is actually doing.** Both eliminations above are negative
  results; nothing yet says where the 500 s goes. The next instrument is a
  sampling profile or a thread dump partway through, not another guard counter —
  two guard counters have now each been chased to a dead end.
- Whether the 184-of-289 `map_resize` share is itself interesting. It says
  resize-path calls to `map_state` outnumber plain-put calls two to one AMONG
  COERCING CALLS, which is not the same as a resize rate; `map_resize` calls
  `map_state` more than once, so this may be arithmetic rather than a signal.
  It has not been separated.
- The Mockito angle, untouched: this is an HTTP-client test built almost
  entirely of mocks, and `known-issues` already carries several
  Mockito-dispatch records.

## Repro

```bash
# on the Azure host
cd /data/cratonvm/apps/spring-suite-runner
CRATONVM_BIN=<binary> JDK25=/data/toolchain/jdk-25 \
  timeout 500 bash one.sh org.springframework.http.client.SimpleClientHttpResponseTests

# HotSpot control, 9 s
JDK25=/data/toolchain/jdk-25 bash hs.sh \
  org.springframework.http.client.SimpleClientHttpResponseTests
```

Strip the ANSI escapes before grepping the log — `sed -r 's/\x1b\[[0-9;]*m//g'`.
Without that, `grep occurrence=` matches nothing, because the field is rendered
as `[3moccurrence[0m[2m=[0m`. That is how the counter came to be attributed to
the wrong guard in the first place.
