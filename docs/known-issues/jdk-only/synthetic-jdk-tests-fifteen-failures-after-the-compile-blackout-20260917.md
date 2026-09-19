# 15 synthetic-jdk tests fail, and nobody could see them for eight days

**RESOLVED 2026-09-17 -- all 15 pass, and the whole module is green:
`cargo test -p cratonvm-vm --features synthetic-jdk --lib` 4209 / 0.** See
*Resolution* at the end. Filed the same day, the moment the module they live in
compiled again; kept under this name because other records cite it.

**Merged with a parallel follow-up (2026-09-18).** On `dev`, a same-day pass
fixed 3 of the 15 independently, and its text is kept below as history. Two
of its fixes match this branch's: `hex_format_from_hex_digits_p64` (a
receiver passed to a static method) and `async_socket_channel_p67` (a
missing receiver). For `object_output_stream_p70` it also changed
`native-builtins/src/serialization.rs`, so a null `OutputStream` now writes
an explicit null. This branch moved that test to a real stream instead,
since the JDK throws on `new ObjectOutputStream(null)`. The merged tree has
both. The 7 tests that pass says were stale were fixed here, as the
Resolution section records. On the merged tree the whole module is 4210 / 0
(4209 plus a test dev added), measured 2026-09-18.

---

**The parallel follow-up, as written on `dev`:**

**Follow-up, 2026-09-17 (same day): 3 of 15 fixed, 12 remain — and the
null-argument cluster is NOT one shared cause.** Reproduced first (all 15
still failed with the exact panic messages below, confirming the filing was
accurate), then traced.

The "one defect in how reference arguments reach the synthetic natives"
hypothesis below did **not** hold. 7 of the 8 null-argument-cluster tests
(`optional_or_present_returns_self`, `proxy_new_instance_stores_handler`,
`cipher_init_and_block_size`, `crypto_mac_basics_p68`,
`normalizer_is_normalized_p61`, `log_manager_p61`,
`basic_file_attributes_p59`) turned out to be **stale tests, not production
bugs**: each has a matching, measured, dated (several 2026-09-10, inside the
blackout window) code comment showing the production native's null-handling
was deliberately TIGHTENED to match real JDK behavior during the blackout —
`Optional.or` now calls `Objects.requireNonNull(supplier)` unconditionally
per real `Optional.java` (JDK 9+), `Cipher.init`/`Mac.init` now throw
`InvalidKeyException` on a null key (measured, L6JcaSweep row 108/124),
etc. — while the test module, unable to compile, never got updated to match.
Confirmed by reading `native-api/src/registry.rs`'s `register()`: a
retired-shadow triple is only re-tagged `SyntheticStub`, never unregistered,
so the "native got deleted" theory is refuted too. Left these 7 untouched —
correct production behavior, stale test, out of scope for this pass.

**Fixed (3): all three were argument-passing bugs in the TEST, not the
native:**
* `object_output_stream_p70` — genuine production bug, not a test bug: a
  null `OutputStream` constructor arg left the field slot at its zeroed
  allocation default, which reads back as `Value::Int(0)` instead of
  `Value::Object(None)` — exactly the doc's "same bit pattern read as the
  wrong type" hunch, via a missing-`else`-branch write, not an
  argument-passing defect. Fixed in `native-builtins/src/serialization.rs`.
  (Surfaced a second, pre-existing test-only bug — a raw `alloc_object`
  receiver can't dispatch `close()`'s internal `flush()` call — fixed by
  switching to `alloc_receiver` in the test.)
* `hex_format_from_hex_digits_p64` — test called the *static*
  `HexFormat.fromHexDigits(CharSequence)` as if it took a receiver,
  shifting every argument by one so the native read the `HexFormat` object
  itself as the string.
* `async_socket_channel_p67` — test called `isOpen()` with `&[]`, omitting
  the receiver every other call in the module passes as `args[0]`.

