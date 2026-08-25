# `InputStream.transferTo` identified its receiver by reading slots it does not have — FIXED

| | |
|---|---|
| **Status** | ✅ **FIXED 2026-08-25** |
| **Was** | `zgc real: field index OOB index=1/3 num_slots=1 op="get"` on a live `CoyoteInputStream` |
| **Reproduced** | 100% solo: `catalina.servlets.TestDefaultServletRfc9110Section13` (16 hits), `catalina.servlets.TestWebdavServletOptionsUnknown` (8) |
| **After** | **0 hits** on both, both still `OK` (1210 tests / 104 tests) |
| **Fix** | `native-builtins/src/phases_late/zip_streams.rs` — `has_byte_array_stream_layout`; sibling guard in `native-builtins/src/properties_sidetable.rs` |
| **Guard** | `byte_array_stream_layout_tests`, three cases, in `zip_streams.rs` |

## The reader, named

The original page ended at "the reader is not identified, and guessing at it is
what produced the wrong suspect above". It is
**`java/io/InputStream.transferTo(Ljava/io/OutputStream;)J`**, and its
`ByteArrayInputStream` fast path opened with a **slot-shape duck test**:

```rust
let byte_array_stream_layout = matches!(
    (
        ctx.get_field(input, 0),
        ctx.get_field(input, 1),
        ctx.get_field(input, 3),
    ),
    (Value::Object(Some(_)), Value::Int(_), Value::Int(_))
);
```

`CoyoteInputStream` declares exactly one field (`ib`), and its supertypes
`ServletInputStream` / `InputStream` declare none — so slots 1 and 3 are read
two and four slots past the object. That is the whole signature: **index 1 then
index 3, both `op="get"`, one pair per call**, which is why the counts came out
as 8 pairs and 8 pairs.

Reached from `DefaultServlet.doPut` → `StandardRoot.write` →
`DirResourceSet.write` → `java.nio.file.Files.copy(InputStream, Path,
CopyOption[])`, i.e. every WebDAV/PUT of a request body.

### The instrument that named it, because the page had the wrong one

