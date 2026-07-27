# CratonVM — Container / cgroup Awareness

CratonVM ships a cgroup detector (`vm/src/runtime/container.rs`) that reads the
memory and CPU limits a container runtime (Docker, Podman, Kubernetes) imposes,
plus a separate **ergonomic default-heap** sizer in the launcher
(`vm-cli/src/main.rs`) so GC-heavy workloads get a sensible heap without an
explicit `-Xmx`.

This is the analogue of HotSpot's `-XX:+UseContainerSupport`. Read the
**Status / what's wired** section before relying on it — the detector is
implemented and unit-tested, but it is **not yet consumed by the heap sizer**.
The ergonomic default heap is driven by *physical RAM*, not the cgroup limit.

Cross-links: [CONFIG.md](CONFIG.md) (full flag reference, "Heap and GC"),
[GC tuning notes](gc-tuning.md).

---

## What the detector reads

`detect_container()` returns a `ContainerInfo`:

| Field | Source (cgroup v2 / v1) | Meaning |
|-------|--------------------------|---------|
| `is_containerized` | `/.dockerenv`, `/run/.containerenv`, or container markers in `/proc/1/cgroup` (`docker`, `kubepods`, `containerd`, `lxc`) | Container runtime detected |
| `memory_limit` | `memory.max` / `memory.limit_in_bytes` | Memory limit in bytes (`max`/`-1`/huge-sentinel → `None`) |
| `memory_usage` | `memory.current` / `memory.usage_in_bytes` | Current usage in bytes |
| `cpu_quota` | `cpu.max` field 1 / `cpu.cfs_quota_us` | Quota µs per period (`-1` = unlimited) |
| `cpu_period` | `cpu.max` field 2 / `cpu.cfs_period_us` | Period µs (typically 100 000) |
| `cpu_shares` | — / `cpu.shares` | Relative weight (v1 only; v2 uses `cpu.weight`, not read) |
| `effective_cpu_count` | computed | `ceil(quota / period)`, min 1 |

Detection tries **cgroup v2 first** (unified hierarchy, gated on the presence of
`/sys/fs/cgroup/cgroup.controllers`), then falls back to **cgroup v1** (separate
`/sys/fs/cgroup/{memory,cpu}` controller hierarchies).

The "no limit" cases are normalized to `None`:

* v2 writes the literal `max` for an unset memory/CPU limit.
* v1 writes `-1` for an unlimited CPU quota, and a very large sentinel
  (`PAGE_COUNTER_MAX << PAGE_SHIFT`) for "no memory limit" — any value
  `>= 2^62` bytes is treated as unlimited.

**Linux only.** On Windows and macOS the module compiles but
`detect_container()` always returns `is_containerized: false` with every limit
`None` (it never touches `/proc` or `/sys`). `read_cgroup_file` additionally
refuses to follow symlinks that escape `/sys/fs/cgroup`, so it can't be tricked
into reading arbitrary host files.

### CPU count

`calculate_effective_cpus(quota, period)` returns `ceil(quota / period)` clamped
to at least 1 (so `1 / 100000` rounds up to 1 CPU, `150000 / 100000` rounds up
to 2). An unlimited or invalid quota falls back to
`std::thread::available_parallelism()` (the hardware thread count).

The helpers `effective_memory_limit(info, config_max)` (= `min(cgroup_limit,
config_max)`) and `effective_available_processors(info)` (cgroup count, else
hardware) express how the limits *would* be applied.

---

## The ergonomic default max heap

When you do **not** pass `-Xmx`, the launcher calls `ergonomic_default_max_heap()`
instead of using the fixed `256m` library default:

* **Fraction:** max heap = **1/4 of physical RAM**
  (approximating stock JDK `-XX:MaxRAMPercentage=25`).
* **Floor:** 256 MiB — this only ever *raises* the heap above the historical
  baseline, never lowers it.
* **Cap:** 4 GiB (`MAX_ERGONOMIC_HEAP`). The cap exists because CratonVM's
  generational heap **eagerly commits** its arenas (`Arena::new` →
  `vec![0u8; cap]`), so an uncapped 1/4-of-RAM heap on a big host would charge
  that much commit per process. Override the cap with
  `CRATONVM_DEFAULT_HEAP_MAX_MB=<N>` (in MiB).
* **Fallback:** if physical RAM can't be probed, no ergonomic value is applied
  and the fixed `256m` default stands.

