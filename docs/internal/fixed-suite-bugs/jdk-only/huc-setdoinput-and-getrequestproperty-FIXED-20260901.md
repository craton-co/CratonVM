# The HUC drift row — the two conformance bugs stay fixed, and the third body was finally built and run

Retires `known-issues/jdk-only/bug-huc-setdoinput-dropped-and-getrequestproperty-joined-20260826.md`.

## Status

**RETIRED 2026-09-01.** The page's two defects (§2 `setDoInput`, §3
`getRequestProperty`) are verified still fixed, and its one open residual —
§4, "deciding it needs the `--features synthetic-jdk` arm actually built and
run" — has now been decided with that arm built and run.

## 1. The two fixed defects, re-verified

`probes/HucAccessors.java`, 30 checks, on a shipping binary built from dev
`222384268`:

```text
cratonvm --java-home "$JDK" --jdk-only -cp out HucAccessors
  -> PASS HucAccessors (30 checks)
```

Both rows the page was named for — `setDoInput(false); getDoInput()` and
`setRequestProperty/addRequestProperty` then `getRequestProperty` — are among
them.

## 2. What §0 says today

`--dump-native-registry`, real-JDK boot, `--jdk-only`,
`java/net/HttpURLConnection`: still 43 rows, and the shipping-vs-shipping
duplicate species the page described is still there — **10** triples registered
by both `net_phase_e.rs` and `http_url_connection.rs`, with
`http_url_connection.rs` winning every one (`owns=True`). The page counted 9;
the difference is registrations added since 2026-08-26, not a correction.

## 3. §4's residual: the count was wrong before the question was even asked

The page says `registrar_drift.rs` pairs `register_phase54_net_extras` with
`http_url_connection.rs::register_one` on **11** `java/net/HttpURLConnection`
triples. It is **18**, and `DRIFT_TRIPLES`' own entry for that registrar lists
exactly those 18. Extracting both function bodies and intersecting them:

```text
register_one triples: 31
phase54 HUC triples : 37
OVERLAP: 18   phase54-only: 19   register_one-only: 13
```

The 18 overlapping: `connect`, `disconnect`, `getContentLength`,
`getContentLengthLong`, `getErrorStream`, `getHeaderField(String)`,
`getInputStream`, `getInstanceFollowRedirects`, `getOutputStream`,
`getResponseMessage`, `setChunkedStreamingMode`, `setConnectTimeout`,
`setDoInput`, `setDoOutput`, `setFixedLengthStreamingMode(I)`,
`setFixedLengthStreamingMode(J)`, `setInstanceFollowRedirects`,
`setReadTimeout`.

This matters for the page's §4 argument, which asks whether `register_one` is a
**superset** of the synthetic-only body. It is not, and that is decidable
without building anything: 19 of phase54's HUC triples have no `register_one`
counterpart at all, including `getConnectTimeout`, `getReadTimeout`, `getURL`,
`getUseCaches`, `setUseCaches`, `getContentType` and the thirteen `HTTP_*`
status constants. On a real-JDK image those come from `URLConnection`'s own
bytecode; on a synthetic image there is no bytecode, so deleting
`register_phase54_net_extras` wholesale would delete the only implementation.

**So the page's conclusion — keep the drift row — is right. Its stated reason
was not the reason.**

## 4. The two bodies do not share a field layout, and one of them says so

`http_url_connection.rs` and `phases_early.rs` describe *different* carriers:

```text
http_url_connection.rs      phases_early.rs / net_phase_e.rs
  0 conn-id / real URL        0 url
  1 urlStr                    1 method
  2 method                    2 responseCode
  3 reqHeaders                3 fd
  4 reqBodyStream             4 reqHeaders
  5 doInput                   5 respHeaders
  6 doOutput                  6 respBody
  7 connected                 7 doInput
  8 disconnected              8 doOutput
  9 followRedirects           9 connected
 10 connectTimeout           10 jarFile
 11 readTimeout
```

`setDoInput` writes slot 5 in one and slot 7 — which is `connected` in the
other — in the other. The tree already knows: `getErrorStream` in
`phases_early.rs` carries an explicit layout guard for exactly this
("`java/net/HttpURLConnection` carries two synthetic layouts in this tree"), and
it is the **only** one of phase54's HUC registrations that does.

## 5. The decision §4 asked for

The page framed §4 as: is `register_one` a superset, so the synthetic-only copy
can be deleted the way clusters 2 and 3 were? It said the answer needed a
`--features synthetic-jdk` build.

Half of it did not. **`register_one` is not a superset, and that is decidable
from the two function bodies**: 19 of `register_phase54_net_extras`'s 37
`HttpURLConnection` triples have no `register_one` counterpart at all. On a
real-JDK image those are served by `URLConnection`'s own bytecode; on a
synthetic image there is no bytecode, so deleting the synthetic-only registrar —
which is exactly what clusters 2 and 3 did — would delete the only
implementation of `getConnectTimeout`, `getReadTimeout`, `getURL`,
`getUseCaches`, `setUseCaches`, `getContentType` and thirteen `HTTP_*` status
constants.

The two bodies are also not written against the same object. Deleting only the
18 *overlapping* registrations would leave `register_one`'s 12-slot bodies
driving a carrier `net_phase_e::URL.openConnection` allocates with 16 slots in a
different order — `setDoInput` would write slot 5 (`doInput` in one map,
nothing in the other) on an object whose `doInput` is slot 7 and whose slot 7 is
`connected`.

