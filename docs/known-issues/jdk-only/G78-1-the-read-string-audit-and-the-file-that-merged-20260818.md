# G78-1 — the read_string caller audit, and the File that merged two paths

**Status:** FIXED (17 of 19 measured rows), 2 rows deliberately left and costed.
**Provenance:** both VMs. Oracle HotSpot 25.0.3+9-LTS; CratonVM
`C:/craton/target-nolto`, `--jdk-only`. **Re-verified on the fat-LTO binary**
(`C:/craton/target-rel13`): both probes give identical row counts (Sweep16 2 of
44, Sweep12 0 of 26), the vector passes with the same 483 checks, and all three
arms sit at baseline by name — 100/100, 95/100, 61/62. The non-LTO and LTO
builds agree, so no release claim rests on the faster build alone. Probe
`regression-suite/probes/Sweep16ReadStringCallers.java` (44 rows). Vector rows
in `regression-suite/src/RJdkBridge1.java`, family `surrog` (82 → 111).
The `URI.relativize` half also closes `G75-1` N3, verified by
`Sweep12UriComponents` going to 0 of 26.

This closes `G70-1` NOMINATION 1.

---

## 0. The problem the nomination stated

`G70-1` N1 asked for an audit of `read_string` call sites, and said plainly
that the judgement it needs — is each value INSPECTED, or handed BACK to Java —
is one no grep supplies. It does not. A grep supplies 2904 hits, which is a
census, not an audit; at a minute a site that is a week of reading, and the
reading would be mostly wasted, because the overwhelming majority of those
sites read a class name, a descriptor, a charset name or a flag, where
`read_string` is exactly the right reader.

So the first job was not to start the audit. It was to find the filter that
turns it into one.

## 1. Two filters, neither of which is a grep

**Filter one — dataflow, statically.** A `read_string` can only lose something
observable if the value comes BACK. Parsing every `r.register(...)` block by
brace matching and asking which contain both a `read_string` and a
`create_string` cuts 2904 hits to **423 blocks**, of which **136** round-trip.

**Filter two — reachability, by measurement.** Under `--jdk-only` most natives
are dead: real JDK bytecode answers instead. `--dump-native-registry` reports
`owns_slot` and `invocations` per registration, and `registered_by` gives a
`file:line` that joins straight back to the static scan. Running all 104
compiled vectors and unioning the invoked, owning registrations gives 2289 live
natives; intersecting with the 136 gives **18**.

**2904 → 18**, and the survivors are ranked by invocation count, so the audit
starts where the traffic is. That is the part the nomination could not
anticipate: the judgement it correctly said no grep supplies still had to be
made, but only eighteen times, and with the answer's importance already
measured.

The counts, top of the list: `File.getPath` 79, `File.<init>(String)` 49,
`DateFormat.format` 48, `URI.getHost` 42, `URI.getScheme` 41.

## 2. The instrument

A lone surrogate. A Rust `str` cannot hold one, so a value that survives the
round trip was never routed through `read_string`, and a value that returns as
U+FFFD was. Every probe row is a pure string operation — no row needs the
filesystem to hold such a name, because `File` is a string wrapper until
something touches the disk.

**Result: 13 of 42 rows diverged, and all 13 were one family — `File`/`Path`.**
`DateFormat.format` with a lone surrogate in a quoted pattern literal was
exact. So were `URI.getScheme`, `URI.getHost`, `URL.getProtocol`,
`InetSocketAddress.toString`, the path matcher, and all three deliberately
"inspected" controls (a class name, a charset, an array descriptor). The audit
did not convert everything in sight, and the record says which sites it left
alone and why.

## 3. What `File` actually did — three losses and a merge

`java.io.File` keeps its path in a JAVA FIELD. `<init>` reads the argument,
normalises it, and writes it straight BACK. So the value is not inspected; it
is handed back, and it was lost three separate times:

1. **at construction** — `read_string` → normalise → `create_string`;
2. **at the field read** — `getPath`/`toString` re-read the field the same way,
   so even a correct field would have been re-lost here;
3. **in `getName`** — via `std::path::Path::file_name` + `to_string_lossy`, a
   third route with its own substitution.

Any one of these alone would have made a fix look ineffective, which is the
same lesson `G75-1` N1 taught on the URI chain: convert the chain end to end,
and let the probe adjudicate after each step.

**And it was not only rendering.** `equals`, `hashCode` and `compareTo` are all
computed FROM that path. So:

```
new File("d/\uD83Dx").equals(new File("d/\uFFFDx"))  ==  true    (HotSpot: false)
                     ... same hashCode,  compareTo == 0
```

Two DIFFERENT files compared equal, hashed the same, and sorted as one. A
substitution in a key is not a typo — it is a MERGE. Anything keying a map or a
set by `File` silently conflated them.

## 4. The second defect, which is not about surrogates at all

The `hashCode` row was nearly missed. The first probe asserted only that two
distinct paths hash DIFFERENTLY — an inequality, which passed on both VMs for
the wrong reason and hid the value. Asking for the value instead:

```
new File("AB").hashCode()   HotSpot 1235376   CratonVM 2081
```

