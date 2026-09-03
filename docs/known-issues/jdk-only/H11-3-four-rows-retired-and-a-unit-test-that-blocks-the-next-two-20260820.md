# H11-3 — four interface rows retired, and a unit test outside this crate is what keeps the next two

**Status: FIXED-UNVERIFIED — no binary carrying these changes has been built or
run.** This lane was forbidden to build. The **evidence** behind the change is
MEASURED (`H11-1`, `H11-2`); the **effect** of the change is predicted, and §3
says what falsifies each prediction.

**Provenance.** Every measurement cited is from the prebuilt
`C:/craton/target-jdkonly-h2/release/cratonvm.exe` at `fe59bf9d9`, **which does
not contain these edits**. No `cargo` command of any kind was run in this lane.

Commits: `c97e03ffe` (the change), plus the self-correction commit and this
record.

Lane H11, 2026-08-20. Merge/base note: see `H11-1`.

---

## 1. What changed

| Change | file | strict mode | compatible mode |
|---|---|---|---|
| **delete 4 registrations** — `java/io/DataInput.{readInt()I, readLong()J}`, `java/io/DataOutput.{writeInt(I)V, writeLong(J)V}` | `native-io/src/lib.rs`, `register_data_stream_natives` | **−4 rows** | **−4 rows** |
| correct `register_data_stream_natives`' header + in-function claim | same | none | none |
| correct `register_scanner_natives`' header, and write out the blocked deletion at its foot | same | none | none |
| answer `pipe.rs`'s own JDK-ONLY-CLASSIFY block with the `invocations` it asked for; correct the "dispatch lands here either way" comment | `native-io/src/pipe.rs` | none | none |
| name the fabrication site at `aio_assc_open` | `native-io/src/async_socket.rs` | none | none |
| name the fabrication site at `alloc_afc_channel` | `native-io/src/lib.rs` | none | none |

**Four registrations. Everything else is a comment.** The diff is
+195/−21 across three files and 4 of those 21 deleted lines are live code.
Do not let the volume read as a fix.

---

## 2. Why the four were safe, and what "safe" rests on

Full argument in `H11-1`; the short form:

1. **Dispatch keys on the receiver's runtime class** (`H11-1` §2.1, §3
   MEASURED), and the one fallback walk follows `superclass` links only
   (`invoke.rs`:3677), so it never visits an interface.
2. **A second, independent barrier** drops interface instance-method natives at
   step 6 of `execute_invoke_kind` and at `vm_exec.rs`'s `override_cb` arm
   unless the triple is force-listed. None of these four is
   (`H11-1` §2.4, found by `H8-1`).
3. **MEASURED, the strongest single line in this record:** across 15 corpus
   vectors plus a purpose-built probe, the four interface rows report
   `invocations: 0` **in the same runs** where
   `java/io/DataInputStream.readInt()I` takes **540** calls and
   `java/io/DataOutputStream.writeInt(I)V` takes **53**. The traffic exists in
   quantity and every call went to the concrete class row. This is why the zeros
   are informative rather than merely absent (`[zero@consumer]`).
4. **A user implementor is not intercepted.** `DataInput di = new
   MyDataInput(); di.readInt()` returns the user's `9999` on CratonVM, identical
   to HotSpot 25.0.3+9.
5. **Nothing mints a receiver named `java/io/DataInput` or
   `java/io/DataOutput`** — grepped across every allocation helper in the
   workspace, multiline-aware.
6. **No other crate registers these triples** (`dupX = 0`), so the deletion
   actually removes four registry rows rather than handing them to somebody
   else's callback — the trap `H11-2` §4 documents for `Pipe`.
7. **No test asserts them.** `native-builtins/tests/registrar_drift.rs`'s
   expected-triple lists contain `java/io/DataInputStream` rows only;
   `scripts/jdk-only-kind-map.py` states at its head that *"removed rows pass
   and are reported"*; the bridge ratchet treats a falling `Bridge` count as
   IMPROVED. Checked, not assumed.

