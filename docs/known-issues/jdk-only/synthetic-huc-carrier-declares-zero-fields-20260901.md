# `--synthetic-jdk`'s `HttpURLConnection` carrier declares zero fields, so its whole accessor surface is inert

**Status: OPEN.** Measured 2026-09-01 on `fix/jdk-only-huc-drift-and-hr-panic-20260901`
(branched from dev `222384268`). Found while running the `--features synthetic-jdk`
arm that `bug-huc-setdoinput-dropped-and-getrequestproperty-joined-20260826.md`
§4 asked for; recorded here rather than in that page, which is about the
shipping accessors.

## 0. What happens

`probes/HucAccessors.java` — 30 accessor checks, no network — under
`--synthetic-jdk`:

```text
  DIFF getURL(): got=null want=http://127.0.0.1:1/path?q=1
  ok   getRequestMethod() default = GET
  NoSuchMethodError: java/net/HttpURLConnection.getDoInput()Z
      at HucAccessors.main(HucAccessors.java:36)
```

preceded by a burst of

```text
WARN zgc: zgc real: field index OOB index=0 num_slots=0 op="set"
     ... index=1, 2, 3, 4, 5 (set), then 5, 0, 1, 2, 3, 8 (get)
```

`index=0..5` in `set` order is `huc_init` writing `HUC_CONN_ID`, `HUC_URL_STR`,
`HUC_METHOD`, `HUC_REQ_HEADERS`, `HUC_REQ_BODY_STREAM`, `HUC_DO_INPUT`. Every
field access on the carrier is out of bounds.

The shipping binary passes the same probe 30/30 in both compatible and
`--jdk-only`, so this is specific to `--features synthetic-jdk`.

## 1. It is not a stale reference

`num_slots=0` is also the documented signature of a read into a compacted-away
object — `zgc.rs` says so twice, because `compact_low_to` zeroes the vacated
span and the reader then walks a zero-length header. Ruled out directly:

```text
CRATONVM_ZGC_RELOCATE=0   ->  identical warnings, identical DIFF
```

The object really does declare zero slots.

## 2. Where it comes from — and one fact that does not reconcile

`--dump-native-registry --synthetic-jdk` says `java/net/URL.openConnection`
`owns=True` at `native-builtins/src/net_phase_e.rs`, and that body allocates the
carrier with

```rust
let conn = try_alloc_concurrent_synthetic(ctx, carrier, 16)?;
```

`layout_alias`'s census — which is OFF by default, and is the reason none of this
appears in an ordinary run — names the site exactly. `CRATONVM_DBG=layout-alias`,
same probe, same binary:

```text
WARN cratonvm_native_api::layout_alias:
  native allocated slots against a class declaring NONE ...
  class="java/net/HttpURLConnection" requested_fields=16 real_fields=0
  direction="undeclared"
  site=native-builtins/src/net_phase_e.rs:10794:24
  site=HucAccessors.main([Ljava/lang/String;)V
```

Three rows, two sites — the Rust funnel and the Java frame that entered it. The
census's own text lists what `real_fields=0` can mean and puts this case third:
*"a fabricated stub / the `ClassId::new(0)` fallback arm standing in for a class
whose real layout is WIDER, in which case this object is SHORT."*

**The two instruments do not agree, and this record does not pretend they do.**
The census says the object is *"exactly `requested_fields` wide"* — 16. The GC
guard reads `header.num_slots()` and reports **0**:

```text
WARN zgc: zgc real: field index OOB index=0 num_slots=0 op="set"
```

Both are measured, on the same run, about the same class. They cannot both
describe the same object, so one of these is true and none of them has been
shown:

* the funnel asked for 16 and something downstream still produced a 0-slot
  header (in which case the clamp the census believes in did not happen);
* the guard is reading a *different* `HttpURLConnection` — one allocated by
  another path that never reached the funnel, so the census never saw it;
* the header is 16 wide and `num_slots()` is answering from somewhere other
  than the allocation.

Deciding between them is a `CRATONVM_DBG_ZGC_CORPSE` / header-dump question and
is exactly the work this page is holding open. What is NOT in doubt is the
symptom: every accessor on the carrier is refused, `getURL()` reads null, and
`getDoInput()` is absent from the registry entirely.

The funnel already knows about this species in the abstract. Its own comment:

> ALLOCATION IS UNCHANGED by the widening above: still `max` ... Reporting and
> refusing are separate changes and this lane makes only the first — the
> over-allocating population has never been counted, and a funnel with ~2,000
> call sites is not where you discover that number by failing.

## 3. Why it matters, and why it is not urgent

`--features synthetic-jdk` is deliberately not in any default feature set
(`vm/Cargo.toml`, NEW-11), so nothing ships this. What it costs is an
**instrument**: the drift between `phases_early.rs::register_phase54_net_extras`
and `http_url_connection.rs::register_one` on 18 `HttpURLConnection` triples can
only be adjudicated in the mode where the synthetic-only body wins, and in that
mode the carrier cannot hold a field. `registrar_drift.rs`'s row for that
registrar therefore cannot be resolved by measurement today.

## 4. What is NOT claimed

* Not shown to be `HttpURLConnection`-specific. `try_alloc_concurrent_synthetic`
  has ~2,000 call sites and the funnel's comment says the over-allocating
  population has never been counted; this is one member of it, not a survey.
* The fix is not obvious and is not attempted here. Making the funnel refuse, or
  mint a class declaring `num_fields`, is the change its own comment warns
  against making blind.
* No claim that synthetic mode is broadly broken: a hello-world runs and exits 0
  on the same binary.

## Reproduce

```bash
cargo build --release -p cratonvm-cli --features synthetic-jdk
cratonvm --synthetic-jdk -cp <probes/out> HucAccessors
CRATONVM_ZGC_RELOCATE=0 cratonvm --synthetic-jdk -cp <probes/out> HucAccessors
```
