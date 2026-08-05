# L5b / L5c — `register_with_kind` for `native-awt` and `native-builtins` — **DONE 2026-08-05**

> **Retired.** L5's own doc said *"this lane is the template; L5b/L5c repeat it
> for other crates and can run in parallel once this one has set the pattern."*
> They have. **603 registrations across 475 call sites now state their kind —
> `native-awt` 21, `native-builtins` 582 — taking `kind_stated` from 96 rows to
> 699, with zero `kind` changes, the registration totals unchanged, the
> `stub_ratchet` baseline unmoved at 157, L6's bridge ratchet unmoved at
> 10,069 / 4,755, and the `CRATONVM_NO_STUBS=1` drop list byte-identical at 436
> entries.**
>
> What they left open is filed as an open record:
> **`docs/known-issues/jdk-only/l5bc-awt-builtins-bridge-residuals.md`**.

There was never a separate lane doc for L5b/L5c — L5 named them and set the
rule. This is the record of what executing that rule on the other crates
actually produced, and it differs from L5 in four ways worth keeping.

## 1. The one adjudicated `bridge` verdict outside `native-io` did not survive

L5's Rules say to start from the `JDK-ONLY-CLASSIFY` markers, because "those
tagged `bridge` with evidence are safe to state explicitly". Across the whole
tree outside `native-io` there is exactly **one** such marker:
`native-awt/src/natives.rs::register_headless_natives`, asserting that
`sun/awt/PlatformGraphicsInfo.hasDisplays0()Z` is ACC_NATIVE in JDK 25.

It is not, on the image we measure on. A Linux JDK 25 `PlatformGraphicsInfo`
declares four ordinary bytecode methods and no `hasDisplays0` at all;
`hasDisplays0` is on the Windows and macOS variants of the class. The census
says `declared: false` and `javap` agrees.

So the only marker that licensed a statement outside `native-io` is the one
registrar that states nothing, and every one of the 603 statements that *were*
made rests on the census instead. **The marker was the starting point; the image
was the evidence.** That inversion is the main thing to carry into any later
lane.

## 2. The unit of adjudication is still the row, and it cost 21 sites

L5 found that one `for cls in [...]` site can produce rows with different
verdicts. `native-builtins` has 21 such sites, and they share one idiom: a loop
over a compatibility set — `WinNTFileSystem`/`UnixFileSystem`, `getLength`/
`getLength0`, `compareAndExchangeInt`/`weakCompareAndExchangeInt`,
`getStackTraceId(IJ)J`/`(I)J` — in which only the running JDK's spelling
exists. Those sites were **skipped, not claimed**, leaving 24 adjudicable rows
on the table. `native-awt` had one such site (thirteen `initIDs` classes, twelve
declared, one not) and it was split, the same way L5 split `nio_native.rs`.

Skipping was the right call for `nio_file.rs`'s 18 sites specifically: they are
one 400-line nested double loop over live filesystem natives, and restructuring
that is a behaviour-risk change that should not ride along with a mechanical
statement pass.

## 3. `native-collections` needs no migration at all

Its single `set_category(Bridge)` covers 1,350 rows and the image declares
`ACC_NATIVE` on **zero** of them. There is nothing for a `register_with_kind`
lane to state there — not "not yet", but nothing. Everything in that crate is a
reclassification question, which is exactly what its own marker has said since
the ambient-category audit.

## 4. Scale changes what the ambient default means

`native-builtins` holds 9,243 registration rows, 8,187 of them `Bridge`, and
7,581 of those have no `ACC_NATIVE` target. `native-awt` takes its kind for all
188 of its rows from one `with_category(Bridge, register_all)` line. Neither
line was touched: they are what keep those registrations alive under
`CRATONVM_NO_STUBS`, and narrowing either is the 2026-07-14 regression shape.

## Verification

Two release binaries from the same tree, JDK 25 / Linux:

* census `--real-jdk`: totals identical (`intrinsic 687, bridge 10842,
  synthetic-stub 387`), zero `kind` churn over 10,780 distinct triple+kind rows,
  `kind_stated` 96 → 699;
* census `--jdk-only`: same, 11,529 rows, `synthetic-stub: 0` either side;
* `CRATONVM_NO_STUBS=1` both arms: boots; drop list byte-identical, 436 entries;
* `regression-suite/bridge-ratchet.sh`: PASS, 10,069 / 4,755, unmoved — as it
  must be, since the rows a migration may state are the rows that *have* an
  `ACC_NATIVE` target and were therefore never in that number;
* `stub_ratchet`: 4 passed, baseline 157 unmoved;
* `cargo test --release -p cratonvm-native-builtins --lib`: 3,275 passed, 0
  failed;
* `cargo test --release -p cratonvm-native-awt`: 259 passed, 1 failed —
  `image::tests::get_rgb_oob`, which fails identically on unmodified dev
  (verified by stashing the change), i.e. pre-existing.