The 33 `DataInputStream` / `DataOutputStream` rows are untouched. Those are the
load-bearing half — the one whose 2026-08-19 `Bridge`→`SyntheticStub` retag
produced `RDataInputFastPull: skipped.next = -19` — and this change does not go
near them.

---

## 3. Predictions and falsifiers

**Baseline to compare against.** All fifteen vectors below printed their
`PASS <Class>` line on the prebuilt binary at `fe59bf9d9`, `--jdk-only`,
compiled to a private scratch dir: `RDataInputFastPull RJdkNio RJdkAsyncChannel
RJdkWatchService RJdkStrict RSerial RNioNoFollow RChannelInterrupt
RSocketChannelInterrupt RFileTimes RJdkNet RStrings RExceptions RJdkIntrinsics3
RChmKeySetView`.

### 3.1 The registry

`--dump-native-registry`, both modes:

* **PREDICTED:** the four rows `java/io/DataInput.readInt()I`,
  `DataInput.readLong()J`, `DataOutput.writeInt(I)V`, `DataOutput.writeLong(J)V`
  are **absent**. `native-io`'s strict-mode row count falls 925 → 921. Every
  `java/io/DataInputStream.*` and `java/io/DataOutputStream.*` row is
  **unchanged**, including `registered_by` line numbers, which shift but do not
  move file.
* **FALSIFIER:** any `DataInputStream`/`DataOutputStream` row changing `kind`,
  `owns_slot` or callback. That would mean the deletion had a reach this record
  did not predict.

### 3.2 The vectors

* **PREDICTED:** all fifteen stay `PASS`. `RDataInputFastPull` and `RSerial` are
  the two that actually drive these natives (540 + 53 calls) and both drive them
  through the concrete classes.
* **FALSIFIER, the specific one:** an `AbstractMethodError` naming
  `java/io/DataInput.readInt`, `DataInput.readLong`, `DataOutput.writeInt` or
  `DataOutput.writeLong`. Those four image methods have `has_code: false`, so
  that error is exactly what `execute_invoke_kind`'s `recv_is_bare_object`
  rescue (`invoke.rs`:1617, which substitutes the CP class when a receiver
  arrives as a bare `java/lang/Object`) would now produce where it previously
  found a native. **That branch is the one reachability path this lane could not
  construct a witness for** (`H11-1` N2). If it fires, restore the four lines
  and record the receiver — the removal is a four-line revert.

### 3.3 The whole-corpus bar

Verdict-neutral, not green. The standing baseline the building lane re-measures:
`--jdk-only` 104/104, `SUITE=all` 99/104 (`RImmutableFactoryTypes`
`RJdkProxyIface` `RJdkFunctionCombinators` `RJdkEnumerations`
`RServiceLoaderDoubleSource`), `SUITE=core` 63/64. **A green run does not
confirm anything in `H11-1` or `H11-2`** — the corpus contains no vector that
implements `java.io.DataInput` directly, which is precisely the population the
four rows were supposed to serve. `[green≠quiet]`, and the trap this directory
records four instances of.

---

