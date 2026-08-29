# L8 tail — `java/net/URI`: seven defects, and the deferred one was 26 rows rather than 1

**Status: CLOSED.** 2026-08-29, branch `claude/l8-tail-20260829`, worktree
`/data/cvm-l2s-20260828` on the Linux build host. Oracle: HotSpot
`jdk-25.0.4+7`, the same image CratonVM ran against.

First batch of the long tail. `apps/probes/UriRecompositionSweep.java`, 1258
rows, **0 differing lines against HotSpot in both `--jdk-only` and compatible
mode**, from 222.

---

## 0. Why this probe existed at all

`uri-resolve-folded-a-reference-into-the-host-and-the-locale-display-name-gap-20260826.md`
§4 recorded a defect and then declined to fix it, with a reason:

> The merge is right; the recomposition keeps `//` for an EMPTY authority where
> HotSpot drops it. Separate from §2 and left alone — **recomposition changes
> reach every `URI` consumer, and this one row does not justify that blast
> radius without its own probe.**

That is a correct call on the evidence it had, and the evidence was one row —
because the probe that found it (`UriLocaleSweep`, 403 rows across eight
families) asks the `URI` value surface, not the recomposition surface. A probe
aimed at recomposition asks the same question 26 times and gets a different
answer.

**The general shape: a deferral whose stated reason is "not enough evidence to
justify the risk" is a request for a measurement, and it is usually cheaper to
take than the deferral is to carry.** This one took one probe and one build.

---

## 1. The seven defects

| # | what | rows |
| --- | --- | ---: |
| D1 | `new URI("http://")`, `"file://"`, `"//"` ACCEPTED where the JDK throws `Expected authority` | 66 |
| D2 | an empty authority recomposed as `//` — §4's row | 26 |
| D3 | a KEPT `..` given a trailing slash it does not own | 11 |
| D4 | the decoded accessors percent-decode INSIDE `[...]`, destroying an RFC 6874 zone id | 2 |
| D5 | `URI.create` could not reach the closing-bracket rule the constructor enforces | 1 |
| D6 | `resolve(null)` / `relativize(null)` answer the receiver instead of throwing | 2 |
| D7 | `parseServerAuthority()` had no registration and never threw | 1 |

### D1 — 66 rows, and the largest of them

```text
new URI("http://")   HotSpot  URISyntaxException: Expected authority at index 7
                     was      a usable URI, and every accessor answered for it
```

`Parser.parseHierarchical` branches three ways after the `//`, and only the
third is a failure:

```text
int q = scan(p, n, "", "/?#");
if (q > p)      parseAuthority(p, q);   // a real authority
else if (q < n) { /* DEVIATION: empty authority before a non-empty
                     path, query or fragment is ALLOWED */ }
else            failExpecting("authority", p);
```

So an empty authority is legal exactly when something follows it: `http:///a`,
`http://?q` and `http://#f` are accepted, and `http://` is not. This VM accepted
all four, and the probe then asked twenty-two accessors of each of the three
objects that should not exist — which is why one parse defect is 66 rows.

### D2 — the deferred row, and why it was 26

`uri_split` returned `Some("")` for the authority of `file:///a`, and
`uri_recompose` emits `//` for any `Some`. The ACCESSORS were already right —
both VMs answer `getAuthority() == null` — and `toString()` of a parsed URI is
its input text either way, so the defect was invisible except through an
operation that REBUILDS the string. `resolve` is that operation, and every
`resolve` off an empty-authority base carried it:

```text
URI.create("file:///C:/tmp/f.txt").resolve("x")     file:/C:/tmp/x    was file:///C:/tmp/x
URI.create("file:///C:/tmp/f.txt").resolve("../..") file:/            was file:///
URI.create("http:///a/b").resolve("/x")             http:/x           was http:///x
```

### D3 — a trailing slash the JDK does not add

`uri_remove_dot_segments` gave any path whose final segment is `.` or `..` a
trailing `/`. The JDK only does so when that segment is CONSUMED; the `..` this
function deliberately KEEPS (its own DEVIATION comment explains why) is an
ordinary segment, and `join` puts no separator after it:

```text
URI.create("http://host/a/b").resolve("../..")   http://host/..   was http://host/../
URI.create("..").normalize()                     ..               was ../
URI.create(".").normalize()                      <empty>          was /
```

