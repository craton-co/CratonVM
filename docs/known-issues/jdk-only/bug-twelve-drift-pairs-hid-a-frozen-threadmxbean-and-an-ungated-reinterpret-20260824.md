# Twelve of the 23 newly-visible drift pairs hid a frozen `ThreadMXBean` and an ungated `reinterpret`

**Status: FIXED 2026-08-24** for clusters 2 and 3 (12 pairs). Cluster 1 (11
pairs) is **OPEN and diagnosed** in §5.

## 0. Where the 23 came from

`ffe741b6f` taught `registrar_drift.rs`'s resolver one-level call-site
parameter binding. That brought the blind region under its 1000 ceiling and made
**23 `(synthetic-only pass, triple)` pairs visible that no gate had ever
reported**. Three clusters, and one reason for all three: each shipping twin is
a **class-parameterised registrar** (`fn register_x(r, cls: &str)`), the exact
form the resolver could not follow.

```text
register_phase54_net_extras        vs register_one                    11   OPEN
register_p59_management            vs register_thread_mxbean_for       8   FIXED
register_pe2_string_marshaling_on  vs register_p67_foreign_memory      4   FIXED
```

Left column is synthetic-only (all three are in `registrar_reachability.rs`'s
`SYNTHETIC_ONLY_CLOSURE`); right is the shipping twin. Registration is
last-write-wins and `register_synthetic_overrides` runs *after*
`register_essential_natives`, so **the left body wins under `--features
synthetic-jdk` and only the right body exists in every shipping mode,
`--jdk-only` included.**

**A drift row is not a defect by itself.** It says two registrations exist. What
it is worth is that it points at two bodies for one method, and reading both is
where the defects were. Neither of the two below is visible from a drift count.

## 1. ThreadMXBean — three counters frozen at bean construction

`jmx.rs::register_thread_mxbean_for` read FIELD SLOTS that
`init_thread_mxbean_fields` snapshots once, at `<init>`:

| triple | synthetic-only (`management.rs`) | SHIPPING (`jmx.rs`) |
| --- | --- | --- |
| `getThreadCount()I` | `active_thread_count()` — live | **`get_field(this, 0)`** — frozen |
| `getTotalStartedThreadCount()J` | `active_thread_count().max(1)` | **`get_field(this, 2)`** — frozen |
| `getDaemonThreadCount()I` | `daemon_thread_count(ctx)` — live | **`get_field(this, 3)`** — frozen |
| `getPeakThreadCount()I` | `peak_thread_count(ctx)` | `peak_thread_count(ctx)` — agree |
| `getAllThreadIds()[J` | `live_thread_ids(ctx)` | `live_thread_ids(ctx)` — agree |
| `isThreadCpuTimeSupported()Z` | `arbitrary_thread_cpu_time_supported` | same — agree |
| `isThreadContentionMonitoringSupported()Z` | **`0`** | **`1`** — opposite |
| `isThreadContentionMonitoringEnabled()Z` | `0` | atomic read |

`ManagementFactory.getThreadMXBean()` hands back one cached bean, so a shipping
binary answered the thread count **as of whenever that bean was first built,
forever**. HotSpot answers the live count.

**The file diagnoses this itself, and the fix had been applied to one counter of
four.** `getPeakThreadCount`'s comment: *"read the live process-wide high-water
mark rather than slot 1's construction-time snapshot — otherwise
`resetPeakThreadCount()` is invisible through this bean and the 'peak' is frozen
at whatever the thread count happened to be when the bean was allocated."*
`init_thread_mxbean_fields` makes the same argument a third time about slots 4/5
— *"a cached CPU time would be wrong the instant the bean was reused"* — and
does not follow it through either. The other three counters are now live.

## 2. The contention-monitoring pair — the comments are stale, the code is right

This is the one to be careful with, and the first reading of it here was wrong.

The two registrations answered `isThreadContentionMonitoringSupported()`
**differently — `false` synthetic-only, `true` shipping — and BOTH carried a
comment arguing for `false`.** `management.rs`: *"false is the measurement —
contention monitoring needs per-thread blocked/waiting DURATIONS, which nothing
in the VM records."* `jmx.rs` argues the same at greater length, cites the JMM,
names the Elasticsearch `HotThreads` case it was written for, and instructs
*"Report `false` (not supported)"* — directly above a body returning `1`.

**The capability exists and the prose describing its absence outlived it in two
files.** `ThreadJmxSnapshot::blocked_time_ms` and `waited_time_ms` are real
fields on `native-api`, documented there as *"cumulative monitor-enter blocking,
measured only while contention monitoring is enabled"*; `vm_exec.rs` fills them
from the registry, `jmx.rs` writes `snapshot.blocked_time_ms` into
`ThreadInfo.blockedTime`, and `native_set_thread_contention_monitoring_enabled`
calls `ctx.reset_thread_jmx_contention_stats()`.

