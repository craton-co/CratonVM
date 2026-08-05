# Spring Boot loader/zip: the JIT-only failure cluster, root-caused

**Status: the loader/zip half is CLOSED.** Root cause found 2026-08-04 and
already fixed on `dev` by `4972cd9c91` ("the post-clinit fixup wrote Unsafe's
long base-offsets as 32-bit ints"), which landed after the binary this page was
originally filed against. Verified below. Two members of the original table
were **not** this bug and are re-scoped as separate open issues at the bottom.

## What it was

`java.util.zip.ZipUtils.get16`/`get32` are how the JDK reads every little-endian
field of a LOC/CEN header. In JDK 25 they are not byte arithmetic — they are

```java
public static final int get16(byte[] b, int off) {
    ...Preconditions.checkIndex...
    return Short.toUnsignedInt(UNSAFE.getShortUnaligned(b, ARRAY_BYTE_BASE_OFFSET + off, false));
}
```

`jdk.internal.misc.Unsafe.ARRAY_*_BASE_OFFSET` are nine **`long`** fields in
JDK 25 (their `ARRAY_*_INDEX_SCALE` siblings really are `int` — only the
descriptor tells them apart). CratonVM's `post_clinit_fixup` repopulates all
eighteen, because `Unsafe.<clinit>` computes them through natives that are not
registered yet during real-JDK bootstrap. It wrote every one as
`Value::Int(16)`.

A slot holding a well-formed `Value` of the **wrong width** is invisible from
one side and fatal from the other:

* the **interpreter** widens an `Int` where a long is wanted, so `getstatic`
  answered 16 and everything worked — for as long as the fixup has existed;
* **JIT-compiled** code lowers `getstatic …:J` to a 64-bit load of the slot
  (`FIELD_CELL_PAYLOAD64_OFFSET`), and over an `Int`-tagged cell that word is
  whatever sits next to it — a measured `0x7ff700000000` instead of `16`.

So `getShortUnaligned(b, 0x7ff700000000 + off)` read from an address unrelated
to the array, and every field parsed out of a zip header was garbage. That is
the whole cluster: `invalid entry compressed size (expected 4259840 but got 2
bytes)`, `invalid compression method`, `only DEFLATED entries can have EXT
descriptor`, `invalid entry size (expected 80 but got 1321 bytes)` — four
spellings of one wrong base offset.

The same defect is what made `Arrays.equals(long[], long[])` return true for
unequal arrays (`docs/internal/fixed-suite-bugs/hibernate/batchtest-jit-duplicate-batch-insert-unique-violation-20260804.md`);
`ArraysSupport.mismatch` reads `ARRAY_INT_BASE_OFFSET` the same way.

## How it was narrowed (the useful part for next time)

Five levers, no rebuild, ~10 s per run, on the binary that reproduces:

| step | lever | result |
|---|---|---|
| oracle | default vs `--nojit` | FAIL 4/37 vs PASS |
| does compilation matter? | `CRATONVM_DBG=jit-bisect-only=zzz` (allow nothing) | PASS — and census: 545 compiled, **0 OSR** |
| which package? | `jit-bisect-only=java/` \| `org/` \| `net/` \| `jdk/` \| `sun/` | only `java/` FAILs |
| which class? | `jit-bisect-only=java/util/zip/ZipUtils` | FAIL (7/37) — 2 methods, 4 compilations |
| which mechanism? | `CRATONVM_JIT=getstatic-helper` | **PASS** ⇒ the inline `getstatic` load, not the field info |
| confirm | `CRATONVM_JIT=-statics-index` | PASS (it also declines the inline load) |

**Two lever traps cost time here, both worth remembering:**

* `jit-bisect-only` matches **class names only** (`class_name.starts_with`).
  `jit-bisect-only=…ZipUtils.get16` therefore allows *nothing*, and reads
  exactly like "that method is innocent". Per-method isolation needs
  `CRATONVM_JIT=deny=<class.method substring>`, which does match methods.
* Both filters take comma-separated lists, but the **grouped** spelling cannot
  carry one: `CRATONVM_DBG` splits its own spec on `,` first, so
  `jit-bisect-only=a,b` parses as `jit-bisect-only=a` plus an unknown token `b`.
  Set `CRATONVM_JIT_BISECT_ONLY=a,b` directly for multi-prefix runs.

## Verification

Azure Linux, JDK 25, real-JDK mode. `dev`-old = `6b6fc9a0dc` (the binary this
page was filed against); `dev`-new = a build of current `dev`.

The attribution is exact, not inferred from dates alone:
`git merge-base --is-ancestor 4972cd9c91 6b6fc9a0dc` answers **no** — the fix
(committed 2026-08-04 19:41 UTC) is not in the binary that reproduces
(2026-08-04 13:29 UTC), and is in the one that does not.

`probes/ZipSpin.java` — reads a jar entry-by-entry, repeatedly, and compares
every iteration against the first:

| | entries | bytes | 100 iterations |
|---|---|---|---|
| HotSpot 25 | 120 | 414187 | OK |
| CratonVM `dev`-old | **99** | **308737** | then `ZipException` on iteration 1 |
| CratonVM `dev`-old `--nojit` | 120 | 414187 | OK |
| CratonVM `dev`-new | 120 | 414187 | **OK** |

Note the first row of `dev`-old: before it ever threw, it silently read **99 of
120 entries**. A jar loader that loses a fifth of its entries without an error
is the worst shape this bug had.

Spring Boot classes, one process per class, `dev`-new:

| Class | was (`dev`-old, JIT) | now |
|---|---|---|
| `loader.tools.ImagePackagerTests` | FAIL 4/37 | **PASS 37/37** |
| `loader.tools.RepackagerTests` | FAIL | **PASS 52/52** |
| `loader.jar.NestedJarFileTests` | FAIL | **PASS 34/34** |
| `loader.jar.SecurityInfoTests` | FAIL (both JIT and `--nojit`) | **PASS 3/3** |

## Still open, and NOT this bug

Both were in this page's original table because they shared the `--nojit`-passes
signature. Neither is fixed by `4972cd9c91`, and neither is a zip-header bug.

### 1. `core/spring-boot` `OriginTrackedYamlLoaderTests.canLoadFilesBiggerThan3Mb` — an OSR miscompile

Still FAILs on current `dev` with the same snakeyaml scanner error at line
142539 of the generated document (`ry` on its own line, where the appended line
is `- some list entry`). The test only does this:

```java
StringBuilder yaml = new StringBuilder();
while (yaml.length() < 4_194_304) { yaml.append("- some list entry\n"); }
```

so the **input snakeyaml is handed is already corrupt** — the failure is in
building the 4 MiB `StringBuilder`, not in parsing it.

Established (single runs, on a quiet-enough host):

* `--nojit` PASSes; `CRATONVM_JIT=-osr` PASSes ⇒ **OSR**, not the main compiler.
  The `jit-compiled` census for this run is 166 methods but there are **9 OSR
  entries**, which that census does not show (`put_osr`, not `put`) — among them
  the test method itself and `java/util/Arrays.fill([BIIB)V`.
* `CRATONVM_JIT=deny=Arrays.fill` still FAILs, so it is not the fill.
* `deny=canLoadFilesBiggerThan3Mb` and `deny=constructSequenceStep2` each PASS.

Unconfirmed leads (each observed **once**, and the host then became too loaded
to repeat them — do not treat these as narrowed): `-osr-dead-locals`,
`-kernel-reg-osr` and `-kernel-reg-locals` each made it PASS. If that survives
repetition it points at a local that the OSR entry's dead-mask says is dead
while a register home still holds a stale value — the coordinate-space family of
`docs/internal/fixed-suite-bugs/jit-osr-backedge-value-corruption-cluster.md`
and `…/jit/arrays-sort-long-osr-miscompile-FIXED.md` (whose fix,
`14a2740859`, is already in this build and does not cover this).

**Next step:** repeat those three levers 3× each on a quiet host before
believing them, then narrow with `deny` inside the OSR set. A standalone
reproducer of just the append loop (`SbGrow.java`, in this session's scratch)
did *not* reproduce — it timed out at 4 MiB on CratonVM and completed at 1 MiB,
so the minimal case still needs finding.

### 2. `loader/spring-boot-loader` `ZipContentTests` — heap, not headers

On current `dev` it no longer corrupts: it dies with
`OutOfMemoryError: Java heap space (alloc_array length 8192)` at `--Xmx 2g`
inside `nestedZip64CanBeRead`, and PASSes with `--nojit` at the same heap. So
the JIT arm has a materially larger footprint on this test. Different problem,
different page; recorded here only so the next reader does not re-file it as a
zip-header bug.
