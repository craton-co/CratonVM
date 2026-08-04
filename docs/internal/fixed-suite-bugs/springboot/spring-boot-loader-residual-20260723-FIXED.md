# Spring Boot loader residual — closed 2026-08-04

**Status: FIXED.** Retired from `docs/known-issues/springboot/spring-boot-loader-residual-20260723.md`.

## Scope

The `loader/spring-boot-loader` residual shard exercises nested jar URLs,
archive/launcher class loading, security metadata, nested file systems, and
large ZIP64 content. It was first closed on 2026-07-23 (15/15 PASS, both
modes), then regressed twice:

| round | `ZipContentTests` |
|---|---|
| 2026-07-23 closure | 29/29 PASS, 109.6 s JIT / 138.2 s no-JIT |
| 2026-08-02 full suite | **FAIL** at 149.2 s — 28/29 successful, 1 aborted |
| 2026-08-04 residual round | **CRASH** at 156.8 s — `OutOfMemoryError: Java heap space (alloc_array length 8192)` escaping `SbRunner.main`, empty `.out.log` |

The 2026-08-04 note filed this as a moving-young GC root-map problem, on the
strength of an escalating run of `reason=unregistered-jit-frame-on-stack`
non-moving fallbacks in the `.err.log`. That reading was half right: the
fallbacks are real and they are the reason the class is slow, but they are not
what killed it. **Six separate defects** were behind the regression and the two
residual failures found while verifying it. Four are in the VM's allocation and
GC paths; two are functional bugs in code the loader shard is the only suite to
exercise hard.

## What was actually wrong

### 1. `jit_newarray` retried young-only after a forced GC — the CRASH