**So the page's conclusion is right and its stated reason is not the operative
one.** The row stays because the two bodies are not interchangeable — different
coverage, different layout — not because "the shipping body is the better one"
failed as an assumption.

## 6. The synthetic arm, built and run — and what it actually answers

`--features synthetic-jdk`, built and run for the first time on this page's
question. (Built with `lto=false, codegen-units=16, opt-level=0`: this host
OOM-killed three release builds during this session, and a conformance probe's
verdict does not depend on codegen. `--synthetic-jdk` itself is healthy on that
binary — a hello-world runs and exits 0.)

### 6.1 §0's headline, measured instead of argued

`--dump-native-registry` under `--synthetic-jdk`, `java/net/HttpURLConnection`:
**80 rows**, against 43 in the shipping registry. The page said "the drift gate
sees two bodies here and there are three". Here are the three, on one triple:

```text
connect()V   owns=False   native-builtins/src/net_phase_e.rs:11711
connect()V   owns=False   native-builtins/src/http_url_connection.rs:5162
connect()V   owns=True    native-builtins/src/phases_early.rs:22607
```

Same shape for `disconnect`, `getContentLength`, `getInputStream`,
`getHeaderField(String)`. **`phases_early.rs` — the synthetic-only registrar —
wins every one of the 18 overlapping triples in synthetic mode**, and
`http_url_connection.rs` wins them all in the shipping modes. That is the drift,
in the registry, in both directions. Nothing had shown this before.

It also confirms §2 from the other side: `getDoInput()Z` is absent from the
synthetic registry entirely, and the probe dies on it with `NoSuchMethodError`
where a real-JDK image serves it from `URLConnection`'s bytecode.

### 6.2 The arm cannot adjudicate body-vs-body, and that is the finding

`HucAccessors` under `--synthetic-jdk` does not reach a verdict at all:

```text
  DIFF getURL(): got=null want=http://127.0.0.1:1/path?q=1
  NoSuchMethodError: java/net/HttpURLConnection.getDoInput()Z
```

preceded by a flood of

```text
WARN zgc: zgc real: field index OOB index=0 num_slots=0 op="set"
     ... index=1 .. index=5, then get for 0,1,2,3,5,8
```

`index=0..5` in `set` order is `huc_init` writing `HUC_CONN_ID .. HUC_DO_INPUT`.
**Every field access on the synthetic carrier is out of bounds: the object
declares zero slots.** Not a stale-reference artifact — `num_slots=0` is also
the documented signature of a read into a compacted-away object, so that was
ruled out directly: with `CRATONVM_ZGC_RELOCATE=0` the warnings are unchanged.

`net_phase_e.rs:10626`'s `URL.openConnection` (which `owns=True` in synthetic
mode) allocates the carrier through `try_alloc_concurrent_synthetic(.., 16)`.
That funnel's own comment names this exact species and says the lane
deliberately reports it without refusing it: *"ALLOCATION IS UNCHANGED by the
widening above ... Reporting and refusing are separate changes and this lane
makes only the first — a funnel with ~2,000 call sites is not where you discover
that number by failing."* The observation goes to
`layout_alias::observe_from_rust`, a census rather than a log, which is why the
run's stderr carries the GC guard's complaint and not the fabrication's.

**So: in the one mode where the synthetic-only body wins, the carrier it drives
has no declared fields, and neither body can work on it.** The drift row is not
a live risk there; it is a record of two implementations of a surface that mode
cannot currently run at all. Choosing between them by measurement is not
possible until the carrier declares its shape — which is a separate defect, in a
legacy non-default mode, and is written up on its own page rather than fixed
inside a page about the shipping accessors.

## 7. Verdict on §4

**The drift row stays, and `register_phase54_net_extras` is not deleted.** Same
conclusion the page reached; three measured reasons it did not have:

1. `register_one` is **not** a superset — 19 of phase54's 37 HUC triples have no
   counterpart, and on a synthetic image there is no bytecode behind them.
2. The two bodies are written against **different carriers**. Deleting only the
   18 overlapping registrations would leave 12-slot bodies driving a 16-slot
   carrier; `phases_early.rs`'s own `getErrorStream` already carries a layout
   guard saying exactly this, and it is the only one of its HUC registrations
   that does.
3. In the mode where the synthetic-only body wins, the carrier **declares zero
   fields**, so no measurement can currently prefer one body over the other.

What the page had instead — "§2 and §3 just showed its real-carrier arm carrying
two conformance bugs, so 'the shipping body is the better one' is exactly the
assumption that failed here" — is true but is not the operative reason. Two bugs
in a body that were then fixed say nothing about whether the bodies are
interchangeable; the coverage gap and the layout mismatch do, and neither
needed the synthetic build to see.

## 8. What this leaves behind

* The two conformance defects: **fixed and re-verified**, 30/30.
* The drift row: **kept, with the reason now measured** rather than argued.
* New, recorded separately: the synthetic-JDK `java/net/HttpURLConnection`
  carrier declares zero fields, so its whole accessor surface is inert in that
  mode. It is a `--features synthetic-jdk` defect — a legacy, non-default mode
  — and it belongs to `try_alloc_concurrent_synthetic`'s known
  report-but-do-not-refuse population, not to this page.
