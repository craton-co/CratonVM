# G75-1 — URI components, measured properly and costed

**Status:** MEASURED. **N2 FIXED** (`URL.toString`/`toExternalForm`); **N1 NOT
started** — §3 says why and what it would take. 13 rows diverged, 2 are fixed,
11 remain and all 11 are N1. **Provenance:** both VMs. Oracle HotSpot 25.0.3+9-LTS;
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

## 4. NOMINATIONS

**N1 — the conversion, whole, as costed in §2.** Ten callers, one 195-line
slicer, ASCII delimiters throughout. Lift the rows from
`probes/Sweep12UriComponents.java`, which is checked in and already has the
oracle column.

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
