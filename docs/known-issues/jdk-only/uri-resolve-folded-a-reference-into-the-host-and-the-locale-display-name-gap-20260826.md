# `URI.resolve` folded a relative reference into the HOST, and `Locale` answers codes where the JDK answers names

**Status: both `URI` defects are FIXED — §2 on 2026-08-26 and §4 on 2026-08-29.**
The `Locale` gap (24 of the 26 differences) is the only thing still **OPEN**, and
it is scoped in §3.

## 1. The batch

Eight more families off the bridge-kind retirement surface — `URI` (29 rows),
`TreeSet` (31), `HashMap` (29), `Locale` (24), `Hashtable` (22), `Optional`
(20), `ArrayList` (19), `Date` (19) — in `probes/UriLocaleSweep.java`, 403
lines, diffed against HotSpot 25.0.3+9.

Determinism was arranged before anything was read: `TimeZone` pinned to UTC and
`Locale` to `US`, because `Date`'s component getters and `Locale`'s display
names are functions of both and this host's defaults are not a property either
VM should be judged on. Hash-ordered containers are sorted before printing.

```text
compatible   26 differing lines
--jdk-only   26 differing lines      <- identical, so neither is a mode defect
```

`TreeSet`, `HashMap`, `Hashtable`, `Optional`, `ArrayList` and `Date` were
**clean** — including `HashMap`'s null key, `Hashtable`'s two null refusals,
`TreeSet`'s navigation and `pollFirst`/`pollLast`, `Optional`'s `or`/`stream`/
`flatMap` and three refusal shapes, and `Date.clone` independence.

## 2. The defect — two rows, one of them serious

```text
URI.create("http://host").resolve("x")
  HotSpot    http://host/x
  CratonVM   http://hostx        <- the reference became part of the HOST
```

`uri_merge_paths` is `java.net.URI.resolvePath`, whose `i >= 0` guard prepends
nothing when the base path contains no `/`. That is right for the base path
itself and wrong for the *result*: with an authority present, recomposition
concatenates an unrooted path straight onto the host. `http://host` + `x`
became `http://hostx` — a different host, silently, with no error.

The fix roots the merged path when the base is absolute AND carries an
authority AND the merged path is non-empty.

**The non-empty condition is the whole difficulty, and it is why the fix is at
the call site rather than in the merge helper.** `URI.create("https://h")
.resolve("")` must stay `https://h` and NOT become `https://h/` — that is
`OpaqueUriProbe` row S17, which the merge helper's own comment records as a
previously-fixed defect. An empty child leaves the merged path empty, so the
new rule does not fire, and the unit test pins both directions.

## 3. OPEN — `Locale` answers codes where the JDK answers display names

24 of the 26 differences are one family:

```text
Locale.US.getISO3Language()          HotSpot "eng"                CratonVM ""
Locale.US.getDisplayLanguage(ROOT)   HotSpot "English"            CratonVM "en"
Locale.US.getDisplayCountry(ROOT)    HotSpot "United States"      CratonVM "US"
Locale.US.getDisplayName(ROOT)       HotSpot "English (United States)"
                                     CratonVM "en (US)"
```

Same shape for `en-GB`, `fr-FR`, `de-DE`, `ja-JP`, `zh-Hans-CN`.

`getLanguage`, `getCountry`, `getScript`, `getVariant`, `toLanguageTag`,
`toString` and `equals` are all **correct** — the tag parsing works. What is
missing is the display-name and ISO3 DATA: CratonVM returns the code it already
has where the JDK returns a localized name from its locale bundles, and empty
where the JDK returns a three-letter ISO code.

**Not fixed here, and it is not a one-line fix.** It needs a data source —
either the JDK's own `sun.util.locale.provider` bundles routed through, or a
table — and the right answer is almost certainly the former, since inventing a
table would drift from whatever JDK image is mounted. Recorded because the
failure mode is quiet: a caller formatting `getDisplayName()` into a UI gets a
language tag instead of a name, with no exception.

This is the same shape as the already-recorded
`java-time-text-names-do-not-come-from-dateformatsymbols` finding — a
display-name path that answers a code — and the two should probably be fixed by
the same wiring.

## 4. FIXED 2026-08-29 by L8 — and it was 26 rows, not one

> **Closed.** `apps/probes/UriRecompositionSweep.java` is the probe this section
> asked for: 1258 rows aimed at recomposition rather than at the value surface,
> and **0 differing lines in both modes** after the fix. The deferral below was
> right about the blast radius and wrong about the size — this is 26 rows, every
> `resolve` off an empty-authority base, plus six more `URI` defects the same
> probe found (an accepted `http://`, a trailing slash on a kept `..`, an RFC
> 6874 zone id destroyed by decoding, a closing-bracket rule only one door
> enforced, two null refusals that answered, and an unregistered
> `parseServerAuthority`). The blast radius was then CHECKED rather than
> feared: 893 rows of existing `URI` coverage across six probes, unmoved. See
> `l8-tail-uri-seven-defects-and-a-deferral-that-was-26-rows-20260829.md`.
>
> The general lesson, since this page is where it is visible: **a deferral whose
> stated reason is "not enough evidence to justify the risk" is a request for a
> measurement**, and taking it cost one probe and one build.

### The original section, kept



```text
URI.create("file:///C:/tmp/f.txt").resolve("x")
  HotSpot    file:/C:/tmp/x        CratonVM   file:///C:/tmp/x
```

The merge is right; the recomposition keeps `//` for an EMPTY authority where
HotSpot drops it. Separate from §2 and left alone — recomposition changes reach
every `URI` consumer, and this one row does not justify that blast radius
without its own probe.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out UriLocaleSweep
```
