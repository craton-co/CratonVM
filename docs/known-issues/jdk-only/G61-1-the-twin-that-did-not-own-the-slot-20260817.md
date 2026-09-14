# G61-1 — the twin that did not own the slot

**Status:** MEASURED. **Provenance:** every row below is MEASURED on both VMs.
Binaries named per row (`C:/craton/target-rel7`, `--jdk-only`); oracle is
HotSpot 25.0.3+9-LTS. Probes: `scratchpad/g60/{UriProbe,UriProbe2,StrProbe}.java`,
ASCII labels only. Done in-session by the orchestrator.

This closes `RJdkBridge1`, **the last red vector under `--jdk-only`**. It also
records two wrong turns of my own, because both are cheap to repeat and one of
them nearly went into this directory as a fact.

---

## 0. The headline

| | before | after |
|---|---|---|
| `RJdkBridge1` | red since the branch began; 128 of the oracle's 164 checks at `3fcc8d90f` | **164 checks, empty diff against the oracle** |
| `surrog` family | 13 of 17 lines | **green** |
| `new URI(<lone high surrogate in path>).toString()` | `charAt(10)=fffd`, and a COPY | `d800`, and the argument object itself (`toString() == src`) |

## 1. Mapping the vector before writing a line

`RJdkBridge1` aborts at its first failing assertion, so its check count is
"how far it got" and nothing more. The vector takes `--only=<family>`, and
eleven runs cost about a minute:

```text
props treenav collect deque vector uri bytebuf sbidx atomarr bigint   all green
surrog                                                                13 of 17
```

**That is the whole finding of this section.** The previous lane's two
nominations — `BigInteger` with a lone surrogate, and the `atomarr` family —
needed no work at all; both families already matched the oracle. Without the
family map I would have written a `BigInteger` fix for a green family, which
is the mistake `HANDOFF`'s §4 exists to prevent and which the same handoff's
own §6.1 ("nothing clever is needed") would have led me into.

## 2. The defect

The URI constructor decodes its argument into a Rust `String`, which cannot
hold an unpaired surrogate, and then rebuilds the stored text from it — once
for the `string` field and once for slot 6. MEASURED with the source built
from a `char[]`, so no constant-pool interning is involved:

```text
new URI("http://h/a<U+D800>b").toString()
  HotSpot    charAt(10)=d800    toString() == the argument   (true)
  CratonVM   charAt(10)=fffd    toString() == the argument   (false)
```

**The length was right on both.** Exactly one code unit differed, so nothing
that measures size, splits on ASCII delimiters, or compares lengths ever
noticed. That is why it outlived every other `uri` row — the whole `uri`
family is green.

The fix is to store the object the caller handed us. `java.lang.String` is
immutable and HotSpot's `URI` keeps one reference to its input, so this is
both exact and two allocations cheaper. The decoded copy stays for the
PARSING, which splits on ASCII delimiters and is unaffected.

Pinned before the parse: the constructor allocates through `url_parse` and
`uri_store_named`, and an `ObjectRef` re-read from `args` afterwards would be
a from-space address. Slot 6 is written **only** on our synthetic layout — on
a real `java.net.URI` slot 6 is `path`, and writing the whole URI there would
corrupt it.

## 3. Two wrong turns, and what each one cost

### 3a. I fixed a twin that does not own the slot

`java/net/URI.<init>(Ljava/lang/String;)V` is registered in **two** places. I
found one by grep — `phases_early.rs:21746` — fixed it, wrote a commit message
calling it "the actual cause", and built. The assertion failed identically.

`--dump-native-registry` answers this in one command:

```text
java/net/URI rows: 29
  <init>(Ljava/lang/String;)V   owns_slot=true  invocations=1
                                registered_by=native-builtins/src/lib.rs:19577
  toString()Ljava/lang/String;  owns_slot=true  invocations=2
                                registered_by=native-builtins/src/net_phase_e.rs:3859
```

The owner is `lib.rs:19577` → `net_uri_inet::native_uri_init`. The body I had
edited is a twin. `HANDOFF`'s §4 says to run this dump to answer "which body
runs", and that reading cannot settle it — I ran it *after* writing the fix
instead of before, and the build was the thing that caught me.

The `phases_early.rs` edit is left in place: it is correct for whatever path
reaches it, and reverting a correct change to tidy a narrative is not an
improvement. But it fixed nothing here, and the commit that claimed otherwise
is corrected by the one that followed.