> **VERIFIED AGAINST A BINARY 2026-09-02.** §4's first bullet said "Nothing was
> compiled. I did not run `cargo build`, `cargo check` or `cargo test` ... That
> is a review, not a build." It has now been built.
>
> ```text
> cargo check -p cratonvm-native-io --tests    Finished, no errors
> ```
>
> `--tests`, not the bare form: the record's sibling `H7-1` §6a.1 records that
> `cargo check -p X` builds the LIB only, so the bare command would not have
> compiled the test trees at all.
>
> **The two things §4's review could only inspect by eye are now checked by the
> compiler.** Brace balance at the two deletion sites — it compiles. And the five
> callbacks whose only remaining reference is the concrete-class row are all
> still referenced, so no `dead_code` warning appears:
>
> ```text
> native_dis_read_int 4   native_dis_read_long 4   native_dos_write_int 4
> native_dos_write_long 4   native_scanner_close 5     (references in native-io/src)
> ```
>
> **The retirement this record proposed has since landed**, by the route §6's
> out-of-file list needed: `c97e03ffe fix(native-io): retire the four
> DataInput/DataOutput interface rows -- dispatch keys on the receiver, so they
> served nobody`. `H8-1` §5.1's `Closeable`/`AutoCloseable` rows went the same
> way on 2026-08-21, and `native-io/src/lib.rs:8902` preserves those two deleted
> lines verbatim with the reasoning — including that **`H11-3` N1 wrote the
> deletion out and could not make it because `vm/` was outside this lane's
> bounds.** N1 is closed by someone else's two-file commit.
>
> **Everything else in §4 stands, and this note closes none of it:** no vector
> was run against a binary containing these edits (§3's PASSes remain pre-change
> baselines), `recv_is_bare_object` is still unwitnessed, compiled frames are
> still uncovered, `H11-2` §3's `twin` column is still a heuristic, and the
> `java/nio/ByteBuffer` contradiction with `H5-1` §3.4 is still unchased.

## 4. What I did NOT verify

Stated plainly, because the rest of this record is confident:

* **Nothing was compiled.** I did not run `cargo build`, `cargo check` or
  `cargo test`. The edits are syntactically reviewed by eye and by
  `git diff`; brace balance in the two deletion sites was inspected, and the
  five callbacks whose only remaining reference is now the concrete-class row
  (`native_dis_read_int`, `native_dis_read_long`, `native_dos_write_int`,
  `native_dos_write_long`, `native_scanner_close`) were confirmed still
  referenced, so no `dead_code` warning is introduced. **That is a review, not a
  build.**
* **No vector was run against a binary containing these edits.** Every "PASS" in
  §3 is a pre-change baseline.
* **The `recv_is_bare_object` path is unwitnessed** (§3.2). A negative from an
  unwitnessed probe is not a ruled-out hypothesis — `[neg≠ruled out]`.
* **Compiled frames are not covered.** `H11-1` §5.2, N3.
* **The `twin` column in `H11-2` §3 is a heuristic** and every "(iii) partial"
  verdict inherits its imprecision.
* **`java/nio/ByteBuffer` and the typed buffers.** `H11-2` §6 records that these
  appear under no `native-io` `registered_by` in either mode's dump, contradicting
  `H5-1` §3.4's map. I did not chase it.

---

## 5. The self-correction, kept because it is the most transferable thing here

My first draft of the comment at the foot of `register_scanner_natives`
described `native_scanner_close`'s `Ok(None)` decline as a **live** silent
swallow, citing `H5-1` N2 and `HANDOFF-20260820` §7.

It is not live. `e9f08d42b` (`H8-C`) replaced that decline with a real yield
through `invoke_virtual_bytecode_only` — **inside the 58-commit gap this
worktree was cut behind**, on this same branch, by a lane running one round
earlier. I took the claim from two records rather than from the file, and both
records describe the tree as it was before that commit.

`[a triage page is stale the day after it is written]` is in the index and I
still did it. The specific, mechanical guard: **before repeating any behavioural
claim a record makes about a function, read the function.** Not the record's
line number — the function, by symbol. It costs one grep. Fixed in this lane's
second commit.

The same pass turned up something better than the correction: `H8-1` reached
"nothing reaches these interface rows" from a completely different mechanism
(the step-6 interface-default gate, `H11-1` §2.4) than the one I found
(receiver-keying at step 1). Two independent source arguments and one
measurement now agree, which is a much stronger position than any of the three
alone. Neither lane knew the other was working on it.

---

## 6. OUT-OF-FILE EDITS REQUIRED

**None made.** One is **required** for N1 below and belongs to whoever owns
`vm/`.

---

## 7. NOMINATIONS