`gc_alloc_array` (the interpreter's array path) has carried this comment for
months:

> `try_alloc_array_full` is deliberately used rather than the young-only
> `try_alloc_array` … once a non-moving JIT-safe sweep has left the young
> generation fragmented it can spill the request into old space. Retrying
> young-only here used to report OOM for a tiny array while most of the heap
> was available as old-generation headroom.

`jit_anewarray_object` and `jit_new_object` both use the `*_full` variants.
`jit_newarray` — the **primitive**-array helper — did not. `byte[8192]` is a
primitive array, and it is what assertj's `assertHasContent` allocates once per
ZIP entry, 65,537 times, inside `nestedZip64CanBeRead`.

Measured with `CRATONVM_DBG=gc-overhead` at the moment of death:

```
before=134847192 after=134267592 promoted=0 freed=579600 cap=1610612736
old_headroom=1042623544 freed_sliver=true old_gen_wedged=false streak=0
```

A 134 MB live set in a 1.5 GiB heap with **1042 MB of old-generation
headroom**, and `old_gen_wedged=false` — so the GC-overhead limit correctly
never fired. The heap was not full. The allocator refused to look at the free
gigabyte. The same class passed 29/29 under `--nojit`, where every array goes
through `gc_alloc_array`.

Fixed in `vm/src/jit/helpers.rs`. The same young-only retry was fixed on three
more allocation-failure paths that could only have taken over as the next OOM
site: `alloc_object_shared`, `create_exception_object_for_class` (where failing
replaces the exception the program actually threw), and `alloc_multi_array`.

### 2. `jit_newarray` forced a full STW GC before trying that spill

`gc_alloc_array` tries `try_alloc_array_full` *before* it forces a collection,
so a young generation that cannot fit a request spills into old gen and the
mutator continues; the spill arms `note_young_spill_pressure`, which schedules
the collection at the next native-call boundary where roots are pinned and
remappable. `jit_newarray` went straight from a failed young probe to a full
STW GC. On this class that was ~750 forced collections, one per 8 KB `byte[]`,
each freeing about 10 KB.

### 3. The young-spill relief hook could not see old-generation pressure

With (1) and (2) fixed, the class stopped crashing and started ballooning:

```
before=1555123224 … cap=1610612736 …
```

live reached **1.55 GB of a 1.61 GB capacity** before any collection ran, and a
*native* allocation — which cannot initiate a GC of its own, by design — raised
`OutOfMemoryError` in the gap. The collection that eventually ran freed 1.49 GB,
so nothing was leaking.

`safe_native_call`'s young-spill-pressure hook fires exactly when young could
not serve an allocation and the request went to old gen, then gated itself on
`needs_gc()` — a **young-only** predicate whose live term is small in precisely
that situation. So the relief never ran and old gen absorbed every spill with
nothing watching it. It now also consults `old_gen_needs_gc()`, the same 75 %
threshold both major-GC branches use, so the collection it admits is exactly the
one that reclaims old.

### 4. The non-moving young sweep degenerates monotonically

Root cause of the *slowness*, and the thing the 08-04 note was pointing at.

The non-moving sweep never compacts, so on a process that stays on it — any
process whose JIT frames the coverage proof cannot clear — the young arena
decays: the bump cursor reaches the top once and never resets (a survivor
anywhere forbids a reset), each cycle's survivors are a different few MB
scattered somewhere new, almost none live to `PROMOTION_AGE`, and the free-list
coalescer cannot merge across a survivor. After ~700 cycles:

```
young_used=536866904 young_free_list=529026112 young_largest_free=9608
young_cap=536870912   sp_sweeps=752 sp_selective=16 promoted=0
```

**98.5 % of the arena free, largest contiguous hole 9,608 bytes** — smaller than
the 8 KB array that started this.

`sweep_young_non_moving` now escalates: once the arena is bump-exhausted **and**
its largest hole falls under 64 KiB, it tenures every unpinned survivor
regardless of age. That is the drain selective promotion already implements and
already argues safe (it pins by raw slot **value**, so a conservative false
positive pins some object rather than mis-relocating one); the age gate was
never part of that safety argument. Self-limiting: one escalated cycle restores
a huge largest-free-block, so the predicate goes false again. Kill switch
`CRATONVM_GC=-defrag-promote`.

### 5. `Unsafe.ARRAY_*_BASE_OFFSET` read as 8 from compiled code

Pre-existing on `dev`, and the cause of the second `ZipContentTests` failure
(`entryWithEpochTimeOfZeroShouldNotFail`, expected `1970-01-01T00:00:00Z` but
got `1980-01-01T00:00:00Z`) — which passes cold and fails once the class is
warm.

`jdk/internal/misc/Unsafe.ARRAY_*_BASE_OFFSET` is declared `J` on JDK 21+
(`getstatic … ARRAY_BYTE_BASE_OFFSET:J` in `ZipUtils.get16`'s bytecode). The
post-`<clinit>` fixup wrote `Value::Int(16)`. The interpreter coerces on read
and saw 16, so nothing looked wrong — but the JIT's inline `getstatic`
(`try_emit_inline_getstatic`, the direct-load path added 2026-08-03) dispatches
on the **descriptor**: for `J` it loads the cell's 64-bit payload word, which an
`Int` cell never wrote. Compiled code read **8**.

`ZipUtils.get16` is `getShortUnaligned(b, off + ARRAY_BYTE_BASE_OFFSET)`, so
every compiled ZIP central-directory parse addressed 8 bytes before the array
data and the extra-field walk silently found nothing — the entry fell back to
its DOS timestamp. Isolated to a four-line probe:

```
foreignWide (Unsafe.ARRAY_BYTE_BASE_OFFSET) bad=98726 first=1272 last=8
ownWide     (own class, long static)        bad=0
foreignWideInt (ARRAY_BYTE_INDEX_SCALE, I)  bad=0
```

`set_static_by_name` now coerces every fixup value to the variant its field's
descriptor declares. This affected far more than ZIP parsing: every JIT-compiled
`ARRAY_*_BASE_OFFSET` user was reading 8.

**Found twice, independently, on the same day.**
`fix/hib-batchtest-jit-batch-binding-20260804` reached the same slot from the
other end — compiled `ArraysSupport.mismatch` handed its intrinsic an offset of
`0x7ff700000000`, the range check failed, it returned `-1` ("no mismatch"), and
`Arrays.equals(long[],long[])` answered `true` for arrays that differ, which is
what made H2 report a unique-index collision that did not exist. That branch's
version of the coercion (which refuses, loudly, rather than writing a mistyped
slot when no conversion exists) landed on `dev` first and is the one kept here;
this branch's duplicate was dropped in the merge. See
`fixed-suite-bugs/hibernate/batchtest-jit-duplicate-batch-insert-unique-violation-20260804.md`.

### 6. An unpinned `ObjectRef` in the Spring `ZipInflaterInputStream` bridge

Pre-existing on `dev`. Cause of the two `--nojit` residual failures found while
verifying the shard (`SecurityInfoTests.getWhenJarIsSigned`,
`NestedJarFileTests.verifySignedJar`, both
`[open paths] Expecting empty but was: [bcprov-jdk18on-1.78.1.jar]`).

`native_sb_zip_inflater_init` pinned `this` and read it back, but stored the
source stream into `this.in` straight out of `args[1]` — a raw `ObjectRef`
captured before `drain_input_stream_bulk` and `new_array`, either of which can
run a moving young collection. When one did, `in` held the pre-collection
address, so `InflaterInputStream.close()` closed whatever occupied that slot
afterwards and never closed the real `DataBlockInputStream`, leaving its
`FileDataBlock` reference count above zero and the file channel open.

Of the 5,370 signed entries those tests stream, **2 to 3 leaked per run, and
which ones changed between runs** — a GC-timing signature, not a logic one:

```
HotSpot -Xint : supplierCalls=5370 suppliedStreamsClosed=5370
CratonVM      : supplierCalls=5370 suppliedStreamsClosed=5367
```

### Also: A5 return-address validation

`native_stack_has_jit_frame` is a raw word scan of the native stack above the
outermost registered JIT entry: any word landing inside a JIT code range counted
as a live unregistered compiled frame, and one such word diverts every young
collection in the process to the non-moving sweep. Its own doc comment named
return-address validation as the designated follow-up; that is now done
(`is_plausible_return_pc` requires the preceding bytes to decode as the tail of
an x86-64 near call). Kill switch `CRATONVM_JIT=-retpc-validate`.

It did **not** clear this workload's fallbacks. The word that trips the probe
here (`stack slot 0x…dad8 holds 0x…404c`, preceded by `bb 50 5d 00 00 ff d0` —
`mov $0x5d50,%ebx; call *%rax`) is a genuine return address into compiled code
that is no longer live: dead content in a live frame's uninitialised slot,
which no byte-level filter can distinguish from a live one. Separating those
needs a frame walk, and is left open — see "Still open" below.

## Verification

Azure Linux host, JDK `25.0.3+9`, fixture root
`/data/data/springboot-jsonreader-deprecation-20260718`, binary
`cratonvm-zipgc-r10` built from this branch, `-Xmx 2g`, `-Parallel 2`.

Whole `loader/spring-boot-loader` module — **54 classes, a strict superset of
the 15-class residual shard this document was opened for**:

| mode | PASS | FAIL | CRASH | HANG | EMPTY | shard wall |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| JIT | **52** | 0 | 0 | 0 | 2 | 396.6 s |
| no-JIT | **52** | 0 | 0 | 0 | 2 | 311.5 s |

The two `EMPTY` classes are correct: `AbstractLauncherTests` is declared
`abstract`, and `VirtualZipPerformanceTests` is
`@Disabled("Only used for manual testing")`.

`ZipContentTests` on its own: 29/29 PASS, 0 failed, 0 aborted, in both modes.
The two `--nojit` failures from §6 were re-verified 3/3 runs each after the fix.

Unit tests added with the fixes (`cargo test -p cratonvm-gc --lib`,
`-p cratonvm-vm --lib`): the measured degenerate arena is defragmentable and a
coalesced one is not; an `Int` fixup widens into a `J` field and a matching one
is left alone; the near-call decoder accepts `E8 rel32` / `FF /2` (with and
without REX) and rejects `mov %rax,-0x8(%rbp)`, a displacement `FF`, and
`FF /1`. `cratonvm-gc --lib` is 968/968. `cratonvm-vm --lib` is 2395/2396; the
one failure, `layout_immunity_is_not_open_coded`, is a source-scanning guard
over a file this branch never touched and **fails identically on unmodified
`dev`**.

## Still open (not blocking this closure)

**Throughput.** `ZipContentTests` passes but is ~5x HotSpot. Interleaved
A-B-B-A on the same host, same window:

| arm | run 1 | run 2 |
| --- | ---: | ---: |
| CratonVM (JIT) | 369.5 s | 214.0 s |
| HotSpot | 37.3 s | 40.2 s |

(Host load fell from ~16 to ~7 across the set, which is most of the CratonVM
spread; the arms are interleaved in both orders so the ratio survives it.)

The remaining gap is §4's cause, not its symptom: the young generation still
runs the non-moving sweep for this whole workload because the A5 probe keeps
finding a stale-but-genuine return address, so it never compacts and the defrag
escalation is doing work a copying collector would not have to do. Two things
would close it, and both are GC work rather than loader work:

1. **Liveness for the A5 scan.** A byte-level filter cannot tell a live return
   address from a dead one at the same address; a frame walk can. See
   `is_plausible_return_pc`'s doc comment for what the byte filter does and does
   not buy.
2. **`unrewritable_peer_state` breadth.** Selective promotion ran on only 16 of
   752 sweeps in the pre-fix measurement, because the blocked-peer helper-window
   pass sets the gate on nearly every cycle in a process with any parked thread.
   The gate's hazard (a frozen peer's derived/interior pointer) is real; whether
   it needs to be this wide is not settled, and narrowing it is the single
   highest-leverage change left for JIT-active workloads.