**`path_of_p57` — investigated, not fixed.** Looks like a real interaction
bug: since 2026-09-16 `Path` allocates under the real `sun/nio/fs/UnixPath`/
`WindowsPath` layout (`native-api/src/path_layout.rs`), which synthetic-jdk
mode has no real JDK classes to back — the writer and `toString()` reader
disagree on the resulting slot layout. Deep enough (a synthetic-mode/
real-layout interaction, not a one-line fix) that it's left for its own pass
rather than guessed at here.

**Full-suite re-run confirms zero regressions:** `cargo test -p cratonvm-vm
--lib --features synthetic-jdk`, all 4,346 tests — 4197 passed, 12 failed
(exactly the 12 left alone, same panic messages as originally filed), 137
ignored.

**Not done, still open:** the CI-gap concern in "The gate is not missing"
below is now STALE — `.github/workflows/ci.yml`'s `synthetic-jdk` job
already runs `cargo test -p cratonvm-vm --lib --features synthetic-jdk` as a
BLOCKING step (added since this doc was filed), so that specific gap is
closed; re-verify against `ci.yml` directly rather than trusting this note.
The "bisect at the pre-blackout commit" step under "Where to start" was
explicitly not attempted this pass (would need a separate build at an old
commit) and remains open, though largely superseded by the root-cause
tracing above.

---

**Original filing, below — now historical except where marked stale above.**

**Open, 15 of 4,197.** Filed 2026-09-17, the moment the module they live in
compiled again.

## Why they were invisible

`vm/src/vm/tests.rs` is `#[cfg(all(test, feature = "synthetic-jdk"))]` — about
4,300 tests that a default `cargo test --workspace` never builds. It stopped
compiling on 2026-09-09, when `InlineSite` grew `ldc_fp_pcs` (`d21ae9e3f`) and
`ir_typecheck_info` (`7dc2e83e8`) and `s31_inline_site_metadata_roundtrip`'s
exhaustive initializer did not grow with it. One E0063 took all of them.

That is a compile break, not a test failure, so it hid a set of *test* failures
underneath it. Repairing the initializer surfaced these 15.

**They are not caused by the repair, and this was measured rather than
assumed:** all 15 fail identically with the `InputStreamReader` fix reverted and
only the two `InlineSite` fields kept. They are drift that accumulated while
nothing compiled the module.

## The 15