`CRATONVM_DBG_STRAYSTACK` dumps the culprit native and the Java stack for an
out-of-bounds slot access — but **every door it was wired to was a WRITE door**
(`NativeContext::set_field`, and the interpreter's `putfield`). Every hit in
this signature carries `op="get"`, so the dump stayed empty on all sixteen of
them and the reader stayed anonymous. Adding the READ twins
(`NativeContext::get_field`, and the interpreter's `getfield`) named it on the
first run. Both twins ship with this fix.

One more step was needed: `native_ring`'s callback→name map is only populated
when the ring is armed, so the first dump printed `CULPRIT-NATIVE=<cb@0x…>`.
Re-running with `CRATONVM_ENABLE_NATIVE_RING=1` printed the triple.

## Two things the original page got wrong, and they matter

* **`num_slots=0` was never the signature.** The warning prints
  `header.num_slots()`, which is the object's own count — and it reads **1**,
  matching the `header_num_slots=1` the corpse reporter printed beside it. The
  two lines never disagreed. Reading `num_slots=0` as "the bounds the accessor
  was given" was an interpretation with nothing under it, and it is what kept
  the corpse/compaction reading alive after `in_registry=true` had already
  falsified it.
* **The indices are 1 and 3, not 1 through 4.** Nothing ever read index 2 or 4.
  The pair is `pos` and `count` of a `ByteArrayInputStream` layout, which points
  straight at the offending shape test; "1 to 4" points at nothing.

## Why it was worth fixing rather than silencing

On ZGC, `check_field_index` catches the read and hands back a default, so the
duck test merely answers "no" noisily — no wrong result, which is why the class
PASSES with the warning in its stderr. That is the benign end of the range, not
the contract:

* a collector that does not bounds-check the slot reads whatever follows the
  object — the next object's header, or a free-list cell — and a
  `(ref, int, int)` answer found there **admits the fast path**, which then
  `set_field`s slot 1 of a stream that has no slot 1. An out-of-bounds read that
  decides a subsequent out-of-bounds write is how a wrong answer becomes heap
  corruption, and `CoyoteInputStream` being on the stack of the
  `TestSwallowAbortedUploads` SIGSEGV is what made that worth taking seriously;
* even **in bounds** the test identifies the wrong class. Any stream whose slots
  0/1/3 happen to hold `(ref, int, int)` passed. The shape is not distinctive,
  and nothing about it implies the `pos`/`count` contract the fast path assumes.

## The fix

Decide the layout by **class identity**, exactly as `native-io`'s
`input_stream_has_bais_layout` already did for the sibling family: slot count
first (so no read can go out of bounds), then the class name, then the subclass
walk. The bare `java/io/InputStream` arm is carried over from that function
unchanged, so no receiver the path used to accept is dropped.

A second, identical defect was found by grep in the same pass and fixed with it:
`Properties.load`'s drain has a "Strategy 2: by-index" fallback that read slots
0, 1 and 3 of an arbitrary `InputStream` with no slot-count check. A receiver
too short for the layout now leaves all three `None` and falls through to
strategy 3, which works for any real `InputStream`.

Two nearby shape reads were checked and left alone, because both already gate on
the class name before reading: `native_inflater_input_stream_init` in the same
file, and `dis_read_byte` in `native-builtins/src/classloader.rs`.

## Verification

| | before | after |
|---|---:|---:|
| `TestDefaultServletRfc9110Section13` — `field index OOB` | 16 | **0** |
| …and its result | `OK (1210 tests)` | `OK (1210 tests)` |
| `TestWebdavServletOptionsUnknown` — `field index OOB` | 8 | **0** |
| …and its result | `OK (104 tests)` | `OK (104 tests)` |

`TestSwallowAbortedUploads`, interleaved control/fix, three pairs at load 41–49:
control `rc=1, 1, 0`, fix `rc=0, 0, 0` (and 0 on a fourth run). The control's
failure is `testAbortedUploadUnlimitedNoSwallow` asserting no client exception
and getting `SocketException: Broken pipe` — a network-timing assertion, not the
SIGSEGV the original page cites. **The SIGSEGV did not reproduce on either
arm**, so 3/3 vs 0/4 is suggestive and is *not* evidence that this fix closed
it; at that load, per this repo's standing rule, it is not a result at all.

Gates: `cratonvm-vm --lib` 2617/0, `native-io` 526/0 (3/3 reruns),
`classloading` 797/0, `types` 581/0, `gc` 1687/0, `jit` 2105/0, `native-api`
338/0. `native-builtins` 4161 pass / 2 fail, both `shared_secrets_bridge::tests`
and both reproduced on pristine `origin/dev` with this branch's changes reverted
in place. `regression-suite/run.sh` **72 passed, 0 failed**.

## Reproduction (for the regression, on an unfixed build)

```bash
CRATONVM_DBG_STRAYSTACK=1 CRATONVM_ENABLE_NATIVE_RING=1 \
CRATONVM_YOUNGSCAN_STRIDE=1000000 \
  <cratonvm> --java-home <jdk25> --Xmx 2g -c "$CP" \
  org.junit.runner.JUnitCore \
  org.apache.catalina.servlets.TestWebdavServletOptionsUnknown
```

The class PASSES; the signature is in its stderr. Do not wait for a failure.

## What to keep from this

**A slot-shape duck test is an out-of-bounds read waiting for a shorter
receiver.** Ask the slot count before reading the slot, and prefer class
identity to shape whenever the question is "which class is this" — the shape
answer is both unsafe and imprecise, and on a bounds-checking collector it is
*quiet*, which is worse.

**Instrument both doors.** A diagnostic that covers writes and not reads reports
nothing at all for a read-only defect, and reads as an absence of evidence. Every
straystack door in this tree was a write door until 2026-08-25.
