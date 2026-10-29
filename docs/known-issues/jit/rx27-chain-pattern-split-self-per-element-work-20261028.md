# `cached`: what `Pattern.split`'s compiled body still does per element (29 % self)

**Status:** OPEN (round rx27 waves 1 and 3, lane chain, 2026-10-28). By reading only; not profiled in the lane.
Wave 3 re-read it against `jit-regex-r27` `dc5f532718` (lane bimodal's keep-loop rules, and the
substring / subSequence prefixes, which are now default ON).

**Area:** the optimizing compile of `java/util/regex/Pattern.split(Ljava/lang/CharSequence;IZ)`. In the
default build it is its own root (W13-6: `anewarray`). Rows: `cached` (`WS.split(TEXT)`, about 3.7x HotSpot
with the bimodal fix), and `split-ws` / `regex-split` through `String.split`.

**Severity:** medium.

## What the body does per element (JDK 25 `Pattern.java` 1398-1445)

`TEXT` has about 16 whitespace runs, so the `while (m.find())` loop runs about 17 times per op.

1. **`m.find()`, spliced (about 250 bytes).** `find`'s region checks, then `search`:
   * the `hitEnd` / `requireEnd` / `first` / `oldLast` stores;
   * the `groups` reset loop (2 entries for `\s+`) and the `localsPos` loop (length 0);
   * `acceptMode`;
   * then `parentPattern.root.match(...)`. This stays a call into `Start.match`'s own C2 artifact. Since
     lane bimodal's keep-loop rules, that artifact splices `BmpCharPropertyGreedy` consistently, so the scan
     is counted under `Start.match`, not here;
   * `oldLast`, `modCount++`.
2. **`m.start()` / `m.end()`**, once each per element (plus one more of each on the first element, while
   `index == 0`). Each is a `checkMatch()` test and a field read.
3. **`input.subSequence(index, m.start())`.** Under `CRATONVM_JIT_IR_SEQ_SUBSEQUENCE_PREFIX` (default ON since
   wave 2), this is an allocating inline prefix with the site kept as a call. Before, it was the spliced
   `String.substring` chain (145 bytes). This is lane splitfast's area.
4. **`.toString()`**: `String.toString` returns `this`. Check that it is spliced, not called.
5. **`matchList.add(match)`**: `modCount++`, the grow test, an `aastore` into `Object[]` (lane E) and
   `size = s + 1`.
6. **`withDelimiters`**: a constant `false` from `split(CharSequence,int)`, tested per element because
   `split(CS,I,Z)` is its own root. It costs one test and branch, which is noise.

## Lane chain's share: nothing left to change here

* The `root.match` call (item 1) is one call per `find`. Splicing `Start.match` into this loop is the W13-6
  move that cost +8 to 12 %: a loop root moving into a caller that cannot hold its loop. Wave 1's
  `CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE` refuses that move, and it measured flat.
* The `split` overloads are admitted as they should be, so nothing in chain admission is left on this body.
* The remaining per-element work belongs to other lanes: the prefixes (lane splitfast), allocation (lane D),
  the `checkMatch` field tests (loop-invariant work, lane A / scanloop).

## What would settle the shares

With the current tip binary:

```
CRATONVM_JIT_PERF_MAP=1 perf record -e instructions:u -o ps.data /root/r25-bin/cvm-rx27b3 -cp /root/rx27/p \
  R24RegexSplitRows count cached 300000 600000
perf report -i ps.data --sort symbol | head -25
perf annotate -i ps.data --stdio -s 'java/util/regex/Pattern.split(Ljava/lang/CharSequence;IZ)[Ljava/lang/String; [c2]' \
  | sort -rn | head -60
```

Match the hot blocks against items 1-6 and route each one to its owner. This page closes once the
`Pattern.split` self share is attributed and each item is fixed or paged in its own lane.