Two faults in one line. The old body hashed `path.bytes()` — UTF-8 bytes, which
agrees with Java's `String.hashCode` only for ASCII — and omitted the
`^ 1234321` that both `UnixFileSystem` and `WinNTFileSystem` mix in. And a
third, in the neighbouring methods: on Windows `File` comparison is
case-INSENSITIVE, so HotSpot answers `new File("ab").equals(new File("AB"))`
true where CratonVM answered false.

**The lesson worth carrying: an inequality assertion hides a wrong value.**
`a != b` passes whenever both sides are wrong in different ways. Three of the
five rows here were found only by re-asking the same question as "what IS it".

## 5. The fix

Same discipline as `G75-1` N1, and stated once so it cannot drift: the `_units`
functions are the IMPLEMENTATIONS, and the `&str` spellings that already
existed are thin wrappers over them. A `&str` can never hold a lone surrogate,
so encoding one to units and back is exact — the wrappers lose nothing they did
not already lack, and there is no second rule to keep in sync.

Converted: `file_normalise_path`, `file_join_parent_child`, `file_read_path`,
plus `file_basename_units` / `file_parent_units` replacing the `std::path`
detour; the three constructors; `getPath`, `toString`, `getName`, `getParent`;
and `equals` / `hashCode` / `compareTo`, which additionally gained the case fold
and the mixing constant.

One behaviour change came free with the conversion and is called out because it
is invisible otherwise: the Windows trailing-separator guard counted BYTES on a
`&str` and now counts UNITS. `WinNTFileSystem.normalize` counts chars, so the
units count is the correct one; the byte count agreed only for ASCII paths.

**18 diverging rows → 2**, plus the `File(URI)` row §5a describes, which the probe added afterwards and which is also fixed.

## 5a. Two faults the probe could not have found, and one it did

Three things were caught AFTER the fix was written, by re-reading it and by
asking the oracle rather than paraphrasing it. They are recorded because each
was invisible to the sweep that motivated the work.

**The root prefix.** `getParent` and `getName` both consult `java.io.File`'s
`prefixLength`, and the first conversion paraphrased the rule instead of
transcribing it. Three cases were wrong and no relative-path probe row could
see any of them: `C:\` has a NULL parent (not `C:\`, because the path is no
longer than its own prefix), `C:x` has parent `C:` (there is no separator in it
at all), and `C:\` has an EMPTY name. The JDK's own three-line bodies are now
transcribed into the record, and the vector carries absolute rows.

**The byte/char confusion, twice.** `file_normalise_path` measured its
trailing-separator guard in BYTES of a `&str`; `File(URI)`'s `fromURIPath`
measured the drive test in CHARS and the trailing-slash test in BYTES of the
same string. Both agreed with Java only for ASCII. Units settle them.

**The fourth constructor.** `File(URI)` was invoked ZERO times across all 104
vectors, so the reachability filter correctly excluded it — and that is the
filter's blind spot, stated plainly: it measures what the corpus reaches, not
what users reach. `G70-1` N2 warns that a sibling next to a fixed method is not
thereby fixed, so a probe row was added to ask rather than assume. It diverged.
By then `URI.getPath()` was already exact, so the only remaining loss was this
constructor's own read of the answer.

## 6. What is deliberately NOT fixed

**`java.nio.file.Path` / `Paths.get`** — the two remaining rows
(`path_get_lone_toString`, `path_get_lone_getFileName`). This is not the same
shape as `File` and should not be done by analogy with it. A `File` path lives
in a Java field; an NIO path is interned in a NATIVE path store
(`p57_alloc_path`, `vfs_decode`) that is `String`-typed by construction, and the
SAME stored value is what gets handed to `std::fs` to touch the disk. Converting
it means either changing that store's type throughout or keeping two
representations in it — a real design decision, not a mechanical conversion,
and the evidence for it is two rows.

**`File.getParentFile`** — returns a `File`, and allocation goes through
`file_alloc`, which takes a `&str`. `getParent()`, the string answer and by far
the commoner call, is exact. Noted at the call site.

## 7. NOMINATIONS

**N1 — convert `p57_alloc_path`'s store to units, or decide not to.** §6 states
the choice. Two probe rows are checked in and will show it the moment it is
made; whoever takes it should also re-check `Path.equals`/`hashCode`, which have
the same merge hazard `File` had and were never probed for it.

**N2 — re-ask the inequality assertions elsewhere.** §4 found two real defects
under one `!=`. This session's probes contain other inequality rows; each is a
place a wrong value can hide. Cheap to re-ask, and the yield here was 2 for 1.

**N3 — the reachability filter has a blind spot, and it is now measured.**
`File(URI)` was excluded because the corpus never invoked it, and it was
defective. The filter answers "what does the corpus reach", which is not "what
is correct". Anything it excludes is UNMEASURED, not clean — the exclusion list
is a work list, not a pass list. §5a is one instance; there are 118 others in
the round-tripping set that no vector reaches.

**N4 — the audit method generalises, and the scripts are checked in.** The
static scan plus the registry join (`scratchpad/g81/audit.py` in spirit; the
method is three greps and a brace matcher) reduces ANY "audit every call site of
X" nomination to its live, round-tripping subset. `create_string` callers,
`read_string_chars` callers and the `to_string_lossy` sites are three obvious
next applications — the third would have caught `getName` here without a probe.
