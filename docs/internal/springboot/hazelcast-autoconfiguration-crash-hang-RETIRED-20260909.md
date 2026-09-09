# RETIRED — the Hazelcast autoconfiguration "crash or hang on every GC"

**Status: RETIRED 2026-09-09**, superseded by measurement. Filed 2026-09-08 as
`docs/known-issues/springboot/hazelcast-autoconfiguration-crash-hang-cluster-decommitted-span-20260908.md`,
deleted by the same change that added this file. Everything it claimed is
quoted below.

Of the six cells in the page's result matrix, **five were wrong by the time it
was filed or wrong when it was filed**, and the one that survives is a
different defect from the one the page names as its lead. The page's own
closing sentence — *"a recorded residual's reason is a hypothesis, not a
verdict"* — turned out to be the accurate part of it.

## What the page claimed, and what a standalone re-run measures

Original matrix, dev tip `a58ebd5c`, one 1991-class suite run on a heavily
loaded shared host, `-TimeoutSec 300`:

| Class | Generational | G1 | ZGC |
|---|---|---|---|
| `HazelcastAutoConfigurationClientTests` | CRASH (139) | HANG (300.0s) | PASS |
| `HazelcastAutoConfigurationServerTests` | CRASH (139) | HANG (308.4s) | HANG (300.0s) |

Re-measured 2026-09-09 at dev tip `ccf731dd3`, one process per class, nothing
else of mine on the host, real JDK 25, `--Xmx 2g`, JIT on:

| Class | Generational | G1 | ZGC |
|---|---|---|---|
| `HazelcastAutoConfigurationClientTests` | **PASS 12/12** (95 s) | **PASS 12/12** (87 s) | **PASS 12/12** (78 s) |
| `HazelcastAutoConfigurationServerTests` | **SIGSEGV, 2 runs in 6** | **PASS 20/20** (491 s) | **PASS 20/20** (476 s) |

HotSpot 25 on the same classpath and harness: `ServerTests` 20/20 in **33 s**.

### The three `HANG` cells were the 300-second timeout, not a hang

Both G1 and ZGC `ServerTests` runs were still emitting Hazelcast lifecycle
lines in the last second before `timeout` killed them. They complete 20/20
when given room: 491 s and 476 s respectively, against HotSpot's 33 s.

`ServerTests` starts and shuts down a whole Hazelcast member per test — nine
full member lifecycles had completed in the 303 s the original G1 arm was
allowed, ten in ZGC's. There was never enough budget for twenty.

That leaves a real finding, but a **throughput** one and not a liveness one:
this class is roughly an order of magnitude slower than HotSpot on this VM.
It belongs in a performance page, not a crash page.

### The `ClientTests` crash and hang are gone

`ClientTests` passes 12/12 on all three collectors. Between `a58ebd5c` and
`ccf731dd3` a run of stale-reference and evacuation fixes landed — among them
`3a2858a17` (a stale reference in a frozen peer's spill slots), `f62c28989`
and `80943727d` (the vacated-address ledger was never told the allocator
re-issues addresses), and `2ea2e5921` (`new Object()` published sixteen zero
bytes). No arm of this page was re-run against them before it was filed.

## The `java/nio/Bits$1` lead was an artefact, twice over

The page's central lead is a `gen_heap::get_field` warning whose own text
calls the shape *"total, silent data loss"*:

```
[RESID-DIAG READ] class=java/nio/Bits$1 index=0 num_slots=0 obj=… real_field_count=Some(0)
   2: buffer_pool_get_name   native-builtins/src/shared_secrets_bridge.rs:3070
```

**It is not causal.** It fires at **line 6** of the stderr log — during boot,
from `VM$BufferPoolsHolder.<clinit>` reaching `JavaNioAccess.getBufferPool()` —
and the crash it was correlated with arrives one to two minutes later. It
fires on `ClientTests`/Generational, which passes 12/12.

**And the per-collector correlation the page tabulates does not exist.** Only
`gen_heap.rs` prints a `RESID-DIAG READ` line; `g1.rs` prints a shorter
message of its own and `zgc.rs` prints nothing. The read happens on every
collector. Counting the Generational spelling produced a table that looked
like a collector-dependent signal and was a logging difference.

**The warning's triage clause is what made it look damning.** The clause reads
"if `class_name` is a REAL JDK class and `num_slots` equals `real_field_count`,
it is not [benign]". Here both numbers are **zero**: `java/nio/Bits$1` declares
no fields at all, so it cannot be carrying a second, aliasing layout and there
is no other writer to disagree with. The clause is vacuously true and says
nothing.

### Fixed anyway, at both levels — `07fc03c0a`

* `buffer_pool_get_name` and `buffer_pool_kind` now ask the receiver's class
  (`buffer_pool_has_slots`, via `class_id_of_object_forwarded`) before reading
  slot 0 or 1. `alloc_buffer_pool` is the only producer of a slot-carrying
  pool object and always stamps `cratonvm/internal/BufferPool`; the other three
  registered receivers — the `BufferPoolMXBean` interface, the
  `jdk/internal/misc/VM$BufferPool` stamp, and `java/nio/Bits$1` — are real
  classes with no such layout. The answers are unchanged: the out-of-bounds
  read was dropped and the legacy fallback taken every time.
* `gen_heap::get_field` grew a third arm for `num_slots == 0 &&
  real_field_count == Some(0)` that states the aliasing clause does not apply
  and points at the caller.

Measured on `ClientTests`/Generational: 1 warning before, **0 after**, 12/12
either way.

## What survives: the `ServerTests` Generational SIGSEGV

This one is real and reproduces at dev tip. See its own page:
[`hazelcast-servertests-generational-identity-hash-code-faults-on-a-vacated-young-address-20260909.md`](../../known-issues/springboot/hazelcast-servertests-generational-identity-hash-code-faults-on-a-vacated-young-address-20260909.md).

It is **not** the `Bits$1` read, and it is not new to this workload: it is the
same "a stale reference reaches a reader after a moving young cycle" family as
the two open `BindableTests` pages.

## Reproducer (both classes, no PowerShell)

`SbRunner` can be launched directly; the suite runner's own launch line is
reproduced by:

```bash
SB=<repo>/apps/spring-boot
MOD=$SB/module/spring-boot-hazelcast
cd "$MOD"
<cratonvm> --java-home <jdk25> --Xmx 2g \
  --add-opens=java.base/java.net=ALL-UNNAMED --stack-dump-on-timeout 0 \
  --XX:UseGc Generational -Dfile.encoding=UTF-8 -Djava.awt.headless=true \
  -cp "$SB/sb-runner:$(cat build/cratonvm-test-cp.txt)" \
  SbRunner org.springframework.boot.hazelcast.autoconfigure.HazelcastAutoConfigurationServerTests
```

**Give it at least 900 seconds.** A 300-second budget reports this class as a
hang on every collector, which is how this page came to exist.

## The standing lesson

A per-collector correlation table is only a signal if every collector can
produce the row. Before reading one, check that the diagnostic you are
counting is emitted by all of them — and check that the "failing" cells are
failures rather than a timeout you set.
