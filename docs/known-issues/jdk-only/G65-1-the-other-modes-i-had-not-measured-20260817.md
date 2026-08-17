# G65-1 — the other modes, which I had not measured

**Status:** MEASURED. **Provenance:** every row below is a run, at two commits
each where attribution required it. Binaries: `C:/craton/target-rel7`
(stock) and `C:/craton/target-syn` (`--features synthetic-jdk`), both from the
same source. Oracle HotSpot 25.0.3+9-LTS where a differential applies.

Written because the question "can these changes break the other modes?" was
asked of me, and the honest first answer was **"I don't know, I measured one
arm."** This record is what measuring the rest produced.

---

## 0. There are THREE modes, and the suite covers two

| mode | how it is selected | suite arm |
|---|---|---|
| Compatible | default, or `--real-jdk` | `SUITE=core` (61) and `SUITE=all` (99) |
| JDK-only | `--jdk-only` | `CRATONVM_ARGS="--jdk-only"` (99) |
| **Synthetic** | `--synthetic-jdk`, and **only on a binary built `--features synthetic-jdk`** | **none** |

The third is not a flag on a stock build — a stock binary refuses it, exit 1.
`run.sh` states the gap itself and why no arm exists: an arm nobody on the box
can execute "would either skip (a gate that cannot fail, the exact defect this
file's guards exist to catch) or fail every run for all eight lanes."

**That gap predates this work, but it means "unverified" was the accurate word
for synthetic mode across this entire session**, and nothing in my earlier
records said so.

## 1. Why the question was sharp

Almost none of this session's fixes are `--jdk-only`-specific. `String.intern`,
`String.valueOf(Object)`, `String.replace(CharSequence,CharSequence)`,
`Scanner(Readable)`, `Normalizer.normalize`/`isNormalized`, `URI.<init>` and
the `HttpsURLConnectionImpl` carrier writes are all **registrations that exist
in every mode**. Compatible mode runs the same changed bodies.

I had re-run only the `--jdk-only` arm after the last five fixes.

## 2. All three suite arms, at `89e2c56f1` and at HEAD

| arm | `89e2c56f1` | HEAD (`0dd253bf4`) |
|---|---|---|
| `--jdk-only` | 99 of 99 | **99 of 99** |
| `SUITE=all` (Compatible) | 94 of 99 | **94 of 99** |
| `SUITE=core` (default) | 60 of 61 | **60 of 61** |

The failing sets are identical **vector for vector**, not merely equal in
count: `RImmutableFactoryTypes RJdkProxyIface RJdkFunctionCombinators
RJdkEnumerations RServiceLoaderDoubleSource` in the Compatible arm, and
`RImmutableFactoryTypes` in the default arm. That identity is the attribution;
two runs agreeing on a total can still disagree about which vectors failed.

## 3. Synthetic mode, measured for the first time

It **compiles** clean with every change (`cargo check -p cratonvm-cli
--features synthetic-jdk`), and — because compiling is not running — it was
built and run.

One change had a specific, named risk there, and it is worth recording that
the risk was stated **before** the measurement rather than after: the
`Scanner` fix walks the class hierarchy looking for `java/io/Reader`, and a
flatter synthetic class graph would miss it and fall through to `toString()`,
turning an empty scanner into a *garbage* scanner. Measured:

```text
synthetic --synthetic-jdk, ScanProbe
  P1 Scanner(StringReader).next()   hello        <- the risk did not materialise
  P4 Scanner(StringReader).hasNext() true
  intern("a<U+D800>b").charAt(1)    d800         <- the intern fix carries
  String.valueOf((Object) s)        61,d800,62   <- the valueOf fix carries
```

Two surfaces answer differently in synthetic mode, and **neither is a
regression**:

* `replace(CharSequence,CharSequence)` is still `61,fffd,62` there. Synthetic
  mode stores `String` differently, so the units reader the fix depends on
  does not recover them. It was lossy before this change too. Synthetic mode
  is broadly lossier on this axis — `substring`, `concat`, `split` and
  `StringJoiner` are all lossy there while exact under `--jdk-only`.
* `Normalizer` is unreachable: `NoSuchFieldError` on `Form.NFC`, because
  synthetic mode has no JDK enum to resolve. The native cannot run at all.

## 4. Gates and unit tests, with attribution

| suite | HEAD | `e7e840264` | verdict |
|---|---|---|---|
| `duplicate_registration_gate` | 6 passed | — | green |
| `registrar_drift` | 7 passed | — | green |
| `registrar_reachability` | 5 passed | — | green |
| `stub_ratchet` | 9 passed, **1 failed** | **1 failed, identical** | **pre-existing** |
| `native-builtins --lib` | 23 failing | 23 failing, **same names** | pre-existing |
| `native-io --lib` | 507/3, `fis_*` | 507/3, **same three** | pre-existing |
| `native-collections --lib` | 130 passed | — | green |

`stub_ratchet` reports **1300 SyntheticStub registrations out of 12763,
baseline 1277** — and the count is **identical at `e7e840264`**, which does
double duty: it dates the failure before this work, and it proves these
changes added **zero** registrations. It also fails only in the
`no-management` configuration, which the test itself labels
"NON-SHIPPING: ten jmx registrars absent".

## 5. What this does and does not license

**Does:** every arm the suite can run is unchanged or better at HEAD, in all
three modes, with the failing sets identical by name and every failure dated
to before this work.

**Does not:** synthetic mode has **no vector coverage at all**, so §3 is four
probe rows, not a suite. A change that broke synthetic mode in a way those
four rows do not touch would still be invisible. Nobody should read §3 as
"synthetic mode is fine" — it is "synthetic mode was checked at four points
that the changed code reaches."

## 6. NOMINATIONS

**N1 — the synthetic arm `run.sh` declines to add.** Its reasoning is sound
for a shared box where no lane can build the binary, and it is now wrong in
one respect: the binary takes one `cargo build --features synthetic-jdk` and
this session built one. The design constraints are already written down in
`W8-E9-1-three-broken-oracles-and-the-suite-denominator.md`. An arm that runs
only when `CV_SYNTHETIC` points at such a binary — and **fails loudly rather
than skipping when it is set and broken** — avoids the gate-that-cannot-fail
trap the current comment is guarding against.

**N2 — `stub_ratchet`'s `no-management` baseline is 23 short and has been for
at least this session.** Either the ten jmx registrars moved 23 stubs, in
which case the constant wants re-freezing with that reason recorded, or
something added stubs and nobody noticed because the shipping configuration is
the one people run. The test's own message forbids raising the baseline
without an explanation, which is correct and is why this is a nomination
rather than a one-line change.

**N3 — synthetic mode's String is lossier than the other two modes, measured
in §3.** `substring`, `concat`, `split`, `StringJoiner` and
`replace(CharSequence,CharSequence)` all lose an unpaired surrogate there
while being exact under `--jdk-only`. That is a different implementation with
a different defect, not the one `G63-1` fixed, and no vector covers it.
