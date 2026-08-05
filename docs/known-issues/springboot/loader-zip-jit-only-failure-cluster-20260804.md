# Spring Boot loader/zip: the JIT-only failure cluster, root-caused

**Status: the loader/zip half is CLOSED.** Root cause found 2026-08-04 and
already fixed on `dev` by `4972cd9c91` ("the post-clinit fixup wrote Unsafe's
long base-offsets as 32-bit ints"), which landed after the binary this page was
originally filed against. Verified below. Two members of the original table
were **not** this bug and are re-scoped as separate open issues at the bottom.

> **2026-08-05 re-verification — read this before acting on the two open items.**
> Both are **NOT REPRODUCIBLE**, including on the pre-fix binary they were filed
> against, and the second one's "JIT-only" framing is **falsified**. Details in
> [Re-verification](#re-verification-2026-08-05). The page is deliberately NOT
> retired: the failures were real when observed, and nothing here identifies a
> fix for them.

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

**Correction (2026-08-04, later):** this page previously asserted that the
input snakeyaml is handed "is already corrupt", i.e. that the failure is in
building the 4 MiB `StringBuilder` rather than in parsing it. That was an
*inference* from the test body being only the append loop, and it does not
follow — snakeyaml's own parser is JIT-compiled in the same run, and this
page's own datum that `deny=constructSequenceStep2` (a **snakeyaml** method)
makes it PASS points the other way. Treat "which side is corrupt" as **open
and unmeasured** until `probes/YamlSplit.java` answers it: that probe verifies
the document byte-for-byte *before* handing it to snakeyaml, so it separates a
corrupt build from a miscompiled parse by measurement instead of by argument.

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

**2026-08-05: this item is NOT REPRODUCIBLE and therefore cannot be narrowed.**
11 runs on the pre-fix binary (3 at 2 g, 2 at 1 g, 6 pinned to 1/2/4 cores) all
PASS — see [Re-verification](#re-verification-2026-08-05). Every lever below
answers a question about a failure that no longer occurs, so running them now
would produce a table of PASSes that means nothing. Do **not** read this as
fixed: no fix is identified, no commit is attributed, and OSR still enters the
method. What changed between then and now is unknown; the leading candidate is
whatever the loaded host (load 30+) supplied that an idle one does not, and heap
size and CPU count are not it.

The one thing worth doing when it next appears: capture the failing run's full
stderr *at that moment*, plus `CRATONVM_DBG=osr` and the host's load, before the
window closes. This page's `deny=` verdicts were single runs taken during such a
window and two of them (`canLoadFilesBiggerThan3Mb` and `constructSequenceStep2`
each "PASS when denied") cannot both name a sole culprit — `deny` perturbs
compile scheduling globally, so an unrepeated PASS from it is weak evidence.

**Original next step, retained for whenever it reproduces:** repeat those three
levers 3× each on a quiet host before believing them, then narrow with `deny`
inside the OSR set. A standalone
reproducer of just the append loop (`SbGrow.java`, in this session's scratch)
did *not* reproduce — it timed out at 4 MiB on CratonVM and completed at 1 MiB,
so the minimal case still needs finding. That timeout was measured while the
host was above load 30, so it is not evidence of anything.

`probes/yaml-osr-lever-matrix.sh` is that next step, written out: it runs every
lever **3×**, records `/proc/loadavg` next to each verdict so a load-poisoned
row can be discarded rather than believed, and adds one lever this failure has
never been run against — `CRATONVM_JIT_OSR_SEED_FRAME_SLOTS=1`, the
trampoline's frame-slot-store elision. That elision is the standing suspect
for "the compiled body reads a local from its FRAME SLOT on some path the
trampoline only seeded into a REGISTER", and it is listed in
`…/jit/arrays-sort-long-osr-miscompile-FIXED.md` as one of the four levers that
investigation added but this one never tried.

**Why it is Linux-only, mechanically:** `x64::LOCAL_REGS` is 7 registers on
Windows (`R12–R15, RBX, RSI, RDI`) and **5** on System V (no `RSI`/`RDI`).
Linux therefore runs this code at materially higher register pressure, which is
what produces the coalescing the OSR dead-mask exists to handle. That is a
concrete reason a Windows reproduction attempt is expected to come back green
and must not be read as an exoneration:

| probe | host | OSR fired? | verdict |
|---|---|---|---|
| `probes/YamlGrow.java` (verbatim append loop) | Windows, JDK 25 | **yes** — `[cratonvm-osr] enter YamlGrow.main entry_pc=50` | PASS (does **not** reproduce) |
| same | HotSpot 25 control | n/a | PASS |

The OSR-fired column is the part that makes that a real negative rather than a
vacuous one ([[reference_jit_regression_fixture_must_prove_it_compiles]]).

## Re-verification 2026-08-05

Azure Linux, 16 cores, **idle** (load ~1–2), JDK 25, real-JDK mode, `--Xmx 2g`
unless stated. Two binaries:

* **new** = `2572ea9afe` (current `dev`, 74 commits after this page was filed);
* **old** = `fe886b08c` — *the code this page was measured against*. It is a
  docs-only commit, so its code is identical to its parent.

### Every class in the page's tables passes on current `dev`

Three runs each, all `containersFailed=0`, and the test counts match the page's
own numbers, so none of these is a vacuous zero-test run:

| class | tests | result |
|---|---|---|
| `loader.tools.ImagePackagerTests` | 37 | PASS 3/3 |
| `loader.tools.RepackagerTests` | 52 | PASS 3/3 |
| `loader.jar.NestedJarFileTests` | 34 | PASS 3/3 |
| `loader.jar.SecurityInfoTests` | 3 | PASS 3/3 |
| `loader.zip.ZipContentTests` | 29 | PASS 3/3 |
| `OriginTrackedYamlLoaderTests.canLoadFilesBiggerThan3Mb` | 1 | PASS 3/3 |

### The control: neither open item reproduces on its own pre-fix binary

A green only means something if the oracle can go red on the code that was
failing. It cannot:

| arm (binary **old** = `fe886b08c`) | runs | result |
|---|---|---|
| yaml, `--Xmx 2g`, idle | 3 | **all PASS** |
| yaml, `--Xmx 1g` | 2 | **all PASS** |
| yaml, pinned to 1 / 2 / 4 cores (`taskset`) | 6 | **all PASS** |
| `ZipContentTests`, `--Xmx 2g` | 3 | **all PASS** |

Heaps at or below 512m are not evidence either way: the test legitimately
`OutOfMemoryError`s there, since it builds a 4 MiB document and then parses
233 k entries out of it.

**So current `dev` passing is not evidence of a fix.** 74 commits landed in
between, but nothing here attributes the change to any of them — the failure is
absent from the *before* binary too. GC pressure (via heap) and CPU starvation
(via `taskset`) were both tried because the original measurements were taken on
a host above load 30 and this one is idle; neither brought it back. Raising load
on the host itself was deliberately not done — it would invalidate every other
session's runs on this shared box.

### It is not "masked by the tiering changes" either

Four commits in the range change tier-up/OSR triggering, so the obvious
hypothesis was that OSR simply no longer enters the test method. It does.
`CRATONVM_DBG=osr` on **both** binaries lists the same sites:

```
enter java/util/Arrays.fill([BIIB)V entry_pc=10                     (x5 new, x6 old)
enter OriginTrackedYamlLoaderTests.canLoadFilesBiggerThan3Mb()V entry_pc=8
enter snakeyaml BaseConstructor.constructSequenceStep2(...)  entry_pc=10
enter YamlProcessor.lambda$buildFlattenedMap$0(...)          entry_pc=144 (x2)
```

That is the page's own census ("9 OSR entries … among them the test method
itself and `java/util/Arrays.fill([BIIB)V`"). The OSR path is still compiled and
still entered; the test just does not fail. A latent OSR miscompile is therefore
**not excluded** — only unreproducible.

### Harness correction — one that could have scored a red as green

Both oracles decided PASS with `case "$res" in *failed=0*)`. That substring also
occurs inside `containersFailed=0`, so a run that failed to even load its test
class (`tests=0 … containersFailed=1 LOADFAIL`) reported **PASS**. Fixed to
extract each field and to treat `tests=0` as a vacuous failure; the FAIL path
was then confirmed to fire before any verdict above was trusted.

The bug can only turn a red into a green, never the reverse, so this page's
original FAIL observations stand unaffected — and its PASS rows all carry real
test counts (37/37, 52/52, …), so none of them was a mis-scored load failure.

### 2. `loader/spring-boot-loader` `ZipContentTests` — heap, not headers

On current `dev` it no longer corrupts: it dies with
`OutOfMemoryError: Java heap space (alloc_array length 8192)` at `--Xmx 2g`
inside `nestedZip64CanBeRead`, and PASSes with `--nojit` at the same heap. So
the JIT arm has a materially larger footprint on this test. Different problem,
different page; recorded here only so the next reader does not re-file it as a
zip-header bug.

Note the failing allocation is **8192 bytes** — a small array. The heap was
already exhausted by something else, so this is "much more was allocated or
retained", not "one absurd length was computed from a garbage header". The two
have different fixes, and the lever matrix separates them: it runs the class
under `CRATONVM_DBG=gc-overhead` on **both** arms at the same `--Xmx 2g`, so a
live set that matches `--nojit` means the collector is not getting to garbage,
while a genuinely larger live set means something is pinning.

**2026-08-05: "JIT-only" is falsified, and the symptom is not the OOM.**
Re-measured on current `dev`, whole class, `--Xmx 2g`:

| arm | class-level runs | failures |
|---|---|---|
| JIT | 9 | **0** |
| `--nojit` | 9 | **1** |

The `--nojit` failure is `nestedZip64CanBeRead` — the very test this section
names — and it is not an `OutOfMemoryError`:

```
java.io.IOException: Zip64 'End Of Central Directory Record' not found at
position 8027398. Zip file is corrupt or includes prefixed bytes which are not
supported with Zip64 files
```

A `--nojit` arm that fails at any rate at all disposes of "the JIT arm has a
materially larger footprint": whatever this is, disabling the compiler does not
prevent it. One observation is a rate of about 1-in-9, not a proof of
JIT-independence, but it is enough to retire the footprint story as *stated*.

The failure also needs the **whole class**. Run on its own,
`nestedZip64CanBeRead` passed **40/40** — 10 reps each across
{JIT, `--nojit`} × {old, new}. So the trigger is something the other 28 tests
leave behind (temp files, accumulated heap, or shared static state), not a
property of that test. Disk and memory were ruled out at the time of the
failure: 15 G free on `/tmp`, 25 G RAM available.

**ROOT-CAUSED 2026-08-05 - it is the DISK, and it is not a VM bug.**

`ZipContentTests` builds a zip that deliberately exceeds the ZIP SIZE LIMIT
(`openWhenZip64ThatExceedsZipSizeLimitOpensZip`) plus a 65537-entry zip64. One
run takes free space on `/` from **15219 MB down to 143 MB** - measured by
sampling `df` for the duration of a single solo run. The host has ~15 GB free,
so the class barely fits, and *anything* else using disk at the same time tips
it over:

```
MethodSource [... methodName = 'openWhenZip64ThatExceedsZipSizeLimitOpensZip']
=> java.io.IOException: No space left on device (os error 28)
```

That reproduces on demand: a solo run started with 7.2 GB free failed exactly
this way, and the same class passes 12/12 when the disk is empty. Six
concurrent copies fail 6/6 - which is why an earlier "6/6 reproduction" in this
investigation was an artifact of the harness, not a finding.

This explains every observation on this item:

* **why `--nojit` fails too** (1 of 9 whole-class runs) - the disk does not care
  whether the compiler is on, which is what falsified "JIT-only";
* **why it is intermittent** - it depends on what else is on the disk;
* **why it passes 12/12 sequentially on an idle host** but was filed from a host
  above load 30, where concurrent builds and test runs were consuming the disk;
* **why the symptom sometimes reads as zip corruption** - a file truncated by
  ENOSPC produces `Zip64 'End Of Central Directory Record' not found at position
  8027398`, which looks exactly like a header bug and is how this got into a JIT
  cluster page in the first place.

**Guarded so it cannot be re-filed as a VM bug.** `sb-class-oracle.sh` now
checks free space before running this class and emits
`SKIP-INSUFFICIENT-DISK <n>MB free, need >=16000MB` instead of a FAIL verdict.
Override with `ZIP_NEED_MB`.

The original `OutOfMemoryError: Java heap space` spelling is NOT explained by
disk and is not re-tested here; if it returns at `--Xmx 2g`, that is a separate
question from this one.

If the OOM spelling does come back, the first thing to run on it is one env var:
`CRATONVM_JIT=getstatic-helper`.

## The root cause's second door — FIXED 2026-08-05 (`4ac429f2f`)

Auditing the family this page root-causes turned up a live instance of it that
`4972cd9c91` did not cover. The hazard is a property of the **slot**, not of the
writer that was patched:

* `try_emit_inline_getstatic` takes its load width purely from the field
  **descriptor** and never checks that the slot's runtime `Value` tag agrees;
* `StaticsBlock::new` fills every slot with `Value::Int(0)`;
* `set_static_shared` built its block that way whenever a static was written
  **before its class was prepared** — `prepare_class` seeds
  `default_value_for_descriptor` per slot, but this path bypassed it.

So a `J`/`D` static reaching compiled code through such a block was mistyped by
construction: the interpreter widens it to a correct `0`, and JIT-compiled code
pulls 8 bytes at `FIELD_CELL_PAYLOAD64_OFFSET` over a 4-byte payload — the exact
mechanism that turned `ARRAY_*_BASE_OFFSET` into `0x7ff700000000`.

Both doors now seed through one helper, `typed_default_static_slots`. Block
length is unchanged (the class's total field count, while slot indices are the
static-field enumeration order); only the seeded slot *types* change.

The tests assert the `Value` **width**, not the numeric value — every arm is
zero either way, so a value-based assertion could not fail. Confirmed
non-vacuous by injecting the old blanket fill: both new tests go red (`J must be
Long, got Int(0)`) and pass again when it is reverted. All 29 statics unit tests
stay green, and on Linux all six classes in this page's tables still pass with
the fix in (29/34/3/37/52/1).

This is a latent-defect fix, **not** a fix for either open item above — neither
of those reproduces, so nothing here can be claimed to close them.
