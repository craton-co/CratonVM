# G75-1 — URI components, measured properly and costed

> **N1 DONE 2026-08-18. 12 of the 13 rows are fixed; one remains (§5).**
> The conversion took a shape §2 and its correction both missed, and the shape
> is the point: **the getters re-parse, so ONE splitter in units serves them
> all, and the `&str` helpers become wrappers over it.** No second spelling was
> created, and `url_parse` was never converted — it did not need to be.
>
> Three things had to be right together, and each was invisible until the one
> before it was fixed:
>
> 1. the ACCESSORS, converted to units through one splitter and one decoder;
> 2. the component FIELDS, which the accessors PREFER over their own parse —
>    so a lossy field silently won over an exact parse, and the multi-argument
>    constructor looked fixed while the single-string one did not;
> 3. `URL.toURI()`, which re-encoded the string it already held as an object.
>
> Each step revealed the next only after it landed. That is the argument for
> converting a chain end to end rather than at its most obvious point.

**Status:** N1 **DONE** (12 of 13 rows); N2 **DONE**. One row remains —
`URI.relativize`, §5. **Provenance:** both VMs. Oracle HotSpot 25.0.3+9-LTS;
CratonVM `C:/craton/target-rel13` (fat-LTO release), `--jdk-only`. Probe:
`regression-suite/probes/Sweep12UriComponents.java`, 26 rows, every row printed
as UTF-16 unit values.

---

## 0. What `G61-1` N2 said, and what is actually true

`G61-1` N2 named three surfaces — `getPath`, `getRawPath`,
`getSchemeSpecificPart` — and deferred the fix as "needs `url_parse` to expose
ranges first". Both halves needed correcting.

**The scope is bigger.** 13 of 26 rows diverge — I first wrote 16 here from
eyeballing a diff, and the count was wrong; `diff | grep -c` says 13, before
and after. Two of those 13 are now fixed (N2 below), leaving 11:

```text
uri.getPath / getRawPath          /a FFFD b     (HotSpot /a D800 b)
uri.getQuery / getRawQuery        a FFFD b
uri.getFragment                   a FFFD b
uri.getSchemeSpecificPart / raw   both
uri.relativize(..).toString       a FFFD b
url.toString                      .../a FFFD b
url.toExternalForm                .../a FFFD b
url.toURI().getPath               /a FFFD b
```

...and it is not everything, which is the useful half of the measurement:

```text
uri.toString          EXACT   <- G61-1's own fix, the cached raw object
uri.getScheme/Host/Authority   EXACT   (ASCII by construction here)
uri.normalize / resolve        EXACT
url.getPath / url.getFile      EXACT   <- URL stores its own components fine
ordinary text, and a WELL-FORMED surrogate pair   EXACT
```

`URL.getPath()` being right while `URL.toString()` is wrong is the row that
locates the defect — and it turned out to locate a DIFFERENT one than I first
wrote here. My original sentence read "the component storage is not the
problem, the parse is". Wrong for URL: the components are filled by real JDK
bytecode and are exact, and what was ours was the external-form
RECONSTRUCTION. See N2. The row was the right clue and I drew the wrong
conclusion from it until I followed it.

## 1. Where the unit is actually lost

Not in `url_parse`. **Before it.**

```rust
// native_uri_init and its siblings
Some(o) => ctx.read_string(o).unwrap_or_default(),   // <- here
...
url_parse(ctx, this, &full);                          // already lossy
```

Every constructor reads its arguments with `read_string`, which cannot hold an
unpaired surrogate, and assembles a `full: String` that has already lost it.
`url_parse` then slices a string that no longer contains the unit, and
`create_string` writes the components back. The unit is gone three steps before
anything URI-specific happens.

This is why `G61-1`'s fix worked for `toString` and nothing else: it cached the
ARGUMENT object and handed the same object back, bypassing the parse entirely.
The cache was the right fix for that row and could never have helped the others.

## 2. Why this is not the collections refactor again

`G70-1` converted seventeen `toString` families and the mechanism was the
trait's two units methods. Those exist now, so the *primitives* for this are
already in the tree: `read_string_units` and `create_string_from_units`.

What is not there is the middle. `url_parse` is 195 lines of `&str` slicing
with **ten callers**, and each caller assembles its own `full` string from
several arguments (`format!("{scheme}:{ssp}")` and friends). Converting it
means:

* every constructor reads its arguments as `Vec<u16>`;
* every caller assembles `full` as units — string concatenation becomes slice
  concatenation, which is mechanical but touches all ten;
* `url_parse` slices `&[u16]`. **This part is easy and worth saying so:** every
  URI delimiter is ASCII (`:`, `/`, `?`, `#`, `@`, `[`, `]`), so scanning units
  for them is the same code with a different index type — there is no Unicode
  reasoning anywhere in the parse;
* components come back through `create_string_from_units`.

So the cost is the caller chain, not the parser.

