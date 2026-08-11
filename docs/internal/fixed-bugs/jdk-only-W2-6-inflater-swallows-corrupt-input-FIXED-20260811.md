> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vector `RJdkJni` passes in the 53/1 run. Entirely in-lane; the one "Adjacent, out of this lane's files" item — `System.loadLibrary`/`System.load` discarding the load error — was taken by W5-1 (allowlist narrowed) and W6-6 (`NativeLibraries.load`) and is fixed.
>
> Previous location: `docs/known-issues/jdk-only/W2-6-inflater-swallows-corrupt-input.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# `Inflater` swallowed a corrupt-stream error and answered "no progress"

**Status:** FIXED in source 2026-08-07 (lane W2-6, JDK-only wave 2). Not yet
verified against a binary — see *How to verify* below.

## The failure

`regression-suite/src/RJdkJni.java` fails in **both** `--real-jdk` and
`--jdk-only`; HotSpot 25 runs it to `PASS RJdkJni (35 checks)`. Both CratonVM
arms produce the identical trace, which is the tell that this is an ordinary
Compatible-mode defect and not a strict-mode policy drop:

```
AssertionError: corrupt deflate input must raise DataFormatException
    at RJdkJni.zipNatives(RJdkJni.java:165)
    at RJdkJni.check(RJdkJni.java:44)
