# SIGSEGV in compiled code during the Tomcat annotation scan — and the bisect levers cannot isolate it

| | |
|---|---|
| **Status** | OPEN — reproduced, narrowed, not root-caused |
| **Severity** | high — a hard crash (`EXCEPTION_ACCESS_VIOLATION`) in compiled code, from a 20-line probe, in ~30 s |
| **HotSpot** | clean |
| **CratonVM** | SIGSEGV, deterministic (5/5) |
| **Discovered** | 2026-08-04, incidentally, while decomposing the webapp-deploy annotation scan |

## Reproduction

```bash
javac -cp "$(cat apps/tomcat/.suite/cp.txt)" -d probes/out probes/AnnotationScanSplitProbe.java
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g \
  -cp "probes/out;$(cat apps/tomcat/.suite/cp.txt)" \
  AnnotationScanSplitProbe apps/tomcat/output/build/webapps/examples/WEB-INF/lib 1
```

Prints `parseMem`, `readBytes`, `readRaw`, then dies before `arrayRead`
reports:

```
# A fatal error has been detected by the CratonVM Runtime Environment:
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x0000025DD82612E3
#  Java frames (primordial thread): <none published yet>
```

The faulting `pc` is in the dynamic code region, not the executable, so it is
compiled code. `Java frames … <none published yet>` means the crash reporter
could not map it back to a frame.

The faulting stage is `arrayRead`, which is this, and nothing else:

```java
long sink = 0;
for (byte[] b : data) {
    for (int i = 0; i < b.length; i++) { sink += b[i]; }
}
```

## What is established

| arm | result |
|---|---|
| default, `-Xmx2g` | **SIGSEGV** |
| default, `-Xmx8g` | **SIGSEGV** — so not heap pressure / GC |
| `--nojit` | **clean**, runs to completion |
| `CRATONVM_DISABLE_JIT=1` | **clean** |

JIT-only and heap-independent, so this is a **miscompile**, not a GC
use-after-free — the exclusion
[[reference_exclude_jit_wrong_object_before_blaming_gc]] asks for, done first.

**It does not minimise.** `probes/`-style standalone versions of the same two
nested loops over synthetic `byte[]` data (156 × 2150 B, matching the real
shape, with and without the earlier stages to warm tiering) run **clean** in
both JIT and no-JIT modes. The crash needs the real `parseMem` stage — i.e.
Tomcat's BCEL parser actually running — before it. Whatever is wrong is set up
by that workload, not by the faulting loop's own shape.

## The bisect levers had a real hole — in OSR, not where I first said

> **Correction (2026-08-04).** The first revision of this section claimed the
> levers were bypassed by *the eager single-pass first-call path*, on the
> strength of a run where `bisect-only=zzzNoSuchPrefix` still left 11 methods
> compiled. **That was wrong, and it was wrong for the dumbest possible
> reason: I spelled the flag as `CRATONVM_JIT=bisect-only=…`, and
> `bisect-only` is a `CRATONVM_DBG` token (`jit-bisect-only`), not a
> `CRATONVM_JIT` one.** The VM rejected it with `unknown configuration token`
> on stderr, which my output filter dropped. Every "no effect" row was an
> inert lever — the exact trap `reference_inert_lever_is_not_an_elimination`
> exists to prevent, walked into while holding the note that warns about it.
> Validating *one* token (`definitely-not-a-token`) and then assuming the rest
> parsed is not validation. **Check that the lever changed the compiled census,
> not merely that some token somewhere is rejected.**

With the correct spelling the levers work, and there was still a genuine hole
underneath the mistake. `CRATONVM_DBG=jit-bisect-only=zzzNoSuchPrefix` allows
nothing to compile:

| binary | OSR entries under `jit-bisect-only=zzz` | `JitCache::put` census | result |
|---|---:|---:|---|
| unmodified `dev` | **21** | 0 | **SIGSEGV** |
| with the gate below | **0** | 0 | **clean** |

