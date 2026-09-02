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

## 2. Where it comes from

`--dump-native-registry --synthetic-jdk` says `java/net/URL.openConnection`
`owns=True` at `native-builtins/src/net_phase_e.rs:10626`. That body allocates
the carrier with

```rust
let conn = try_alloc_concurrent_synthetic(ctx, carrier, 16)?;
```

`try_alloc_concurrent_synthetic` resolves the class first; when the class loads
but declares **zero** instance fields, `n = num_fields.max(real)` still asks for
16 slots, but the object's class says zero and the GC's bounds guard rejects
every access against that.

The funnel already knows about this species. Its own comment:

> ALLOCATION IS UNCHANGED by the widening above: still `max` ... Reporting and
> refusing are separate changes and this lane makes only the first — the
> over-allocating population has never been counted, and a funnel with ~2,000
> call sites is not where you discover that number by failing.

and `layout_alias::classify` returns `Undeclared` for exactly `real == 0`. So
the case is **observed** — via `layout_alias::observe_from_rust`, which feeds a
census rather than a log, which is why the run's stderr shows the GC guard's
complaint and never the fabrication's.

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
