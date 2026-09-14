# G64-1 — the Scanner that read nothing, and a duck test that could not quack

**Status:** MEASURED throughout, fix included (see the banner). **Provenance:** every row
MEASURED on both VMs; oracle HotSpot 25.0.3+9-LTS, CratonVM
`C:/craton/target-rel7` under `--jdk-only`. Probes:
`scratchpad/g63/{Sweep3,ScanProbe,CbufProbe}.java`, ASCII labels only.

> **MEASURED 2026-08-17 at `561e0b5b5`.** The fix is in a binary:
> `new Scanner(new StringReader("hello world"))` now answers `hasNext() = true`
> and `next() = "hello"`, matching HotSpot on all four probe rows.
>
> **A 29-row FUNCTIONAL sweep run at the same time is an EMPTY diff** —
> `BufferedReader.readLine`/`lines`, `StringTokenizer`, eight `Stream`
> collectors, `Comparator.comparing`, `Map.computeIfAbsent`/`merge`,
> `EnumMap`/`EnumSet`, `BitSet`, `UUID`, seeded `Random`, `ArrayDeque` as a
> stack, `ListIterator.set`, `Objects.requireNonNull` messages, overlapping
> `System.arraycopy`, `Files` temp round-trip and `readAllLines`,
> try-with-resources, `printf` width, `Iterator.remove`, `subList`, `TreeMap`
> navigation and `Collections.unmodifiable`. **A clean sweep is a result too**,
> and it is recorded here so the next person does not re-probe those 29.

---

## 0. The defect, and it is not about surrogates

```text
new Scanner(new StringReader("hello world")).next()
  HotSpot    "hello"
  CratonVM   java.util.NoSuchElementException      hasNext() == false

new Scanner("hello world").next()
  HotSpot    "hello"      CratonVM   "hello"
```

Plain ASCII. **Every `Scanner` over a `Reader` was empty** —
`new Scanner(System.in)` where the argument is wrapped,
`new Scanner(new FileReader(f))`, `new Scanner(new InputStreamReader(s))`.
This is one of the first things any Java program does.

## 1. How it was found, which is the transferable part

It was not looked for. It fell out of the third **surrogate** sweep (`G63-1`),
where the probe's `Scanner` row aborted the run at row 7 of 20 while the other
19 rows were about text encoding. **The sweep found a defect in a completely
different family than the one it was written for**, because a broad probe over
unrelated APIs is also a broad probe over whether those APIs work at all.

That is worth stating because it is cheap to repeat: 20 rows of "does this
surface answer the same thing" is also 20 rows of "does this surface answer".

## 2. Why the wrong body looked right

`<init>(Ljava/lang/Readable;)V` was registered to
`native_scanner_init_inputstream` — the `InputStream` body. That body
duck-types a `ByteArrayInputStream` from the real-JDK layout:

```text
ByteArrayInputStream   { buf: byte[], pos: int, mark: int, count: int }
StringReader           { str: String, length: int, next: int, mark: int }
```

Field 0 an object, field 1 an int, field 3 an int — **a `StringReader`
matches the shape test.** So it took the byte-array branch and read "array
elements" out of a `String`, producing an empty scanner instead of an error.

**A duck test that both shapes pass is not a test.** This is the same family
as `G59-1`'s slot aliasing (a synthetic slot map written into a real class,
where two of four wrong writes were well-typed and silent) and it fails the
same way: the wrong answer is indistinguishable from a right one at the point
where it is produced.

## 3. What was ruled out first

`Reader.read(CharBuffer)` was the obvious suspect, since `Scanner(Readable)`
uses it and `G63-1` had just measured `CharBuffer.wrap(char[]).toString()`
diverging. **It is not the cause and `CharBuffer` is exonerated**: a 14-row
probe of `allocate`/`hasArray`/`capacity`/`position`/`limit`/`remaining`/
`arrayOffset`/`array`/`flip`/`wrap`/`charAt`, plus
`new StringReader("hello").read(cb)` itself, is **byte-identical on both VMs**.

Ruling it out took one probe and stopped a fix being written against
`CharBuffer`, which would have been the `G61-1` §3a mistake — editing a body
that is not the one at fault.

## 4. The fix

`java.lang.Readable` declares only `read(CharBuffer)`, but every `Readable`
that reaches this in practice is a `java.io.Reader`, which also declares
`read(char[], int, int)`. That is what the new body drains, in 4 KiB chunks —
the same text through `read()I` would be one re-entrant bytecode call per
character.

Three details that are load-bearing rather than stylistic:

* **The hierarchy walk is by NAME**, not by resolving `java/io/Reader` to a
  `ClassId`. The class may not be loaded when this runs, and a resolution miss
  would read as "not a Reader" — restoring the empty scanner, silently.
* **All three references are pinned and re-read across every
  `invoke_virtual`.** Those calls run bytecode, which allocates; a raw
  `ObjectRef` held across them is a from-space address.
* **A non-`Reader` `Readable` falls back to `toString()`.** That is exactly
  right for `CharBuffer`, the only such implementation in the JDK, and is
  recorded as a limitation for anything else rather than left to be discovered.

## 5. What is guarded

Five rows in `RJdkIntrinsics3`'s `regex` family (42 → 47), all verified PASS on
HotSpot before the fix was built.

**Every pre-existing `Scanner` row in that vector uses the `String`
constructor**, which is exactly why an empty `Scanner(Reader)` survived a
vector that already covers `Scanner` in three places. Two of the new rows push
a token across a 5000-character read-chunk boundary, so a chunked drain that
stops after the first block fails rather than passes.

## 6. NOMINATIONS

**N1 — `java.text.Normalizer.normalize` loses an unpaired surrogate.**
MEASURED in the same sweep: HotSpot `61,d800,62`, CratonVM `61,fffd,62`. The
bridge runs the whole input through the `unicode_normalization` crate, whose
input type cannot represent the unit at all, so there is no local repair.

The fix shape, so the next person does not have to derive it: **split the
units at each unpaired surrogate, normalize each representable run, and
rejoin.** That is correct rather than approximate — an unpaired surrogate is
unassigned, has combining class 0 and composes with nothing, so it is a
normalization boundary. Deliberately not taken here: it needs a units-aware
`CharSequence` reader (the current one handles `StringBuilder` and friends,
not just `String`), and Scanner was the general defect while this is an
exotic input to an exotic API.

**N2 — `Scanner.hasNext()Z` is registered TWICE**, at `native-io/src/lib.rs`
7573 and 7681, and the census shows the first with `owns_slot=false`. Registration
is last-write-wins with no unregister API, so one of the two is dead and
nothing in the source says which. Same shape as `G61-1` N3 (`URI.<init>`) and
`G59-1` N2 (two `HUC_*` maps): a twin that is invisible until someone edits
the loser. The dump names the winner in one line.

**N3 — the sweep is still not finished, and its yield went UP.** `G63-1` N4
listed the untouched surfaces; sweep three covered a third of them and
returned two defects, one of them this. Still untouched: `Base64` streaming,
`Collator` strength/decomposition, `String.chars()`/`codePoints()` round
trips, `MessageDigest` over text, and every `java.time` formatter.