```

This is the assertion immediately behind lane L7's `Adler32` fix
(`L7-adler32-missing-natives.md`), which moved `RJdkJni` from `:118` to `:165`.
L7 audited the rest of `zipNatives` and recorded "`Inflater` round-trip and the
corrupt-input `DataFormatException` (lines 142-165) — `init(Z)J`,
`inflateBytesBytes`, `end` all registered". That was true and not sufficient:
the natives were *registered*, they just could not *fail*.

`RJdkJni.zipNatives` lines 154-165:

```java
Inflater bad = new Inflater();
boolean threw = false;
try {
    bad.setInput(new byte[] { 1, 2, 3, 4, 5, 6, 7, 8 });
    bad.inflate(new byte[64]);
} catch (DataFormatException expected) {
    threw = true;
} finally {
    bad.end();
}
check(threw, "corrupt deflate input must raise DataFormatException");
```

`new Inflater()` is `nowrap == false`, so a zlib-wrapped stream is expected.
`{1,2,...}` is not one: `CMF = 0x01` carries `CM = 1` rather than the mandatory
`8`, and the two-byte header check `(0x01 << 8 | 0x02) % 31 != 0`. zlib rejects
it on the header, before producing a byte.

## Root cause — a fabricated success where the spec mandates a failure

`native-builtins/src/zip_real.rs` classified every `flate2` decompress error as
"not an error":

```rust
Err(e) => {
    let needs = e.needs_dictionary().is_some();
    (false, needs)          // -> packed (0, 0, false, false)
}
```

A hard `Z_DATA_ERROR` therefore became a packed progress long of `0`, and
`Inflater.inflate` returned `0`. The caller could not distinguish corrupt input
from "needs more input", and `check(threw, ...)` saw `false`.

Two facts pin that a throw out of the native is the ONLY correct answer, both
measured against the JDK 25 image at
`C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot` rather than assumed:

1. `javap -p java.util.zip.Inflater` shows **all four** inflate natives
   declared `throws java.util.zip.DataFormatException`:

   ```
   private native long inflateBytesBytes(long, byte[], int, int, byte[], int, int)
           throws java.util.zip.DataFormatException;
   private native long inflateBytesBuffer(long, byte[], int, int, long, int)   throws ...;
   private native long inflateBufferBytes(long, long, int, byte[], int, int)   throws ...;
   private native long inflateBufferBuffer(long, long, int, long, int)         throws ...;
   ```

   libzip's JNI entry point maps `Z_DATA_ERROR` to
   `JNU_ThrowByName(env, "java/util/zip/DataFormatException", strm->msg)`.

2. `javap -c` on `Inflater.inflate(byte[],int,int)` shows there is **no**
   Java-level check that turns a zero-progress result into an exception: the
   bytecode calls the native, `lstore`s the packed long, and falls through to
   the unpack. The only Java-side handling of a throw is the catch at bci 72,
   which reads `this.inputConsumed` back to advance `inputPos` and re-throws.

So the exception cannot be manufactured anywhere but in the native frame.

`DataFormatException` is a **checked** exception (`extends Exception`,
constructors `()` and `(String)`), so an internal/uncatchable VM error is not a
substitute — it would escape the caller's `catch (DataFormatException)` and
abort the whole call chain instead of being handled. It is raised here as a
real, constructed, catchable throwable.

## Bug species

Same shape as the three a wave-1 lane found (`System.loadLibrary(absent)`
returning normally, `ModuleLayer.findModule` answering present for an absent
module, `Cipher.getInstance(bogus)` returning a synthetic `Cipher`): an error
path that swallows and returns `Ok`. The distinguishing feature of this
instance is that the swallow was *typed correctly* — the packed-long protocol
has no way to say "failed", so returning any long at all is a claim of success.

## What changed

All in `native-builtins/src/zip_real.rs`. No new native rows: the same four
triples that were already registered now report failure.

* `infl_do_decompress` returns a new `InflateStep` struct carrying
  `data_error: Option<String>` alongside the four progress values. `Z_NEED_DICT`
  is deliberately **not** a `data_error`: flate2 signals it as an `Err` whose
  `needs_dictionary()` is `Some(adler)`, and the JDK reports it through the
  `needDict` bit and `Inflater.needsDictionary()`, not an exception. Everything
  else (`Z_DATA_ERROR` / `Z_STREAM_ERROR`) becomes `Some(zlib message)`.
* `finish_inflate` converts an `InflateStep` into either the packed long or a
  thrown `DataFormatException`, and is the single exit for all four overloads.
* `throw_data_format` constructs the real class via
  `ctx.new_object_initialized("java/util/zip/DataFormatException",
  "(Ljava/lang/String;)V", ...)`, the same pattern
  `jca/message_digest.rs::throw_digest_exception` and
  `jca/key_factory.rs::throw_jca` use. If the class cannot be constructed it
  still raises a **catchable** `IOException` naming the fault rather than
  reporting success — the recorded rule is that a refusal must be a catchable
  throwable, never a silent `Ok`.
* `store_progress_fields` writes the receiver's `inputConsumed` /
  `outputConsumed` int fields before the throw, which is the channel the JDK's
  own JNI code uses and the only one left once the return value is discarded by
  an exception. It resolves the slot with
  `resolve_field_index_by_class_id` and fails closed on an out-of-range index,
  so it is a no-op on any layout that does not declare them.
* `infl_inflate_bytes_bytes` no longer carries its own copy of the decompress
  loop — it now goes through `infl_do_decompress` like the other three. The two
  copies had already drifted in their handling of `Status::BufError` comments;
  one core means the four overloads can never disagree about what counts as a
  data error.

Lock discipline: `infl_do_decompress` holds the handle-table `Mutex` for the
zlib step only and returns the guard-free `InflateStep`. `finish_inflate`
allocates a Java String and runs `DataFormatException.<init>` — re-entering
Java while holding a process-global mutex is the recorded lock-cycle shape, so
the split is load-bearing, not cosmetic.

## Deliberately unchanged

`infl_do_decompress` still answers "no progress" (not an error) for a handle
that is **not in the table** — a zero handle or an already-`end()`ed stream.
The Java wrapper's `ensureOpen()` is what rejects a closed `Inflater`, and
HotSpot would never reach the native in that state; turning it into a decode
error would invent a `DataFormatException` for a lifecycle bug the caller
cannot act on.

## Native triples: unchanged

| class | method | descriptor | kind |
| --- | --- | --- | --- |
| `java/util/zip/Inflater` | `inflateBytesBytes` | `(J[BII[BII)J` | `Bridge` (unchanged) |
| `java/util/zip/Inflater` | `inflateBytesBuffer` | `(J[BIIJI)J` | `Bridge` (unchanged) |
| `java/util/zip/Inflater` | `inflateBufferBytes` | `(JJI[BII)J` | `Bridge` (unchanged) |
| `java/util/zip/Inflater` | `inflateBufferBuffer` | `(JJIJI)J` | `Bridge` (unchanged) |

No registration row was added, removed, or re-kinded, so
`scripts/baselines/jdk-only-kind-map-25-linux.tsv`,
`scripts/baselines/jdk-only-bridge-ratchet.json` and
`native-builtins/tests/stub_ratchet.rs` are all untouched by this change.

## Test

`zip_real::tests::corrupt_input_fails_instead_of_reporting_no_progress` pins
both halves: the corrupt `{1..8}` stream must come back `Err`, and a valid
zlib stream through the *same* native must still inflate to the original bytes
and report `finished`. The negative half alone would pass for an
implementation that rejected everything — the recorded "pin the NEGATIVE half"
trap, run in the other direction.

The mock context cannot load a real `java.util.zip.DataFormatException`, so the
unit test asserts only that the call fails; the exception CLASS is what
`RJdkJni:165` measures on a real image.

## The rest of `zipNatives` past :165

Only one block remains (lines 167-179): `Deflater.end()` twice, then
use-after-end must raise `NullPointerException` or `IllegalStateException`.
Traced but **not** modified, because nothing in this lane's files decides it:

* JDK 25 `Deflater.ensureOpen()` throws
  `IllegalStateException("Deflater has been closed")` when
  `zsRef.address() == 0` (disassembled — the older `NullPointerException`
  wording is gone), which the test's `catch` accepts.
* `Deflater.end()` early-returns when `address() == 0`, so the double `end()` is
  a no-op at the Java level and never reaches `defl_end` twice.
* Whether `address` is actually zeroed depends on
  `Cleaner$Cleanable.clean()` reaching `DeflaterZStreamRef.run()`. Both
  `java/lang/ref/Cleaner` and `java/lang/ref/Cleaner$Cleanable` are on
  `native_override.rs::real_protected_stub_class_common`, so in real-JDK mode
  the SyntheticStub yields and real `CleanerImpl` bytecode runs.
* Note that `run()` zeroes `address` and calls the static `Deflater.end(addr)`
  native in the same method, so the Java-side address and this file's handle
  table are removed together or not at all. There is no state in which
  `defl_do_compress` could distinguish the two, which is why the "handle not
  found -> silent `(0, 0, false)`" branch was left as it is: changing it cannot
  affect this assertion either way.

## How to verify, once a binary exists

```
cargo build --release -p cratonvm-cli
cargo test -p cratonvm-native-builtins zip_real::tests::corrupt_input

javac -d regression-suite/build regression-suite/src/RJdkJni.java
target/release/cratonvm --real-jdk -cp regression-suite/build RJdkJni
target/release/cratonvm --jdk-only -cp regression-suite/build RJdkJni
java -cp regression-suite/build RJdkJni      # HotSpot 25 oracle
```

The line that closes this one is
`CK RJdkJni crc32=3421780262 adler=152961502 roundTrip=true`, i.e. `zipNatives`
completing. `RJdkJni` has never executed past `:165` on this VM, so the next
assertion to move is `:179` (use-after-end) and then `libraryLoading`.

## Adjacent, out of this lane's files

`native-builtins/src/lang_system.rs:1483-1507` registers
`System.loadLibrary(String)` and `System.load(String)` as
`let _ = ctx.load_native_library(...)` — the error is discarded and the method
returns normally. `RJdkJni.libraryLoading` (`:212`, `:221`) requires a real
`UnsatisfiedLinkError` for a missing library, and `:201`/the
`loadedLibrary=` CK line requires `loadLibrary("zip")` to FAIL on this image
(HotSpot's oracle prints `loadedLibrary=net`, i.e. it fell through to the
fallback probe). Same swallow species, different owner.
