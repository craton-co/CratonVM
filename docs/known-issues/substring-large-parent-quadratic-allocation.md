# Extracting many small strings from one large parent `String` scales O(n²), not O(n) — root cause NOT YET FOUND

Status: OPEN — confirmed, reproduced with zero regex/Matcher involvement, ruled out
several plausible causes; true root cause still unknown, needs a dedicated GC/allocator
investigation.

Found: 2026-07-11, as the corrected root cause behind a user-reported
`StringBuilder` + `Pattern.compile().matcher()` + `while (m.find()) { m.group(1); }`
benchmark whose CratonVM-vs-JDK-25 slowdown ratio grew with input size (18.8× at
1,000 entries → 94.4× at 5,000 → 238.8× at 10,000). The original investigation
misattributed this to a bug in `Matcher`'s native bridge
(see [`matcher-native-full-input-redecode-quadratic.md`](matcher-native-full-input-redecode-quadratic.md),
now corrected) — that bridge is provably **not** the active dispatch path in
real-JDK mode (confirmed via runtime instrumentation: real JDK bytecode runs
`Pattern`/`Matcher` unconditionally). This doc covers the actual, still-open bug.

## Symptom

Extracting many small (fixed-size) substrings from one large, already-built parent
`String` gets progressively slower **per extraction** as the parent grows — even
though each individual extraction only touches a small, bounded number of
characters. Reproduces with **zero regex/Matcher code** — pure `String.substring()`.

## Quantification

P-core-pinned (`ProcessorAffinity=0xFFFF`), `target/release/cratonvm.exe`, JDK-25,
checksums identical at every size (pure perf bug, not correctness).

`bench/SubstringOnly.java`: build a parent `String` of length ~10n *outside* the
timed region (`sb.append("value").append(i).append(",")` × n), then loop n times
extracting a 5-char substring at an advancing offset (`text.substring(pos, pos+5)`,
wrapping `pos` back to 0 periodically) — no regex, no Matcher, nothing but
`StringBuilder` (already confirmed correctly linear) and `String.substring`:

| entries | CratonVM | ms/entry |
|---|---|---|
| 1,000  | 14 ms   | 0.014 |
| 5,000  | 316 ms  | 0.063 |
| 10,000 | 1283 ms | 0.128 |

Per-entry cost roughly doubles every time n increases — the O(n²) signature.
`bench/UnrelatedAllocOnly.java` (same parent `String` built and kept alive via a
static field for the whole run, but each loop iteration does `new String("abcde")`
— an allocation *unrelated* to and not *derived from* the large parent) stays
**perfectly linear**: 2 ms / 11 ms / 22 ms (0.002 ms/entry at every size). So a large
live object sitting in the heap is not, by itself, sufficient to reproduce this —
it specifically requires deriving/copying a small string *from* the large one.

`bench/RegexFindGroupOnly.java` (`find()` + `group(1)`, no `Long.parseLong`) shows
the identical curve to the full original benchmark (51 ms / 546 ms / 1775 ms at
1K/5K/10K), while `bench/RegexFindOnly.java` (`find()` only, no `group()` call at
all) is close to linear (26 ms / 127 ms / 249 ms — ~2× growth for 2× n, not 4×).
So within the regex benchmark specifically, **`group(1)` is the trigger**, not
`find()` or `Long.parseLong`. This is consistent with `group(int)`'s real-JDK
bytecode ultimately doing the same kind of "extract from parent" operation
`String.substring()` does.

## Dispatch verified: this is REAL JDK bytecode, and its call chain is provably NOT the source

`String.substring(int,int)` is not force-dispatched to CratonVM's native
(`native_string_substring`, `native-builtins/src/lang_string.rs`) in real-JDK mode —
it's absent from both `force_native_over_real_jdk_bytecode`
(`vm/src/runtime/interpreter.rs`) and the `check_override` slow-path allowlist
(`vm/src/vm/vm_exec.rs`), and `java/lang/String` is not in the
`drop_real_layout_synthetic` class list. So real JDK 25's own
`substring`→`checkBoundsBeginEnd`→`isLatin1`→`StringLatin1.newString`/
`StringUTF16.newString`→`Arrays.copyOfRange`→`System.arraycopy` bytecode chain
runs. Traced each hop's dispatch:

| call | dispatch | bound by |
|---|---|---|
| `String.checkBoundsBeginEnd` | native (`native_string_check_bounds_begin_end`) | O(1) |
| `StringLatin1/UTF16.newString` | real bytecode (no native registered for it) | — |
| `Arrays.copyOfRange` | native (`phases_early.rs`) | `to - from` (requested length) |
| `System.arraycopy` | native/intrinsic (`lang_system.rs`) | `length` arg (requested length) |

**Neither `copyOfRange` nor `arraycopy` ever reads/writes more than the requested
substring length** — verified directly in their implementations (loop bounds are
the copy length, not the source array's total length; `arraycopy`'s primitive-array
fast path uses `ctx.bulk_array_copy(src, src_pos, dest, dest_pos, length)`, a single
bounded `copy_nonoverlapping`). So the O(n²) is **not** in this call chain's own
arithmetic — every step here is O(subLen) or O(1). The cost must be coming from
something else these calls trigger as a side effect (most plausibly: allocation).