### 3b. I nearly recorded "the real bytecode runs" as a fact

My first read of the registry dump reported **0 rows** for `java/net/URI`, and
the conclusion that follows is a big one: the natives are refused and real JDK
bytecode runs. I was one step from writing that down.

It was my own JSON parse. The dump's key is `natives`; I had asked for
`rows`/`methods` and taken the empty default. **A zero from a tool you have
not verified the shape of is not a measurement.** One command — print the
top-level keys — separates the two, and it is the same discipline `G33-1`
established for `invocations` (a zero there proves nothing either), arriving
from a completely different direction.

## 4. Two defects found on the way and deliberately NOT fixed

Both are MEASURED, both are real, and both are larger than the row that
uncovered them.

**`String.intern()` loses an unpaired surrogate.** Probed against the oracle
alongside seven siblings, all of which are exact:

```text
                 HotSpot   CratonVM
  substring       d800      d800
  concat          d800      d800
  StringBuilder   d800      d800
  toCharArray     d800      d800
  indexOf         5         5
  equals          true      true
  intern          d800      fffd     <-- diverges
```

Not on `RJdkBridge1`'s path — the URI defect reproduces from a `char[]`-built
string with no interning — so it is a separate defect that this probe happened
to catch. `String.intern` is a natural place to route text through a Rust map
keyed by `String`, which is exactly the shape `G55-1` fixed for `Properties`.

**The parsed URI components still carry `U+FFFD`.** MEASURED at `89e2c56f1`,
after the fix above:

```text
                        HotSpot          CratonVM
  toString              d800             d800     (fixed here)
  getPath               d800             fffd
  getRawPath            d800             fffd
  getSchemeSpecificPart d800             fffd
```

Only the verbatim text is restored. Restoring the components means slicing the
input **by code unit** rather than by Rust `str` byte offsets — the `JavaText`
shape `G55-1` built for `Properties`. No vector covers these three rows today,
which is precisely why they are recorded here rather than left to be
rediscovered.

## 5. NOMINATIONS

**N1 — `String.intern()`, §4.** A one-method defect with a measured oracle row
and no vector covering it. The natural first step is a vector row, not a fix.

**N2 — the URI components, §4. EXAMINED IN-SESSION AND REFUSED; the reason is
the value of this nomination.** Three measured rows, one shape:
`net_uri_inet::url_parse` derives every component from `&str` slices.

Converting it to units is not a local change. `url_parse` serves the `file:`
fast path, the `jar:` path, opaque URIs and the hierarchical splitter, with
component strings created at a dozen sites — **all of them currently green**,
including the entire 50-row `uri` family and every `file:`/`jar:` consumer in
the tree (`Class.getProtectionDomain`, the Spring Boot launcher's
`getSchemeSpecificPart` → `new File`). The rewrite's blast radius is that
whole surface; the evidence behind it is three rows on lone-surrogate input
that no vector asserts and no consumer has ever been shown to reach.

Two bounded alternatives were designed and both rejected:

* **A units-space splitter used only when `has_unpaired_surrogate` is true.**
  Cannot regress anything, since it runs only on input already answered
  wrongly. Rejected because it is a *second* implementation of URI splitting —
  precisely the "fourth copy of one search rule" this directory has already
  paid for once (`F18-1`) — kept alive by a branch almost nothing takes, which
  is how a copy drifts unnoticed.
* **A byte-offset → unit-offset translation reusing `url_parse`'s own
  decisions.** No duplicated parsing, and it is the right shape. Rejected
  because `url_parse` does not expose ranges — it creates the strings directly
  — so obtaining them means either changing its return type across every call
  site or locating substrings by search, which is ambiguous.

**What this leaves true:** `toString()` is now exact, so the units ARE
recoverable by any caller that needs them. The derived components are not.
Taking this properly means giving `url_parse` a range-returning shape first,
as its own change, with the `uri` family as the regression net — and then the
component fix is small. That sequencing is the nomination.

**N3 — `java/net/URI.<init>(Ljava/lang/String;)V` is registered twice, and one
of the two is dead under `--jdk-only`.** Last-write-wins with no unregister
API means the loser is invisible until someone edits it — which is what
happened here. This is `G59-1` N2's shape in a different file: two registrars
for one triple, and nothing in the tree says which is authoritative. The dump
says it in one line; the source does not say it anywhere.