| `zgc_refill_tlab_carves_a_chunk_and_retire_returns_the_tail` | ZGC must hand the VM thread's TLAB a chunk |
| `object_output_stream_p70` | assertion `left == right` failed   left: Int(0)  right: Object(None) |
| `crypto_mac_basics_p68` | called `Result::unwrap()` on an `Err` value: InternalError(Runtime(IllegalArgumentException { message: "No installed provider supports this key: (null |
| `normalizer_is_normalized_p61` | called `Result::unwrap()` on an `Err` value: InternalError(Runtime(NullPointerException { message: Some("Cannot invoke \"java.text.Normalizer$Form.ord |
| `optional_or_present_returns_self` | called `Result::unwrap()` on an `Err` value: InternalError(Runtime(NullPointerException { message: None })) |
| `basic_file_attributes_p59` | called `Result::unwrap()` on an `Err` value: InternalError(Runtime(NullPointerException { message: None })) |
| `proxy_new_instance_stores_handler` | called `Result::unwrap()` on an `Err` value: InternalError(Runtime(NullPointerException { message: Some("Cannot read the array length because \"interf |
| `scanner_close` | called `Result::unwrap()` on an `Err` value: InternalError(Linkage(NoSuchMethodError { class_name: "java/util/Enumeration$Impl", method_name: "close", |
| `path_of_p57` | called `Option::unwrap()` on a `None` value |
| `async_socket_channel_p67` | assertion `left == right` failed   left: Int(0)  right: Int(1) |
| `hex_format_from_hex_digits_p64` | called `Result::unwrap()` on an `Err` value: InternalError(Runtime(IllegalArgumentException { message: "string length greater than 8: 55" })) |
| `log_manager_p61` | called `Result::unwrap()` on an `Err` value: InternalError(Runtime(NullPointerException { message: Some("Cannot invoke \"Object.hashCode()\" because \ |
| `u6_datagram_channel_connect_disconnect` | called `Result::unwrap()` on an `Err` value: InternalError(Runtime(IOException { message: "DatagramChannel.disconnect: ������� ���������� ������������ |
| `cipher_init_and_block_size` | called `Result::unwrap()` on an `Err` value: InternalError(Runtime(IllegalArgumentException { message: "No installed provider supports this key: (null |
| `scanner_from_bais` | called `Result::unwrap()` on an `Err` value: InternalError(Runtime(NoSuchElementException { message: "no more elements" }))   failures:     vm::tests: |

## The shape most of them share

Eight of the fifteen are an object argument arriving as null, or as a zero that
should have been a reference:

* `object_output_stream_p70` asserts `Object(None)` and gets `Int(0)` — the
  same bit pattern read as the wrong type, which is the tell;
* `normalizer_is_normalized_p61`: `"form" is null`, a `Normalizer.Form` enum
  argument;
* `proxy_new_instance_stores_handler`: `"interfaces" is null`, a `Class[]`;
* `cipher_init_and_block_size` and `crypto_mac_basics_p68`: `No installed
  provider supports this key: (null)` — the `Key` argument;
* `log_manager_p61`: `Cannot invoke "Object.hashCode()"` on a null;
* `optional_or_present_returns_self` and `basic_file_attributes_p59`: bare NPEs
  with no message.

That is a hypothesis, not a diagnosis: one defect in how reference arguments
reach the synthetic natives would produce every row above, and `Int(0)` for
`Object(None)` is exactly what a slot read with the wrong tag looks like. It has
NOT been traced. Do not assume the count is eight until someone has.

The other rows look independent: `zgc_refill_tlab_...` is a collector
assertion, `scanner_close` is a missing `java/util/Enumeration$Impl.close`,
`hex_format_from_hex_digits_p64` rejects a 55-char string against an 8-char
limit, and `u6_datagram_channel_connect_disconnect` reports an OS error whose
message arrives mojibake'd (a Windows ANSI codepage string read as UTF-8 —
cosmetic, but it means the real errno is unreadable in a failure report).

## Where to start

1. **Bisect, don't guess.** The module last compiled before `d21ae9e3f`
   (2026-09-09). Build at that commit `--features synthetic-jdk` and run these
   15: the ones that passed there have a cause inside a known eight-day window,
   and the ones that already failed are older than the blackout.
2. Take the null-argument cluster as one investigation, not eight.
3. `docs/known-issues/jdk-only/synthetic-jdk-feature-shadows-real-classes-in-real-jdk-mode-20260917.md`
   is a sibling finding from the same session about the same feature, and the
   two should be read together: that one is about the feature build being run
   against a real JDK, this one about the feature build's own tests.

## The gate is not missing

`ci.yml`'s `synthetic-jdk` job runs `cargo check --all-targets --features
synthetic-jdk -p cratonvm-vm -p cratonvm-native-builtins`, and its own comment
says `--all-targets` is deliberate *precisely* because the lib/bin scope would
not catch this module's drift. It also runs
`cargo test -p cratonvm-native-builtins --lib --features synthetic-jdk` as a
blocking signal.

So the compile break should have been caught on 2026-09-09. Whatever the reason
it was not — the job not reaching this commit, or its red being passed over —
is a more valuable thing to fix than any single row in the table above. Note
also that the job's `cargo test` step covers `cratonvm-native-builtins` only:
`cratonvm-vm`'s 4,300-test module is CHECKED but never RUN, which is why these
15 would have stayed hidden even with the compile break fixed.

## Resolution (2026-09-17)

The "null-argument cluster" hypothesis was right about the SHAPE and wrong about
the SIDE. There was no defect in how reference arguments reach the natives.
Traced one by one, with JDK 25 as the oracle:

**Eleven were tests encoding pre-JDK behaviour.** The natives were brought to
JDK parity by the null-contract waves, and several now run the real JDK
bytecode; these tests were written against the old lenient stubs. In every null
case the VM's error text matched the JDK's verbatim.

| test | what the test did | JDK 25 |
|---|---|---|
| `optional_or_present_returns_self` | `Optional.of(v).or(null)` | NPE -- `requireNonNull` precedes the presence check |
| `normalizer_is_normalized_p61` | `isNormalized(s, null)` | NPE on `form.ordinal()` |
| `proxy_new_instance_stores_handler` | `newProxyInstance(null, null, h)` | NPE reading `interfaces.length` |
| `cipher_init_and_block_size` | `init` with a class-0 "key" | InvalidKeyException |
| `crypto_mac_basics_p68` | `Mac.init(null)` | InvalidKeyException |
| `log_manager_p61` | `getProperty(null)` | NPE in `Hashtable.get` |
| `basic_file_attributes_p59` | `readAttributes(p, null)` | NPE |
| `object_output_stream_p70` | `new ObjectOutputStream(null)` | NPE -- rewritten on a real stream |
| `hex_format_from_hex_digits_p64` | a receiver passed to STATIC `fromHexDigits` | the "55" is `HexFormat.of().toString().length()` |
| `async_socket_channel_p67` | `isOpen()` with NO receiver | -- |
| `scanner_close` | asserted `hasNext()` after `close()` works "in our impl" | IllegalStateException "Scanner closed" |

Each keeps its intent and now gets a valid argument; no implementation was
weakened to accept an invalid one.

**Two were a trap in the fixture itself.** `ClassId::new(0)` is used throughout
`vm::tests` as if it meant "no class". It does not: it is a real registered
class, and a virtual call on a fabricated class-0 object dispatches INTO it --
which is why `scanner_close` failed with `NoSuchMethodError` on
`java/util/Enumeration$Impl.close`, a class the test never names.
`scanner_from_bais` read nothing for the same reason. Both now use
`alloc_receiver`. **Any other test that fabricates a class-0 object and lets a
virtual call reach it has the same latent problem.**

**One was an opt-in feature.** `zgc_refill_tlab_...` pinned ZGC TLAB bridging,
which was measured 4/4 slower and ships default-off as `CRATONVM_ZGC_JIT_TLAB`.
The test now opts its own heap in via `set_vm_tlab_enabled`.

**Two were real defects, both fixed:**

* `path_of_p57`: in a synthetic-JDK build `Path.of` produced a ZERO-SLOT object.
  `alloc_concrete` falls back to the `java.nio.file.Path` interface when
  `sun.nio.fs.*Path` is absent, and the interface declares no fields, so every
  write was dropped. The `""` it returned was not data but `p57_read_path`'s
  re-entrancy guard. `alloc_platform_path_object` now guarantees
  `PathSlots::width`.
* `u6_datagram_channel_connect_disconnect`: `DatagramChannel.disconnect()` on
  Windows passed a 16-byte `AF_UNSPEC` sockaddr to a dual-stack IPv6 socket,
  which wants 28 bytes -- WSAEFAULT (10014). It now connects to a zeroed
  address of the socket's own family, and native-api tests check the OS reports
  no peer afterwards. (The mojibake this record noted had already been fixed;
  the message arrives readable now.)

**Found by the full run, not in the original 15:** the first version of the
`BigDecimal` fix on this branch broke `bigdecimal_extreme_scale_refusals_f31`.
Corrected -- see `synthetic-bigdecimal-comparison-keeps-the-f64-body-20260917.md`.

**Still open, recorded rather than changed:** this VM's
`ObjectOutputStream.<init>` accepts a null stream, where the JDK throws NPE.
Tightening a native is a behaviour change with its own blast radius.
