# G59-1 — four writes into a real class, two of them silent

**Status:** MEASURED (defect) / PENDING-BUILD (fix). **Provenance:** every row
in §1 is MEASURED at `e7e840264` (`C:/craton/target-rel5`, `--jdk-only`) with
`CRATONVM_DBG_COERCION=1` and `CRATONVM_DBG_LAYOUT=1` on `RSslLiveSession`,
cross-checked against `javap` on HotSpot 25.0.3+9-LTS. The fix compiles and its
witness is green; its effect on the vector is **not yet measured**.

This is G58-1 N1 taken, and it is the first defect this session that the
**instrument found on its own** — no vector row pointed at it.

---

## 0. How it was found, because the method is the transferable part

G56-1 removed 1,111 of 1,122 coercion events and left **eleven**. Eleven fits
on a screen, so for the first time the residue could simply be *read*. Two of
the eleven were a species the census had never had a row for:
`pointer-into-primitive` — an `Object` written into a slot the class declares
`Z`.

The chain from there was three commands:

```bash
CRATONVM_DBG_LAYOUT=1   ...    # [layout] <name> cid=<N> body= refs= fields=
CRATONVM_DBG_COERCION=1 ...    # a Rust backtrace per event
javap -p <the name>            # the JDK's own field list
```

`CRATONVM_DBG_LAYOUT` resolved `cid=735` to
`sun/net/www/protocol/https/HttpsURLConnectionImpl`, `refs=7 fields=20` — which
matches `javap`'s flattened instance-field list **exactly**, so the slot
numbering is not an inference. `CRATONVM_DBG_COERCION` named the writer:
`net_phase_e::register_re4_url_http::closure_env$10`, i.e.
`URL.openConnection()`.

Worth stating plainly: the instrument was only usable because its noise floor
had been removed first. At 1,122 events these two were 0.2% of the output.

## 1. The defect

`URL.openConnection()` picks a **real JDK class** as the carrier —
`HttpsURLConnectionImpl` for `https:`, the abstract `java/net/HttpURLConnection`
otherwise — allocates it, and then writes `net_phase_e`'s own `HUC_*` slot map
into it. That map is synthetic and does not match the JDK's layout:

| slot | this file meant | the real field it hit | outcome |
|---|---|---|---|
| 0 | `HUC_URL` | `URLConnection.url` (L) | **right by luck** |
| 1 | `HUC_METHOD` | `URLConnection.doInput` (Z) | `String("GET")` → guard **DESTROYED** it → `doInput` reads false |
| 7 | `HUC_DO_INPUT` | `URLConnection.connectTimeout` (I) | **SILENT** — `connectTimeout = 1` |
| 9 | `HUC_CONNECTED` | `URLConnection.requests` (L) | `Int(0)` → guard **DESTROYED** it → `requests` null |

**The guard caught two and said nothing about the other two**, and that is the
part worth carrying forward. `coerce_field_value_by_descriptor` fires on a
descriptor MISMATCH. An `Int(1)` into an `I` field is not a mismatch — it is a
perfectly well-typed one-millisecond connect timeout. So the warning volume was
never a measure of the damage: half of this defect was invisible to the exact
instrument that found the other half.

`sun.net.www.MessageHeader requests` is the field holding every request header
the JDK's own path would send, and it was being nulled at construction.

**Two files, two different maps, one class.** `http_url_connection.rs` also
defines `HUC_*` constants (`0 = CONN_ID`, `1 = URL_STR`, …) and they are a
*different* map from this file's (`0 = URL`, `1 = METHOD`, …). Both files
register natives on the same real classes, and registration is last-write-wins.
That is the shape behind this, not a single bad line.

## 2. Why it survived

* **No vector fails on it.** `RSslLiveSession` passes 96 of its 104 checks with
  all four writes wrong, because nothing it asks reaches `doInput`,
  `connectTimeout` or `requests`.