**N1 — `java/io/Closeable.close()V` and `java/lang/AutoCloseable.close()V` are
measured dead and a unit test is the only thing keeping them.** The deletion is
written out verbatim in the comment at the foot of `register_scanner_natives`;
it needs a two-file commit this lane could not make.

* Evidence: `invocations: 0` for both across 15 vectors + a try-with-resources
  probe; `java/util/Scanner.close()V` is registered directly ~80 lines above
  with the same callback and is what every real Scanner receiver reaches;
  nothing mints a `java/io/Closeable` or `java/lang/AutoCloseable`; `dupX = 0`.
* Blocker, named exactly: **`vm/src/vm/tests.rs`, `auto_closeable_close_p70`**
  calls `call_native(.., "java/lang/AutoCloseable", "close", "()V", ..)`, and
  that helper (`vm/src/vm/tests.rs:679`) does
  `.unwrap_or_else(|| panic!("{class}.{method}{descriptor} not registered"))`.
  Deleting the row turns a unit test red for a reason unrelated to anything it
  means to protect. **`vm/` was out of bounds for this lane.**
* The whole change: delete the two `registry.register` calls, delete
  `auto_closeable_close_p70`, one commit. **Do not do it without also reading
  `H11-1` §2.2 item 2** — the `recv_is_bare_object` rescue is the same
  unwitnessed falsifier as §3.2.

**N2 — the six abstract `Pipe$SourceChannel` / `Pipe$SinkChannel` rows need a
two-crate commit or they cannot be retired at all.** `H11-2` §4. `native-io`
owns the slots; `native-builtins/src/phases_late/net_channels.rs` (~2281–2400)
registers the same six triples with an **incompatible field layout**. Deleting
`native-io`'s six hands the slot to that body instead of removing anything. Both
halves are `invocations: 0` and both should go together. `pipe.rs` now carries
the measurement and the warning at the registration site.

**N3 — `H11-1` N1 is the biggest thing this lane opened and did not close.**
The interface rows were never the hazard; the **abstract-class** rows reached
through the superclass walk are. `java/io/InputStream` (9 rows, 8 shadowing
concrete bytecode, `Bridge`, live under `--jdk-only`), `java/io/OutputStream`
(4) and `java/nio/channels/spi/SelectorProvider` (4, an SPI) are the three
worth probing first. One user subclass per class, diffed against HotSpot. The
mechanism is `invoke.rs`:3664–3715: the walk runs only when the receiver's own
class declares neither the method nor a registration, which is exactly the shape
of a small application subclass.

**N3b — `H11-2` N1's OPEN list is 5 classes / 15 rows and two of them cannot
move anywhere.** `sun/nio/ch/SelectorProviderImpl` and
`sun/nio/ch/NativeDispatcher` are already `sun.nio.ch` classes that happen to be
abstract; there is no lower layer. That leaves `MappedByteBuffer` (3),
`java/nio/file/FileSystem` (1) and `spi/FileSystemProvider` (1) as the entire
genuinely-movable population of this crate, pending `H11-2` N5's replacement of
the heuristic that produced the column.

**N4 — `docs/jdk-only-runtime-services.md`'s P1 *NIO, files, networking* row is
now disproved in three separate ways and should be rewritten rather than
re-planned against.** `H5-1` §6.2/§6.3 disproved its prescription and its
example; `H11-1` §3 disproved the dispatch model it assumes; `H11-2` §0 replaces
its "~200 registrations in the wrong place" with a per-registration table whose
movable column is **at most 18 rows and possibly zero**. I did not touch that
file or `INDEX.md` — out of bounds for this lane — so this is the fourth record
in a row asking someone with write access to that row to act on it.

**N5 — a `resolved_via` field in `--dump-native-registry`.** `H11-1` N4. Every
question this lane answered needed a bespoke Java probe because the dump says
which slot was invoked but not what NAME the lookup used. `receiver` /
`cp-class` / `superclass-walk:<ancestor>` on each invoked row would turn N3's
whole investigation into one census, and would have made `H5-1` N1 a grep
instead of a lane.