Without this, real-world apps (Spring / Mockito / ByteBuddy / JUnit) thrash GC
at 256 MB and look like a hang where HotSpot, which auto-sizes, finishes fine.

`--verbose:gc` prints the chosen value at startup, e.g.:

```
[cratonvm] ergonomic default max heap: 4096 MB (1/4 physical RAM; set -Xmx or CRATONVM_DEFAULT_HEAP_ERGONOMICS=0 to override)
```

---

## Overriding

| What | How | Notes |
|------|-----|-------|
| Explicit heap | `-Xmx<size>` (`-Xmx512m`, `--Xmx 1g`) | Always wins over the ergonomic default and any cgroup limit. |
| Disable ergonomic sizing | `CRATONVM_DEFAULT_HEAP_ERGONOMICS=0` | Reverts to the fixed `256m` default. |
| Raise/lower the cap | `CRATONVM_DEFAULT_HEAP_MAX_MB=<N>` | `<N>` in MiB; floored at 256 MiB. |
| Disable container support | `-XX:-UseContainerSupport` | Sets `use_container_support=false` on the config. See status note below. |

The precedence the launcher applies: **`-Xmx` → ergonomic default (1/4 RAM,
capped) → `256m`**.

---

## Usage examples

Run under a Docker memory limit and watch the heap decision:

```bash
docker run --rm --memory=512m myimage \
  cratonvm --verbose:gc -cp /app Main
```

Pin the heap explicitly inside a container (recommended for production —
see limitations):

```bash
docker run --rm --memory=512m myimage \
  java -Xmx384m -cp /app Main
```

Cap the ergonomic default at 2 GiB regardless of host RAM:

```bash
CRATONVM_DEFAULT_HEAP_MAX_MB=2048 cratonvm -cp /app Main
```

Opt out of ergonomic sizing entirely (back to the 256 MB baseline):

```bash
CRATONVM_DEFAULT_HEAP_ERGONOMICS=0 cratonvm -cp /app Main
```

---

## Status / what's wired

Be honest about the seams here:

* **Detector + bridge helper — implemented, not yet wired.** `detect_container()`,
  `effective_memory_limit()`, and `effective_available_processors()` are present
  and unit-tested in `vm/src/runtime/container.rs`. A container-aware bridge,
  **`suggested_default_max_heap(&ContainerInfo, fallback)`**, also exists there —
  it derives a default max-heap of **1/4 of the cgroup memory limit**, floored at
  16 MiB and capped at 8 GiB, returning the `fallback` when not containerized.
  But at the time of writing these have **no production callers** (the helper is
  exercised only by its tests): the cgroup memory limit does **not** yet feed the
  launcher's heap sizer, and the cgroup CPU count does **not** yet back
  `Runtime.availableProcessors()`. Wiring `suggested_default_max_heap` into the
  launcher's `--Xmx`-default path is the documented follow-up.
* **Ergonomic default heap uses physical RAM, not the cgroup limit.**
  `ergonomic_default_max_heap()` calls `physical_ram_bytes()` (which on Linux
  reads `/proc/meminfo` `MemTotal`, i.e. the *host* total), **not**
  `ContainerInfo::memory_limit`. Inside a memory-constrained container the
  default can therefore overshoot the cgroup limit — wiring
  `effective_memory_limit` into this path is the documented TODO.
* **`-XX:+/-UseContainerSupport`.** Only the *disable* form
  (`-XX:-UseContainerSupport`) is a real launcher flag; it flips
  `use_container_support` to `false` on the config. Container support is on by
  default, but because the detector is not yet wired in, this toggle currently
  has no effect on heap or CPU sizing — it is accepted for HotSpot
  command-line parity.

### Recommendation

Until the detector is wired into the heap sizer, **set `-Xmx` explicitly** for
memory-constrained containers (e.g. `-Xmx` ≈ 50–75% of `--memory`), or cap the
ergonomic default with `CRATONVM_DEFAULT_HEAP_MAX_MB`. Don't rely on automatic
cgroup-derived heap sizing yet.

### Limitations

* Linux-only; no Windows-container / job-object support.
* cgroup v1 reads `cpu.shares` but it is not turned into a CPU count
  (only `quota/period` is); cgroup v2 `cpu.weight` is not read at all.
* `memory_usage` / `memory.current` is read but currently unused.
* No swap accounting (`memory.swap.max` / `memsw`).
