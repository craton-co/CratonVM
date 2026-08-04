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

## The isolation tools do not work on it, and that is a second bug

Every documented narrowing lever reads "no effect", and **none of them is
inert** — the plumbing was validated before drawing that conclusion
(`CRATONVM_JIT=definitely-not-a-token` is rejected with `unknown configuration
token`, and `CRATONVM_DISABLE_JIT=1` does change the outcome):

| lever | result |
|---|---|
| `CRATONVM_JIT=deny=ParseSplit2Probe` / `java/io` / `org/apache/tomcat` / `java/util` | SIGSEGV |
| `CRATONVM_JIT=bisect-only=<the probe>` | SIGSEGV |
| `CRATONVM_JIT=bisect-only=zzzNoSuchPrefix` | SIGSEGV |
| `CRATONVM_JIT=-osr`, `-osr-dead-locals`, `osr-dead-mask-blanket` | SIGSEGV |

The last `bisect-only` row is the tell. `zzzNoSuchPrefix` matches nothing, so
it should leave essentially nothing compiled — and yet
`CRATONVM_DBG=jit-compiled` under that exact setting still reports **11
compiled methods**, all `org/apache/tomcat/util/bcel/*`:

```
Constant.readConstant(Ljava/io/DataInput;)…
ConstantUtf8.getInstance(Ljava/io/DataInput;)…
Utility.skipFully(Ljava/io/DataInput;I)V
ConstantUtf8.<init>(Ljava/lang/String;)V
ConstantPool.getConstant(I)… / (IB)… / (ILjava/lang/Class;)…
…
```

So `CRATONVM_JIT=deny=` / `bisect-only=` are applied in `jit::try_compile` and
**do not gate whatever compiled those** — the eager single-pass first-call path
reaches codegen without passing the filter. That makes the project's primary
"which method is miscompiled" tool silently ineffective for an entire class of
compilation, which is worth fixing on its own merits: a lever that filters only
some of the compilers reads exactly like an exoneration.

Those 11 methods are the current candidate set.

## Suggested next steps

1. **Make the filter total.** Apply the `deny` / `bisect-only` predicate at
   every codegen entry point, not just `jit::try_compile`, then re-run the
   bisect above — it should then converge in a few runs.
2. Failing that, `CRATONVM_SYMBOLIZE` the faulting `pc` against the same
   binary (with the `.pdb` beside the exe — see
   [[reference_symbolize_cratonvm_crash_needs_pdb_beside_exe]]) and
   cross-check with `llvm-objdump`, since ICF can name the wrong function
   ([[reference_profsym_profile_and_icf_confused_symbolize]]).
3. The crash needs `parseMem` to run first, so bisect the *preceding* stages
   too — dropping `parseMem` may well make it vanish and name the setup.

## Relationship to the throughput work

Found while decomposing the deploy wall, and unrelated to it. The same probe's
timing output is what establishes that the annotation scan's cost is the
per-byte I/O call chain rather than object construction — see
`docs/known-issues/tomcat/webapp-deploy-annotation-scan-interpreted-226x.md`.
The binary that crashes predates the loader-latch fix merged the same day, so
that change is not implicated.