The third row is the other half of the rule: a relative path whose segments all
vanish is the EMPTY string, not `/`. The fix resolves the trailing slash against
the finished segment list rather than against the input alone.

### D4 — `%` is literal inside brackets

`java.net.URI.decode(String)` is `decode(s, true)`, and the parameter is named
`ignorePercentInBrackets`. It exists for RFC 6874: the `%` of a zone identifier
`%25eth0` is part of the literal address, and decoding it destroys the only
thing separating a zone id from a percent-escape.

```text
URI.create("http://[fe80::1%25eth0]/a").getAuthority()
  HotSpot   [fe80::1%25eth0]
  was       [fe80::1%eth0]
```

### D5 — one rule, two doors, and only one of them enforced it

`URI.create` is documented as `new URI(str)` with the checked exception
translated, so it owes the same refusals. The `Expected closing bracket for IPv6
address` check was written INLINE in `native_uri_init`, so `create` could not
reach it:

```text
URI.create("http://[::1/a")   HotSpot  IllegalArgumentException
                              was      a URI
new URI("http://[::1/a")      both     URISyntaxException     <- the door that worked
```

Fixed by EXTRACTING the rule into `uri_closing_bracket_fail_index` and calling
it from both, rather than adding a third copy. That is the same
two-copies-of-one-decision shape the campaign has now found in `Arrays.copyOf`,
in `MethodHandle.invoke`, in `Class.forName`'s array arm and here.

### D6, D7 — two refusals that answered instead

`resolve(null)` and `relativize(null)` returned the receiver. A caller cannot
tell that from a no-op resolve. And `parseServerAuthority()` — whose entire
purpose is to turn a registry-based authority into an exception — had no
registration at all, so real bytecode read a synthetic receiver's unpopulated
fields and answered `this` for everything:

```text
URI.create("http://host:x/a").parseServerAuthority()
  HotSpot   URISyntaxException: Illegal character in port number at index 12
  was       http://host:x/a
```

Its host character set is deliberately narrow rather than a full hostname
grammar — refusing what is outside `[A-Za-z0-9.-]` separates registry from
server for every measured shape, and refusing a URI the JDK accepts is the worse
half of this bug. `http://host:99999/a` is accepted, because HotSpot performs no
range check; a bracketed IPv6 host is skipped, because the constructor has
already validated it.

---

## 2. What PASSED, and the blast radius that was worried about

§4's concern was that "recomposition changes reach every `URI` consumer". They
do, so the change was made against every `URI` probe in the tree at once, all
restored to `apps/probes/` and run on each build:

```text
OpaqueUriProbe             648 rows   0-diff   <- the N02/N04/N10/S27 dot-segment
                                                  rows this lane's D3 could have broken
UriRawAccessorSweepProbe   245        0-diff
InetFamilySweep            468        0-diff
IoSystemSweep              154        0-diff
TailFamilySweep            117        0-diff
LangMiscSweep              117        0-diff
Phase3Sweep                 35        0-diff
UriLocaleSweep             403        18 rows — the recorded Locale data gap, §3 of
                                      the same page, unchanged and not this batch's
```

**893 rows of existing `URI` coverage, unmoved.** That is the answer to the blast
radius: it was real, and it was checkable.

Also asked and already correct, so the next reader knows where the work is not:
every opaque form (`mailto:`, `urn:`, `news:`, `tel:`), IPv6 literals with and
without ports, `userinfo` with an escaped `@`, the RFC 3986 §5.4 reference table
across nine bases, `relativize` over six pairs, `normalize` idempotence, `URL`'s
twelve accessors over six specs and both `URI`/`URL` crossings, and the
percent-encoding round trip (`%20`, `%2F`, `%C3%A9`) through every accessor.

---

## 3. What this does NOT establish

* **`Locale`'s display-name gap is untouched** and is still §3 of the page this
  batch came from: it needs a data source, not a parser fix.
* **No performance measurement was taken.** `uri_split` is on the parse path of
  every `URI` construction, and the change adds one integer comparison to it.
  That is an argument, not a number — but unlike the string-builder migration
  this is not a hot loop, and the shape of the work is unchanged.
* **`parseServerAuthority` is a NEW registration**, which moves the registry
  count. The gates were run for exactly that reason.