## What's been ruled out

- **Not `StringBuilder`** — `StringBuilderOnly` (append loop alone, no
  substring/regex at all) is perfectly linear (0.002 ms/entry at every size,
  `sb_ensure_capacity`'s amortized-doubling growth confirmed correct).
- **Not the regex/Matcher native bridge** — see the corrected doc above; that
  code is provably unreachable in real-JDK mode.
- **Not the substring/arraycopy call chain's own algorithm** — see the dispatch
  table above; every hop is bounded by the requested copy length.
- **Not merely "a large object is live in the heap"** — `UnrelatedAllocOnly` keeps
  the exact same large parent `String` alive (via a static field) for the whole
  run, while allocating the same *number* of small objects per iteration, and
  stays linear. The parent's mere presence/liveness isn't sufficient; it must be
  specifically the copy/derive operation.
- **Not one or a few catastrophically slow GC pauses** — `CRATONVM_DBG_GCPAUSE=1`
  (which logs any single collection taking ≥100 ms) produced zero output during a
  1253 ms `SubstringOnly` n=10,000 run, so the cost is not concentrated in a small
  number of large stop-the-world pauses.
- **Not simply "more/bigger young generation fixes it"** — re-running with
  `--Xmx 4g` (which per `reference_default_heap_ergonomics`'s "each young
  semi-space = maxheap/4" sizing rule should give a young generation roughly
  16× the small-heap default) made no measurable difference on the original
  combined `RegexOnly` benchmark. This is suggestive but not conclusive since it
  wasn't re-run against the isolated `SubstringOnly` bench specifically — a
  clean re-test of `SubstringOnly` under a much larger `-Xmx` is a good next
  step to fully confirm/refute the nursery-size angle in isolation.

## Leading (unconfirmed) hypothesis

Every `substring()`/`group()` call allocates at minimum one fresh backing array
(via `copyOfRange`'s `ctx.new_array`) plus a `String` wrapper — real, if small,
per-call allocation churn. `UnrelatedAllocOnly`'s `new String("abcde")` (the
`String(String)` copy constructor) does *not* allocate a fresh backing array —
it just copies the `value`/`coder` field references from the argument, so it's a
much lighter allocation per iteration than `substring()`'s "new array + new
wrapper." If some GC- or allocator-internal operation costs O(size of the large,
still-live parent object) *whenever it runs* — e.g. a free-list/region scan, or a
scan-and-relocate pass over already-old-generation-promoted objects — then
`SubstringOnly`'s heavier allocation rate would trigger that operation often
enough (relative to n) to produce the observed O(n²), while
`UnrelatedAllocOnly`'s lighter allocation rate would trigger it too rarely, within
the tested range, to show up as more than linear noise. This is consistent with an
earlier finding in a different benchmark family (bintrees, 2026-07-10 session)
that its real cost was "young-GC sweep+spill+free-list" rather than the algorithm
itself — possibly the same underlying mechanism. **Not yet confirmed** — the ≥100ms-pause
probe and the `-Xmx 4g` test both argue against the simplest form of this
hypothesis (few large pauses; more nursery headroom not helping), so either the
mechanism is different from plain "GC runs more often as parent grows," or the
per-collection cost is spread across many *sub-100ms* pauses that "GCPAUSE" doesn't
surface, or the relevant cost isn't in "collection" proper but in mutator-side
allocation/free-list bookkeeping that runs on every `new_array`/`alloc_object` call
regardless of whether a collection is triggered.

## Suggested next steps (not attempted this session)

1. Re-run `SubstringOnly` specifically (not just the original combined regex
   benchmark) under a much larger `-Xmx`/young-gen to fully confirm or refute the
   nursery-size angle in isolation — the earlier `-Xmx 4g` test was on the wrong
   benchmark.
2. Get a real per-collection *count* over the run (not just a ≥100ms-pause
   filter) — e.g. read `NativeContext::gc_collection_count()` before/after the
   loop from the Java side via a debug hook, or add a raw (unconditional,
   temporary) `eprintln!` counter — to determine whether collection *frequency*
   scales with n at all, independent of individual pause cost.
3. If collection frequency does scale with n, profile a single collection's own
   internal cost breakdown (`gc/src/gen_heap.rs`) for O(size-of-large-live-object)
   work — e.g. does scanning/copying/card-marking a large young- or old-gen object
   cost proportional to its own size on *every* collection regardless of whether
   it moved, versus HotSpot's approach where a stable object costs O(1) per
   collection once promoted/tenured?
4. If collection frequency does NOT scale with n, look at the mutator-side
   allocation path itself (`ctx.new_array`/`alloc_object`/`bulk_array_copy`'s
   allocation, not the copy) for something that scans/walks proportional to
   total heap occupancy or free-list length on every call — e.g. a linear
   free-list search that gets longer as the heap fragments, independent of GC
   cycles.

## Severity

High. `String.substring()`/`Matcher.group()`-from-a-large-parent is an extremely
common pattern — any parser, tokenizer, log scraper, or CSV/JSON reader that
builds one large buffer and then repeatedly extracts small fields from it hits
this. Unlike the (corrected, now-inert) Matcher native-bridge bug, this affects
the **default real-JDK build path** directly and is not specific to regex at all.
