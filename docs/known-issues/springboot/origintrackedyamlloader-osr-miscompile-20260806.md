# `OriginTrackedYamlLoaderTests.canLoadFilesBiggerThan3Mb` — an OSR miscompile that has not been seen since 2026-08-04

**Status: OPEN, and NOT REPRODUCIBLE.** No fix, no attribution, no commit.

Split out of `loader-zip-jit-only-failure-cluster-20260804.md` on 2026-08-06,
which is otherwise closed and retired to the internal tree as
`fixed-suite-bugs/springboot/loader-zip-jit-only-failure-cluster-FIXED-20260806.md`.
That page's other three items are fixed; dragging them along made this one look
like part of a solved cluster when it is the only thing still open.

## What was seen, once

2026-08-04, Azure Linux, a host above load 30. The test builds a >4 MiB YAML
document one line at a time and hands it to snakeyaml, which reported a scanner
error at line 142539 (`ry` on its own line, where the appended line is
`- some list entry`).

Established then, single runs:

* `--nojit` PASSes and `CRATONVM_JIT=-osr` PASSes ⇒ **OSR**, not the main
  compiler. 9 OSR entries, among them the test method and
  `java/util/Arrays.fill([BIIB)V`.
* `deny=Arrays.fill` still FAILs.
* `deny=canLoadFilesBiggerThan3Mb` and `deny=constructSequenceStep2` each PASS —
  and they cannot both name a sole culprit. `deny` perturbs compile scheduling
  globally, so an unrepeated PASS from it is weak evidence.

**Which side is corrupt — the built `StringBuilder` or the parse — was never
measured.** `probes/YamlSplit.java` verifies the document byte-for-byte before
handing it to snakeyaml and exists to answer exactly that; it has never been run
on a failing host, because there has not been one.

## What has been ruled out, and how

Everything below is measured. The 2026-08-05 re-verification tried heap size,
CPU pinning, whole-class and whole-package runs, 6-way concurrency, and the
pre-fix binary itself — all PASS, ~40 runs. See the retired page for that table.

2026-08-06 adds the part that was missing: the **mechanism** the failure was
attributed to is now instrumented, and it is absent.

The attribution was register pressure — `x64::LOCAL_REGS` is 7 on Windows and
5 on System V, so Linux coalesces more and the OSR dead mask has more work to
do. `CRATONVM_JIT_LOCAL_REGS=<n>` now truncates that pool on any host:

| arm | result |
|---|---|
| default (7 registers) | 13/13 |
| `LOCAL_REGS=5` (System V) | 13/13 |
| `LOCAL_REGS=4` | 13/13 |
| `LOCAL_REGS=3` | 13/13 |

And `CRATONVM_DBG_OSR_SEED_COLLISION=1`, which checks both OSR entry-seed
invariants over the published metadata and is verified to go red on a real
miscompile (internal:
`fixed-suite-bugs/jit/osr-seed-invariants-instrumented-20260806.md`):

* **1038 takeable OSR entries across 234 methods, zero violations** of either
  invariant, at 7 registers and at 3.
* `CRATONVM_JIT_OSR_STRIP_ALL_HIGH_HALVES=1` — which reproduces the
  `Arrays.sort(long[])` defect and reports 51 violations on `SortProbe` — reports
  **nothing** here. This workload has no slot that is cat-2 in one range and
  cat-1 in another, so it does not contain the shape that defect needs.

**So a Windows green is no longer vacuous for this failure.** It used to be
("Linux has 2 fewer registers" was an untestable excuse); the pressure is now
controllable and the failure still does not appear.

## What that does and does not mean

It does **not** mean fixed. OSR still enters the method, no commit is
attributed, and a latent miscompile elsewhere in the OSR pipeline is not
excluded — the two invariants above are the two known shapes, not all of them.

It means the leading hypothesis is eliminated by measurement, and the next
reader should not spend time on register pressure or the high-half strip.

## When it next appears

Capture, **at that moment** — the window has closed twice:

1. the failing run's full stderr;
2. `CRATONVM_DBG_OSR_SEED_COLLISION=1` (names the method and local if it is a
   seed-invariant violation at all);
3. `CRATONVM_DBG=osr`;
4. the host's `/proc/loadavg` and `df`.

Then run `probes/YamlSplit.java` on that host to settle build-vs-parse, which is
still the open question. `probes/yaml-osr-lever-matrix.sh` runs every lever 3×
and records the load next to each verdict; single-run lever verdicts on a loaded
box are what produced this page's three unrepeatable "this lever fixes it"
readings.

## Affected classes

- `core/spring-boot` —
  `org.springframework.boot.env.OriginTrackedYamlLoaderTests.canLoadFilesBiggerThan3Mb`
  (1 test; the class is 13). Last observed failing 2026-08-04.