`compile_osr_artifact` reaches `x64::compile_with_param_slots` directly — its
own comment says so ("This path calls the backend directly instead of going
through `try_compile`") — and the levers were applied only inside
`try_compile`. So **OSR bodies were force-interpretable by neither lever**, and
because OSR publishes through `put_osr` rather than the counted `put`, the
`jit-compiled` census showed **zero** while 21 OSR bodies were compiling and
one of them was crashing. A bisect against that reads as a clean exoneration.

**Fixed here**: the two levers are now one shared predicate,
`cratonvm_jit::jit_force_interpret`, applied at `try_compile`, at
`compile_osr_artifact`, and at `execute`'s eager first-call compile.

## With the levers working, the bisect converges

All on the fixed binary. Control first — it must still crash, or the "fix" is
just "OSR disabled":

| arm | result |
|---|---|
| **control, no flags** | **SIGSEGV** |
| `jit-bisect-only=AnnotationScanSplitProbe` | **SIGSEGV** |
| `jit-bisect-only=org/apache/tomcat` | clean |
| `jit-bisect-only=java/` | clean |
| `jit-bisect-only=org/apache/tomcat,java/` | clean |
| `CRATONVM_JIT=deny=AnnotationScanSplitProbe` (whole class) | clean |
| `deny=…​.main` / `.readBytes` / `.arrayRead` / `.parseMem` / `.readRaw` / `.allocOnly` | **SIGSEGV** (each) |

So the miscompiled code is **in the probe class itself**, and **no single
method accounts for it** — denying any one is not enough, denying all of them
is. That points at an interaction between two or more compiled bodies of that
class rather than one bad body, which is the next thing to pin down (pairwise
deny is ~15 runs and was not done here).

The one OSR entry the class takes is worth recording as the leading suspect:

```
[cratonvm-osr] enter AnnotationScanSplitProbe.readBytes()J entry_pc=59
  num_locals=8 locals=[475842200, 0, 115580, 0, …] tags=[1, 1, 1, 1, 4, 4, 4, 0]
```

`readBytes` holds a `long` accumulator (a category-2 local) alongside three
reference locals, which is the shape of the known slot-reuse / OSR-trampoline
family (`probes/SlotReuseCategoryProbe.java`, `probes/HighHalfReuseProbe.java`).
Denying `readBytes` alone does **not** stop the crash, so it is not the whole
story.

## Suggested next steps

1. **Pairwise deny inside the probe class** (~15 runs) to find the interacting
   pair. Single-method denies are all negative and the whole-class deny is
   positive, so the answer is a combination.
2. `CRATONVM_DBG=osr,osr-meta` on the failing run, focusing on
   `readBytes()J entry_pc=59` — the category-2 accumulator beside three
   reference locals is the shape of the slot-reuse family, and
   `probes/OsrDeadLocalProbe.java` / `probes/SlotReuseCategoryProbe.java` are
   the existing differentials for it.
3. `CRATONVM_SYMBOLIZE` the faulting `pc` against the same binary (with the
   `.pdb` beside the exe — see
   [[reference_symbolize_cratonvm_crash_needs_pdb_beside_exe]]) and cross-check
   with `llvm-objdump`, since ICF can name the wrong function
   ([[reference_profsym_profile_and_icf_confused_symbolize]]).
4. The crash needs `parseMem` to run first, so bisect the *preceding* stages
   too — dropping `parseMem` may well make it vanish and name the setup.

**Whatever you do, verify each lever changed the compiled census** (`OSR
enters` and `JIT_COMPILED: put` counts) before reading anything into a "no
effect" row. That is what this investigation got wrong the first time.

## Relationship to the throughput work

Found while decomposing the deploy wall, and unrelated to it. The same probe's
timing output is what establishes that the annotation scan's cost is the
per-byte I/O call chain rather than object construction — see
`docs/known-issues/tomcat/webapp-deploy-annotation-scan-interpreted-226x.md`.
The binary that crashes predates the loader-latch fix merged the same day, so
that change is not implicated.