* **No unit test can see it.** `MockNativeContext` has no real class layout, so
  an indexed write and a by-name write are *the same operation* to it. A full
  green unit suite is not evidence about this class of bug at all.
* `http_url_connection.rs` states the correct rule for itself — "never write
  synthetic slots (they alias real fields on a real-JDK object)" — and keeps its
  state in identity-keyed side tables. The rule was written down and not applied
  in the sibling file.

## 3. The fix

Ask the class where its fields are:

```rust
ctx.set_field_by_name(conn, "url",       Value::Object(Some(this)));
ctx.set_field_by_name(conn, "method",    Value::Object(Some(m)));
ctx.set_field_by_name(conn, "doInput",   Value::Int(1));
ctx.set_field_by_name(conn, "connected", Value::Int(0));
```

All four now land on the field that means what the value means, and
`connectTimeout` and `requests` are left alone — which is the whole point:
this writes **fewer** fields than before, not more.

A side table was considered and is not needed here. Unlike
`http_url_connection.rs`'s `RealReq` state, these four values have real JDK
fields that mean exactly this; the bug was addressing them wrongly, not
addressing them at all.

**Behaviour under the old code, for the readers that exist.** `huc_perform`
reads `HUC_METHOD` and `HUC_CONNECTED`. Before: it read the two DESTROYED
values (`Int(0)`, `Int(0)`) and fell back to `"GET"` and "not connected".
After: it reads `doInput = Int(1)` (not a string → same `"GET"` fallback via
`read_field_string_or`) and `requests = null` (`as_int().unwrap_or(0)` → same
"not connected"). **Identical on both paths**, which is why this is safe
without touching the readers.

The `jrt:` carrier a few lines above is deliberately left on indexed writes: it
IS a CratonVM synthetic with 16 slots of our own, and indexed writes are correct
there. The witness in §4 is scoped to the real-carrier branch for that reason.

## 4. What is guarded

`the_real_jdk_carriers_are_initialised_by_field_name` — a source witness, green.
It asserts no `ctx.set_field(conn, …)` survives between the carrier allocation
and its return, and that at least four `set_field_by_name` calls do.

It is a source witness **because a behavioural one is impossible here**, not
because it was easier: per §2, the mock context cannot distinguish the two
operations. The assertion message carries the two measured slot identities so
the next person to reach for an index is told which real field they would hit.

## 5. What is NOT claimed

No measurement of the fix. `connectTimeout = 1` should mean every connection
this path hands out has a 1 ms connect timeout, which ought to be catastrophic
and evidently is not — most likely because the `perform` path does not consult
that field. **That gap is unexplained**, and it is the reason this record does
not claim a behavioural improvement, only a correctness one. If the next build
shows no change on any vector, that is the expected result, and the value here
is the removal of three wrong writes to a real JDK object, not a green row.

## 6. NOMINATIONS

**N1 — the same audit for the other two carrier classes and for
`huc_perform`.** `huc_perform` writes `HUC_CODE`, `HUC_RESP_HEADERS` and
`HUC_BODY` by index. No coercion event names those, which is evidence it does
not run on the real carriers — **and `invocations`-style absence proves
nothing** (G33-1). Settle it with `owns_slot` plus a probe, then apply §3's fix
or record why it does not apply.

**N2 — the two `HUC_*` maps should not both exist.** `net_phase_e.rs` and
`http_url_connection.rs` define different synthetic slot maps for the same real
classes and both register natives on them, last-write-wins. Whichever survives,
the other file's constants are a loaded gun. This is a rename-and-delete task,
not a behaviour change, and it wants a lane that can build.

**N3 — the coercion guard cannot see same-kind corruption, and should say so.**
Its message presents itself as *the* instrument for wrong field writes. It is
the instrument for wrong *descriptors*. Slot 7 → `connectTimeout` proves the
difference is not theoretical. One sentence in the warning, and a line in
`G30-1`, would stop the next reader from treating a quiet log as a clean one.
