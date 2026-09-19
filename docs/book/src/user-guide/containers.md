# Containers & cgroups

CratonVM ships a cgroup detector that reads the memory and CPU limits a
container runtime (Docker, Podman, Kubernetes) imposes, analogous to HotSpot's
`-XX:+UseContainerSupport`. **Read the status note below before relying on
automatic cgroup-derived sizing** — the detector is implemented and tested, but
not yet wired into the heap sizer.

## What the detector reads

On Linux, the detector inspects cgroup v2 first (unified hierarchy), then falls
back to cgroup v1, and reports:

| Field | Meaning |
|-------|---------|
| `is_containerized` | Whether a container runtime was detected (via `/.dockerenv`, `/run/.containerenv`, or container markers in `/proc/1/cgroup`). |
| `memory_limit` | Memory limit in bytes (`max`/`-1`/huge sentinel → unlimited). |
| `cpu_quota` / `cpu_period` | CPU quota and period (µs). |
| `effective_cpu_count` | `ceil(quota / period)`, at least 1. |

It is **Linux-only**: on Windows and macOS the detector always reports "not
containerized" with no limits, and never touches `/proc` or `/sys`.

## The ergonomic default heap

When you don't pass `-Xmx`, the launcher picks a default heap of about **¼ of
physical RAM**, floored at 256 MiB and capped at 4 GiB. See [Memory &
GC](memory-and-gc.md) for the full rules.

## Status: what's wired

Be aware of the seams:

- **The cgroup detector is implemented and unit-tested, but not yet consumed by
  the heap sizer.** The ergonomic default heap is driven by *physical RAM*
  (the host total on Linux), **not** the cgroup memory limit. Inside a
  memory-constrained container the default can therefore overshoot the limit.
- **`-XX:-UseContainerSupport`** (the disable form) is a real flag and flips the
  config off, but because the detector isn't wired into sizing yet, it currently
  has no effect on heap or CPU sizing — it's accepted for HotSpot
  command-line parity.

### Recommendation

Until cgroup-derived sizing is wired in, **set `-Xmx` explicitly** in
memory-constrained containers, or cap the ergonomic default:

```bash
# Set the heap explicitly (recommended for production containers)
docker run --rm --memory=512m myimage \
  cratonvm -Xmx384m -cp /app Main

# Or cap the ergonomic default regardless of host RAM
docker run --rm --memory=512m myimage \
  bash -c 'CRATONVM_DEFAULT_HEAP_MAX_MB=384 cratonvm -cp /app Main'

# Watch the heap decision
docker run --rm --memory=512m myimage \
  cratonvm --verbose:gc -cp /app Main
```

A good rule of thumb is `-Xmx` ≈ 50–75% of the container memory limit.

## Limitations

- Linux-only; no Windows-container / job-object support.
- cgroup v2 `cpu.weight` and cgroup v1 `cpu.shares` are not turned into a CPU
  count (only `quota/period` is).
- No swap accounting.
