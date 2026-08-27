# Cluster 1: the drift row is legitimate, and probing its SHIPPING side found two conformance bugs

**Status: two defects FIXED 2026-08-26.** The 11 drift pairs themselves stay —
`register_phase54_net_extras` is **not** a redundant duplicate, §4.

## 0. What this cluster turned out to be

`registrar_drift.rs` pairs `phases_early.rs::register_phase54_net_extras`
(synthetic-only) with `http_url_connection.rs::register_one` (shipping) on 11
`java/net/HttpURLConnection` triples. Clusters 2 and 3 resolved by reading both
bodies and keeping the better one. This one did not, and the reason is worth
recording: **the drift gate sees two bodies here and there are three.**

`--dump-native-registry`, real-JDK boot, `java/net/HttpURLConnection` — 43 rows,
identical in both modes:

```text
connect()V              owns=False   native-builtins/src/net_phase_e.rs:11371
connect()V              owns=True    native-builtins/src/http_url_connection.rs:5099   overwrote=bridge
getContentLength()I     owns=False   native-builtins/src/net_phase_e.rs:11555
getContentLength()I     owns=True    native-builtins/src/http_url_connection.rs:5149   overwrote=bridge
… 9 such pairs …
```

So `net_phase_e.rs` registers nine of these triples and **loses every one** to
`http_url_connection.rs`. That is a shipping-vs-shipping duplicate, invisible to
`registrar_drift.rs` by construction, and `register_phase54_net_extras` is a
*third* body on top.

Whole-registry, this species is not rare: **915 registrations never win** in the
`--jdk-only` registry (823 triples), 1,050 in compatible. That population is
already ratcheted by `duplicate_registration_gate.rs` at 1,201, so it is known
debt — but it means "which body runs" is not answerable from the drift row.

## 1. So the question was re-aimed at the shipping side

`register_one`'s bodies are the ones a shipping binary runs. HotSpot is the
oracle for what they must do, and that needs no synthetic-JDK build.
`probes/HucAccessors.java` — 30 checks over the accessor surface, no network
(`openConnection()` does not connect):

```text
HotSpot 25.0.3+9      PASS 30/30
CratonVM compatible   FAIL 3 of 30
CratonVM --jdk-only   FAIL 3 of 30      <- identical, so not a mode defect
```

**The probe was wrong first.** Its initial expectation was that
`getRequestProperty` after an `addRequestProperty` returns `"2, 3"`; HotSpot
answered `"3"` and failed the ORACLE 2 of 30. The oracle disagreeing with the
probe is the probe being wrong — corrected before any VM was judged.

## 2. Defect one — `setDoInput(false)` is silently dropped

```text
setDoInput(false); getDoInput()  ->  true      (HotSpot: false)
```

`getDoInput()` is declared on `URLConnection`, is not overridden, and is one
`getfield`. CratonVM registers `setDoInput` as a native on all four connection
classes but registers `getDoInput` on **only**
`sun/net/www/protocol/https/HttpsURLConnectionImpl`. So on a plain HTTP carrier
the native setter wrote nothing the bytecode getter reads.

`huc_set_do_input` returned early on a real carrier with:

> *"Real carrier: doInput defaults true and is not consulted by our perform;
> never write a synthetic slot on a real object (it corrupts a real field)."*

The rule quoted is right and is not what the fix does. Writing `HUC_DO_INPUT` —
a synthetic INDEX — would corrupt a real field; `set_field_by_name(this,
"doInput", …)` resolves the REAL slot in the receiver's own hierarchy. **Its own
siblings already prove it**: `huc_set_do_output` mirrors `doOutput` and cites a
measured Spring failure for doing so (a dropped request body → "Read timed
out"), and `huc_set_connect_timeout` / `huc_set_read_timeout` mirror their
fields with a comment saying *"a carrier that reports 0 (infinite) for a timeout
the caller just set is a silent lie."*

Three of four setters in one file had the fix; the fourth kept a comment that
argued past it. The same shape as `getPeakThreadCount` being fixed while three
sibling counters were left frozen.

**What it costs:** `getInputStream()`'s own guard —
`ProtocolException("Cannot read from URLConnection if doInput=false")` — reads
that field and could never fire.

## 3. Defect two — `getRequestProperty` joined where the JDK takes the last

```text
setRequestProperty("X-A","2"); addRequestProperty("X-A","3");
  getRequestProperty("X-A")   -> "2, 3"     (HotSpot: "3")
  getRequestProperties()      -> [2, 3]     (HotSpot: [2, 3]  — already right)
```

The singular accessor is `MessageHeader.findValue`, which yields one value; the
comma-joined form belongs to the PLURAL accessor and to response headers.

The sharp part: **the two halves of `huc_get_request_property` disagreed.** Its
synthetic arm scans and overwrites `found`, so it already kept the last match.
Only the real-carrier arm called `vals.join(", ")` — so the half that runs on a
real JDK image was the wrong half, and the correct behaviour was sitting in the
same function.

## 4. Why the 11 drift pairs are NOT deleted

Clusters 2 and 3 were resolved by deleting the synthetic-only copy. That is
**not** established here and this record does not do it:

* `register_one`'s bodies branch on `is_real_carrier` and handle both carriers,
  which *suggests* it is a superset — but §2 and §3 just showed its real-carrier
  arm carrying two conformance bugs, so "the shipping body is the better one"
  is exactly the assumption that failed here.
* On a synthetic-JDK image there is no `java.net.HttpURLConnection` bytecode, so
  these bodies ARE the implementation — the same argument dev made for keeping
  `register_objects_natives`' `Intrinsic` arm while retiring its real-JDK one.
* Deciding it needs the **`--features synthetic-jdk` arm actually built and
  run**, which is the instrument this session does not have: every measurement
  above comes from a shipping binary, and a shipping binary cannot exercise the
  body that only synthetic-JDK mode registers.

That is the honest state: the drift row stays as recorded debt, and the value
extracted from this cluster was two real bugs on the side that ships.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out HucAccessors
```