So the shipping `true` is correct, and the consequence runs the OTHER way from
§1: **synthetic-JDK mode had been reporting a VM that does not support a feature
it does support.** Had the drift been resolved by "keep the synthetic-only body,
it is the careful one", this would have been a regression.

## 3. MemorySegment — the shipping copy loses the real-JDK layout, and a gate

`getUtf8String(J)` and `reinterpret(J)`, each on `PE_SEGMENT_INTERFACE` and
`CRATON_SEGMENT_CLASS` = the 4 pairs. The shipping copy was weaker in both.

* **`getUtf8String`** read `get_field(this, 0)` as the base address — right only
  for the synthetic six-slot carrier. On a real JDK-loaded segment **slot 0 is
  the byte LENGTH**, which is the precise confusion
  `panama_libffi::segment_address` exists to end: its own comment records
  `MemorySegment.ofArray(new byte[16])` faulting with
  `EXCEPTION_ACCESS_VIOLATION … read at address 0x10`, and **0x10 == 16 == that
  array's length**. It now points at `p67_segment_get_string`, which already
  served the JDK-22 spelling `getString`, already reads through
  `segment_address`/`segment_byte_size` (both segment models), and raises
  `IndexOutOfBoundsException` where the JDK does.
* **`reinterpret`** shipped with **no native-access check at all**, while the
  synthetic-only twin refused unless `native_access_enabled()` — calling the
  operation *"the second half of the arbitrary-memory primitive"*. Real JDK 25
  restricts `reinterpret` exactly that way, so the shipping body was the one
  diverging from HotSpot. The gated body survives, lifted out of its closure
  into `panama::pe_segment_reinterpret` so the shipping pass can name it.

`setUtf8String` and `allocateUtf8String` have no shipping twin and were left
where they are.

## 4. The evidence, measured rather than read off the source

`--dump-native-registry` on the PATCHED tree, both modes (debug binary
2026-08-24 18:59, `RStrings`). All 12 triples:

```text
kind = bridge · owns_slot = true · overwrote = null · one owner each
  the 8  -> native-builtins/src/jmx.rs
  the 4  -> native-builtins/src/phases_late/foreign_ffm.rs
identical rows in compatible mode and under --jdk-only
strict whole-registry: synthetic-stub 0, bridge 10334, intrinsic 661
```

**`overwrote = null` is the load-bearing field.** It is positive evidence that
nothing registered these triples ahead of the survivor — i.e. the duplicate is
gone, not merely losing the race. `Bridge` is `allowed_in(JdkOnly)`, so the
surviving bodies serve both modes.

The stale-row report was also checked the way the gate demands: every one of the
12 printed a live `registered by: …` with a file and line, **not** `<not
registered anywhere this scan can see>`. That distinction matters here more than
usual — two earlier re-takes in this file recorded `MemorySegment.{getUtf8String,
reinterpret}` as "collapsed twins" when the scanner had merely gone blind to the
synthetic-only side. The gate's detector fires only when BOTH sides vanish, so
it cannot catch one side going dark, which is the likelier failure and the one
that reads as a fix.

## 5. Cluster 1 — OPEN, diagnosed, not applied

`phases_early.rs::register_phase54_net_extras` carries its own
`java/net/HttpURLConnection` implementation as inline closures, competing with
`http_url_connection.rs::register_one` — a dedicated 5,000-line module reached
by the shipping path.

The direction is very likely the same as cluster 3 (delete the synthetic-only
copies), and there is a structural argument for it: `register_one` is called for
**four** class names including `sun/net/www/protocol/http/HttpURLConnection` and
`sun/net/www/protocol/https/HttpsURLConnectionImpl`, which are what a real JDK
actually instantiates, while the synthetic-only block covers only the abstract
`java/net/HttpURLConnection`.

**It is not applied here because the argument above is structural and clusters 2
and 3 both turned on reading the BODIES**, where the answer went one way in §1
and the opposite way in §2. Eleven pairs of substantial bodies is a separate
piece of work, and doing it on the structural argument alone is exactly the
shortcut this record exists to warn against.

## Reproduce

```bash
cargo test -p cratonvm-native-builtins --test registrar_drift --test registrar_reachability
```

`DRIFT_TRIPLES` 1237 → 1225, `BASELINE_TOTAL_PAIRS` 1370 → 1358, both −12.
`FAMILY_DRIFT_EXPOSURE`: `register_pe_panama` 16 → 12, `register_phase59_natives`
65 → 57. The 12 triples now live in `FIXED_NOT_DRIFTING` with the dump above.
