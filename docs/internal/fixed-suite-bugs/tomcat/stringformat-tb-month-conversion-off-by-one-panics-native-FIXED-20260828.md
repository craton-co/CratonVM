# `String.format("%tb", …)` panicked the native formatter: month 13 into a 12-element array

## Status

**FIXED, 2026-08-28.** `org.apache.juli.TestOneLineFormatterPerformance` fails
**3 of 3** runs on the pre-fix binary and passes **3 of 3** on the fixed one, on
the same host minutes apart. `probes/FormatTemporalLeapCensus.java` — 2 403 rows
over the inputs the conversion got wrong — matches a HotSpot JDK 25 run on every
row except one deliberately-labelled pre-1582 date, which differs *identically*
before and after (see "What still differs, and why it is not this").

The page was right that this is a real VM-internal defect and right that
"index == length" is an exact off-by-one. It was wrong about the trigger being
month-name lookup, and its two failed repro attempts failed for a reason nobody
could have guessed from the failure text.

## The panic

```
thread 'main-vm' panicked at native-builtins/src/lang_string.rs:9243:9:
index out of bounds: the len is 12 but the index is 12
```

Line 9243 was `TEMPORAL_DAYS_IN_MONTH[(month - 1) as usize]` inside
`temporal_days_in_month`, reached with `month == 13`. Not the month-**name**
table (`MONTHS_ABBR`, which `%tb` reads) — that one is indexed through
`month.clamp(1, 12) - 1` and could never have been it. The twelve-element array
that overflowed is the days-**in**-month table, and the caller was the month
walk inside `temporal_from_epoch_day`.

## Root cause: four copies of one wrong calendar

`lang_string` (`String.format`'s `%t` conversions), `lib.rs` (synthetic
`java.time`), `jdbc` (`java.sql` date/time) and `util_time` each carried their
own epoch-day ↔ `(year, month, day)` pair. All four were the same algorithm
because they were the same text — and `phases_early`'s `Calendar` pair, written
from a different source, was the one that had been written correctly.

The shared algorithm took the start of a year as

```rust
year_start(y) = 365*y + y/4 - y/100 + y/400
```

then set `remaining = abs_day - year_start(y)` and walked months off it, adding
`days_in_month(y, m)` until the remainder fit. **That expression steps by 366
days between `y` and `y+1` exactly when `y + 1` is a leap year, while the month
walk spends the leap day of `y`.** The two disagree for every year whose
leap-ness differs from its successor's, which is most of them:

* **Every day of a leap year decoded as the day before.** `2024-02-29` rendered
  as `28`, `2024-12-31` as `30`, `1600-02-29` as `1600-02-28`. Over 1900-2100
  that is **17 885 of 73 049 days** — one year in four, silently wrong, in
  `String.format`, `java.sql.Timestamp` and the synthetic `java.time`.
* **January 1 of a leap year did not decode at all.** There the remainder came
  out **366 in a 365-day year**, so the walk exhausted all twelve months, reached
  `month = 13`, and indexed `[i32; 12]` at 12. That is the panic.

The copies also used truncating `/` where the arithmetic needs floor division,
which is a second, independent wrongness for years ≤ 0.

## Why it looked nondeterministic, and why both standalone probes missed it

The page could not reproduce it outside the harness and reasonably suspected the
interface dispatch, the fixed-wall-clock loop, or the JUnit locale setup. It is
none of those. The test is four lines:

```java
for (int i = 0; i < iters; i++) {
    df.format(System.nanoTime());
}
```

**It passes `System.nanoTime()` as though it were epoch millis.** That is a
monotonic clock reading — time since boot on Linux — so read as milliseconds it
lands about eight to nine millennia out (year ≈ 10 750 on this host), and
consecutive iterations step forward by hours or days of "date". The loop
therefore sweeps arbitrary far-future dates, and eventually steps onto a leap
January 1.