**CORRECTION — the costing above is too low, measured 2026-08-18.** It counts
`url_parse` and its callers. It missed that **the URI getters are themselves
parsers**. `URI.getPath()` does not read the `path` field: it calls
`uri_raw_string(ctx, this)` — a `read_string` — then runs its own
`uri_select_raw_path` over the result and percent-decodes it, preferring the
`path` field only when that field passes a slot-collision sanity check, and
reading THAT with `read_string` too. `getQuery`, `getFragment` and the
scheme-specific-part getters are built the same way, each registered
separately in `net_phase_e.rs`.

So a converted `url_parse` alone would fix nothing observable: every getter
would re-lose the unit on its own raw read. The real scope is `url_parse` +
its ten callers + every URI getter + the percent-decoder they share. That is
materially more than §2 claimed, and it is the reason N1 stays unstarted
rather than "one more pass" — I would rather correct my own estimate than let
someone begin on it.

## 3. Why it was not started here

It is a whole pass, and a half-converted parse is worse than an unconverted
one: a `full` assembled in units and sliced as text, or the reverse, silently
mis-slices every URI rather than only the ones with surrogates. That is a
strictly worse defect than the one being fixed, on a path that decides
security-relevant things (authority, host, scheme).

The alternative I considered and rejected: parse the lossy text, then
substitute the original units back into the components positionally, using the
fact that `from_utf16_lossy` maps each unpaired surrogate to exactly one
U+FFFD. It works, and it is a trick — a parser whose correctness depends on a
coincidence of the replacement function's arity, on a security-relevant path.
Recorded so the next person does not have to re-derive that it is possible, and
does not mistake possible for advisable.

## 5. The one row left: `URI.relativize` — since CLOSED, see N3

`relativize` is not an accessor. It runs `uri_remove_dot_segments` and
`uri_recompose` over `&str` — a genuine ALGORITHM on path segments, not a
range selection — and then builds a new URI from the recomposed text. It is
the only row of the 26 still wrong.

Converting it means those two helpers in units. Both split on ASCII `/`, so
neither needs Unicode reasoning; it is a bounded piece of work and simply a
different one from the chain above. Left because it is a distinct algorithm
and the evidence for it is one row.

## 4. NOMINATIONS

**N1 — DONE, and the costing in §2 was wrong in an instructive direction.**
It said the work was `url_parse` plus ten callers, then corrected itself to
add every getter. The real answer was neither: `url_parse` was never touched.
Because the getters re-parse from the raw text, converting THEM plus the
fields they prefer was sufficient — one splitter, one decoder, and the `&str`
helpers reduced to wrappers. Two cost estimates in a row were wrong because
both assumed the parse was on the critical path and neither checked.

**N3 — DONE (`G78-1` follow-on). `URI.relativize`, §5.** The cost estimate in
§5 held: two helpers, both splitting on ASCII `/`, converted to units with the
`&str` spellings kept as wrappers. Three things §5 did not foresee, all found
by writing it rather than by re-reading it:

* the BASE path needed units too, not just the target's. It is prefix-matched
  against the target, so a lossy base fails to match a target that legitimately
  starts with it — the bug would have been a silently un-relativized URI;
* `make_uri` was a third sink. It takes a `&str`, so the recomposed units were
  lost on the way into the new URI. `make_uri_units` applies the same
  correction `URL.toURI()` already used, deliberately spelled the same way;
* scheme and authority stay TEXT, and that is the point rather than an
  omission: `relativize` only ever compares them, and G78-1's rule is that a
  value which is inspected may stay text. Only what is handed back needs units.

`resolve` and `normalize` still call the `&str` wrappers. Both measured exact
on the 26-row probe, so this is a residue and not a known defect — but it is a
residue: they are exact today because their probe inputs happen not to exercise
the lossy edge, not because they are converted.

**N2 — CLOSED. It WAS the second, narrower defect.** `URL.toString()` and
`toExternalForm()` share one native that reconstructs the external form; it
read `file` and `ref` with `read_string` and assembled a Rust `String`. The
components were never the problem — under `--jdk-only` the real JDK
constructor fills them and only the reconstruction is ours, which is exactly
what `getPath()` being exact was telling us. Fixed by assembling the OUTPUT in
units; the structural reasoning still runs on host text, because scheme, host
and port are ASCII by URI syntax. The cached-full-URL fast path now returns the
STORED OBJECT rather than a re-encoding of it.

**A wasted step worth recording: I fixed the wrong twin first.** I edited
`net_uri_inet::native_url_init` — plausible, adjacent, and not the owner. The
registry dump says `java/net/URL.<init>` belongs to `net_phase_e.rs:9119`, and
under `--jdk-only` that body delegates to real bytecode anyway, so the edit was
dead twice over. `G61-1` records this exact mistake and its remedy is one
command. I ran the dump only after the fix failed to change a single row.