So the failure is **uptime-dependent, not load-dependent**, and no probe built
from realistic timestamps was ever going to land on the one day in 1461 that
trips it. The page's first probe stepped ~30 days at a time and its second
repeated a single timestamp; neither can hit a specific day.

It is also why the page's other open question — whether this shares a mechanism
with the ecj `StackMapFrameCodeStream` off-by-one — resolves to **no**. Same
signature, unrelated cause.

## The fix

One calendar for the crate: `native-builtins/src/civil_date.rs`, holding
Howard Hinnant's `days_from_civil` / `civil_from_days` — the algorithm
`phases_early` already used, and the one `<chrono>` uses. All four call sites
delegate to it.

It is closed-form: **no year search, no month walk, and no indexed month table
at all**, so the out-of-bounds this page is named for is not merely fixed but
unrepresentable. It is exact for every proleptic Gregorian date in `i64`
epoch-day range, negative years included, and uses `div_euclid` throughout.

`days_in_month` survives for the callers that legitimately want it, and it still
panics outside 1..=12 on purpose: a silent clamp there would have converted this
panic into a wrong date rather than removing it.

## Evidence

| | pre-fix | fixed |
| --- | --- | --- |
| `TestOneLineFormatterPerformance` (3 runs) | 0 pass, 3 out-of-bounds panics | **3 pass, 0 panics** |
| `FormatTemporalLeapCensus` (2 403 rows) | dies on row 1 | **2 398 match HotSpot, 5 expected-diff** |
| `%tF` of `1600-02-29` | `1600-02-28` | **`1600-02-29`** (HotSpot: `1600-02-29`) |
| `regression-suite/run.sh` | — | **72 passed, 0 failed** |
| `cargo test -p cratonvm-native-builtins --lib` | — | **4 165 passed, 0 failed** |

The census walks January 1, February 29, December 31 and January 2 of every leap
year from 1904 to 2100, the December 31 before each, a 90-day contiguous run
across the 2023→2024 boundary, the far-future domain the failing test actually
samples, and pre-1970 dates — reporting `%tF`, `%tj`, `%tB`, `%tb`, `%tA`, `%ta`,
`%tc`, `%tD` and the Tomcat format string for each.

The unit tests deliberately do **not** rest on a round-trip. A round-trip would
not have caught the old defect: both halves shared the same wrong `year_start`,
so it was self-consistent while answering the wrong day. The oracle is an
independent calendar walked one day at a time from 1700 to 2200, checked in both
directions.

## What still differs, and why it is not this

Five census rows, all for one input, `-62135596800000` (proleptic Gregorian
0001-01-01): HotSpot renders `0001-01-03`, CratonVM `0001-01-01`. HotSpot's
`Formatter` goes through `GregorianCalendar`, which uses the **Julian** calendar
before the 1582-10-15 cutover; CratonVM is proleptic Gregorian throughout, so
the same instant is two days apart at year 1.

Measured on CratonVM **before and after** this fix: `0001-01-01` both times. A
separate, pre-existing modelling difference, not a residual of this one. The
census keeps those rows under the label
`preGregorianCutover.EXPECTED-DIFF` so the next reader does not re-diagnose
them, and `1583-01-01` — the first January after the cutover — is in the census
too and agrees with HotSpot on all three runtimes.

## Repro

```bash
javac -d /tmp/classes probes/FormatTemporalLeapCensus.java
java -cp /tmp/classes FormatTemporalLeapCensus > /tmp/hotspot.txt
<cratonvm-bin> --java-home <jdk25-home> -c /tmp/classes FormatTemporalLeapCensus > /tmp/craton.txt
diff /tmp/hotspot.txt /tmp/craton.txt
```

On the pre-fix binary the CratonVM run dies on its first row. The single
smallest reproducer is one line:

```java
String.format("%tF", Long.valueOf(1704067200000L));   // 2024-01-01
```
