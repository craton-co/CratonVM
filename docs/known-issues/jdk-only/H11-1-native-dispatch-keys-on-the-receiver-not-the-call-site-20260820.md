# H11-1 — native dispatch keys on the RECEIVER's class, and the one fallback walk never visits an interface

**Status: MEASURED.** Source and run agree, and they were taken independently:
the source read (§2) was written before the probe (§3) was compiled, and §3's
falsifier list was fixed before the binary was invoked. Nothing in §2–§4 is a
prediction.

**Provenance.** Every number in §3–§5 comes from running the **prebuilt** binary
`C:/craton/target-jdkonly-h2/release/cratonvm.exe`, built at commit
`fe59bf9d9`. **That binary does not contain this lane's edits** (H11-3), and no
claim here is evidence about them. This lane ran no `cargo` command of any kind.
Oracle: HotSpot **25.0.3+9**, resolved rather than copied —
`JDK="$(dirname "$(dirname "$(command -v javap)")")"` gives
`/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot`, **not** the `Eclipse
Adoptium` path 66 records in this directory name (H5-1 §6.1).

**Answers** `H5-1` N1 and `HANDOFF-20260820` §7 item 6, both of which call this
"the highest-value probe … one probe settles ~200 abstract-class registrations".

Lane H11, 2026-08-20.

> **Base and merge.** This worktree was cut at **`26e4b5db4`** while
> `claude/jdk-only-mode-handoff-09b48c` was at **`fe59bf9d9`**;
> `git merge --ff-only` before any edit. The gap is **58 commits, 63 files,
> +11499/−801**. What in it touches this lane, named rather than waved at:
> `native-io/` moved in three files (`lib.rs` +272, `process.rs` +33,
> `random_access_file.rs` +64) carrying **`H8-A`** (`f83c24f68`, the dead
> `UnixDispatcher.close0` registration deleted — `H5-1` N3 closed),
> **`H8-B`** (`b2dbf77e9`, the synthetic-RAF gate mirrored onto
> `getFilePointer` — `H5-1` N4 closed) and **`H8-C`** (`e9f08d42b`,
> `native_scanner_close` now YIELDS instead of swallowing — `H5-1` N2 closed).
> `vm/src/runtime/interpreter/native_override.rs` moved 19/18 lines.
> **Three of H5-1's seven nominations were already closed in the gap**, and
> §6.4 records that I nearly wrote up one of them as live. `[wt base=session start]`.

---

## 0. The answer, in one line

**The receiver.** For every `invokevirtual` and `invokeinterface` with a live
object receiver, the class name handed to the native registry is the
**receiver object's runtime class**, not the constant-pool class at the call
site. The constant-pool class is used only where there is no object receiver
(`invokestatic`, `invokespecial`, a `null` receiver, an array-typed call site)
or in three named rescue branches (§2.2).

| Question | Answer | Evidence |
|---|---|---|
| Which class name reaches `resolve_step1_native`? | the receiver's | §2.1 ARGUED, §3 MEASURED |
| Can a registration on an abstract SUPERCLASS still serve a subclass receiver? | **yes**, via the superclass walk, but only when the receiver's own class declares neither the method nor a registration | §2.3 ARGUED, §3 case C MEASURED |
| Can a registration on an INTERFACE ever serve a class receiver? | **no** | §2.3 + §2.4 ARGUED, §3 cases A/D + §4 MEASURED |
| Does the answer differ between `--jdk-only` and `--real-jdk`? | **no** | §3.4 MEASURED, both modes |

---

## 1. Why this question was worth a lane

`H5-1` §3.3 and §3.5 both end in "I cannot decide this from source". The P1
*NIO, files, networking* row of `docs/jdk-only-runtime-services.md` prescribes
moving ~200 registrations off the abstract public API onto `sun.nio.ch.*Impl`.
Whether that is right, wrong, or a no-op turns entirely on this one fact:

* **If dispatch keyed on the CP class**, a registration on
  `java/nio/channels/Pipe$SourceChannel` would fire whenever an application
  wrote `Pipe.SourceChannel src = pipe.source(); src.read(buf);` — the abstract
  rows would be live and the `Impl` rows partly redundant, and `java/io/
  Closeable.close()V` would intercept every try-with-resources in the JDK.
* **If dispatch keyed on the receiver**, the abstract rows are reachable only
  for receivers the VM itself mints under those names, and interface rows are
  reachable for essentially nothing.

The second is the case.

---

## 2. ARGUED — the source path, read before anything was run

Read-only; this lane does not own `vm/`.

### 2.1 The class name that reaches the registry

`vm/src/runtime/interpreter/invoke.rs`, `execute_invoke_kind`:

```text
:1053  let invoke_class: Arc<str> = if is_special { … }
       else if let Some((_, declaring_name)) = &private_virtual_target { … }
       else if method_class_name.starts_with('[') { … }
       else { match &args[0] {
           Value::Object(Some(obj_ref)) => { … let cid = heap.class_id_of(*obj_ref); …
:1627        recv_name_opt.unwrap_or(method_class_name)          <-- the receiver's name
           }
           … } }
:2379  try_stackless_invoke(shared, thread, frame_idx, &invoke_class, …)
```

and `try_stackless_invoke` passes that same `class_name` straight into
`resolve_step1_native` (`invoke.rs:3592`), which does
`registry.resolve_id(class_name, method_name, descriptor)`
(`native_override.rs:7384`).

`method_class_name` — the constant-pool class — survives into `invoke_class`
**only** through the four `else if` arms and the rescue branch below it. The
default for an object receiver is `recv_name_opt`.

The cached fast path agrees rather than diverging, which matters because a
divergence between the two would be a much larger finding:
`dispatch_virtual.rs`'s `execute_invokevirtual_vtable_fast` resolves
`rcv_name` from `receiver_class_id` and probes
`native_methods.find(rcv_name, &method_name, &method_descriptor)`
(`dispatch_virtual.rs` ~:401), and `populate_virtual_invoke_cache` is keyed on
the receiver's `class_id`.

### 2.2 The three places the CP class DOES win

Named because each is a real, if narrow, population, and because a future
change to any of them re-opens this question:

1. **`recv_is_iface`** — the receiver's own class is an interface. `invoke.rs`
   :1617–1626 substitutes `method_class_name` when the receiver is an interface
   (or a bare `java/lang/Object`), the CP class is not `Object`, the member is
   not an `Object` member, and the two names differ. A receiver whose class name
   IS the interface the CP names falls through the last condition and keeps the
   receiver name — same answer either way.
2. **`recv_is_bare_object`** — the receiver's runtime class is literally
   `java/lang/Object`, "a synthetic native return that landed without subclass
   info" in the code's own words. This is the one route by which an interface
   registration could still be reached from ordinary bytecode.
3. **Stale/zero header** — `invoke.rs` :1152 onwards, the all-zero-header
   detector, falls back to the CP class deliberately.

### 2.3 The fallback walk is superclass-only

`invoke.rs`, the `.or_else` after step 1:

```text
:3675   let mut cid = start_cid(&cm)?;
:3676   loop {
:3677       let parent_id = cm.get_class(cid)?.superclass?;
            …
:3705       if let Some(cb) = native_methods.find(&parent.name, method_name, descriptor) { return Some(cb) }
:3713       if has_bytecode { return None; }
:3715       cid = parent_id;
        }
```

`superclass`, and nothing else. **No interface is ever visited.** And for a
virtual call (`walk_native_hierarchy == false`, which is what
`execute_invoke_kind` passes at :2379) the loop is not even entered if the
receiver's class declares the method itself (:3664–3673).

So a registration on an abstract **class** is reachable from a subclass
receiver; a registration on an **interface** is not reachable from any class
receiver at all.

### 2.4 A second, independent barrier — found by H8, not by me

`H8-1` reached the same place from a different direction and I did not find
this until after my run, so it is recorded as corroboration and not as my
result. At **step 6** of `execute_invoke_kind` (`invoke.rs`:4189) and at
`vm_exec.rs`:27907's `override_cb` arm, a native whose **resolved declaring
class** is an interface instance method is dropped outright unless the triple
appears in `should_force_registered_native_over_bytecode`. That is a different
step and a different key from §2.1, and it points the same way.

Two barriers, two steps, two reasons. §2.3 explains why interface rows are
never *found*; §2.4 explains why they would be *discarded* even if they were.

---

## 3. MEASURED — the probe, and what would have falsified it

`H11Dispatch.java`, written to the session scratchpad, compiled with the
oracle's `javac`, run first on HotSpot (control: is the probe well-formed?) and
then on the prebuilt CratonVM. Each case is a call site whose **CP class carries
a registration in `native-io`** and whose **receiver's class does not**, or
carries different behaviour.

The emitted call sites were verified with `javap -c`, not assumed:

```text
17: invokeinterface #24    // InterfaceMethod java/lang/AutoCloseable.close:()V
15: invokevirtual  #83     // Method java/io/File.length:()J
12: invokeinterface #100   // InterfaceMethod java/io/DataInput.readInt:()I
```

| Case | Call site | Receiver | RECEIVER-KEYED predicts | CP-KEYED predicts |
|---|---|---|---|---|
| **A** | `try (AutoCloseable ac = new Res())` → `invokeinterface java/lang/AutoCloseable.close:()V`; `java/lang/AutoCloseable.close()V` is registered (`lib.rs`, `native_scanner_close`) | user class with a printing `close()` | `A-CLOSE-RAN` printed | not printed (the native's guard handles it) |
| **B** | `File f = new MyFile(p); f.length()` → `invokevirtual java/io/File.length:()J`; `java/io/File.length()J` is registered (`lib.rs:6350`) | `MyFile extends File`, overriding `length()` to return `424242`; the file on disk is **7** bytes | `B=424242` | `B=7` |
| **C** | `PlainFile p = …; p.length()`, CP names the SUBclass, registration is on the superclass | subclass declaring nothing | `C=7` (walk reaches the ancestor row) | `C=7` — **not discriminating**, kept only to show the walk does not break |
| **D** | `DataInput di = new MyDataInput(); di.readInt()` → `invokeinterface java/io/DataInput.readInt:()I`; that triple was registered (`lib.rs`, `native_dis_read_int`) | user class implementing `DataInput` directly; it is not a `DataInputStream` and has none of its fields | `D=9999` | the native's answer, or a crash |

Every `read*` on `DataInputStream` is `ACC_FINAL` on JDK 25, which is why case B
is on `java/io/File` and not, as first drafted, on `DataInputStream.readInt`.
The first compile failed with *"overridden method is final"*; recorded because
the natural choice of method for this probe cannot be overridden.

### 3.1 Result

```console
$ java -cp . H11Dispatch                     # HotSpot 25.0.3+9 — control
A-BODY / A-CLOSE-RAN / B=424242 / C=7 / D=9999
$ cratonvm.exe --jdk-only -cp . H11Dispatch  # prebuilt fe59bf9d9
A-BODY / A-CLOSE-RAN / B=424242 / C=7 / D=9999
```

**Byte-identical to the oracle on all four.** Every one is the RECEIVER-KEYED
column. Three independent registrations — an interface `close`, a concrete
class's overridable `length`, an interface `readInt` — all lost to the
receiver's own body.

### 3.2 The falsifiers, stated before the run

* `A-CLOSE-RAN` absent → CP-keyed, and `H5-1` §3.5's alarm about every
  try-with-resources is real.
* `B=7` → CP-keyed for `invokevirtual`.
* `D` anything but `9999` → CP-keyed for `invokeinterface`.
* Any of the three differing from HotSpot **in either direction** → the probe is
  measuring something other than dispatch keying, and §2's read is unconfirmed.

None fired.

### 3.3 The instrument that does not depend on printed values

`--dump-native-registry` (schema 5) reports `invocations` per slot. On a run
that opened a real `Pipe`, wrote 4 bytes and read them back through
**`Pipe.SinkChannel`- and `Pipe.SourceChannel`-TYPED locals** — so the CP class
at every call site is the abstract nested class:

| Slot | `registered_by` | `invocations` |
|---|---|---:|
| `sun/nio/ch/SourceChannelImpl.read(Ljava/nio/ByteBuffer;)I` | `native-io/src/pipe.rs:1410` | **1** |
| `sun/nio/ch/SinkChannelImpl.write(Ljava/nio/ByteBuffer;)I` | `native-io/src/pipe.rs:1439` | **1** |
| `sun/nio/ch/SourceChannelImpl.close()V` | `pipe.rs:1417` | **1** |
| `sun/nio/ch/SinkChannelImpl.close()V` | `pipe.rs:1441` | **1** |
| `java/nio/channels/Pipe$SourceChannel.read(Ljava/nio/ByteBuffer;)I` | `pipe.rs:1428` | **0** |
| `java/nio/channels/Pipe$SinkChannel.write(Ljava/nio/ByteBuffer;)I` | `pipe.rs:1450` | **0** |
| `java/nio/channels/Pipe$SourceChannel.close()V` | `pipe.rs:1435` | **0** |
| `java/nio/channels/Pipe$SinkChannel.close()V` | `pipe.rs:1457` | **0** |

**This is `H5-1` N1's exact probe, and it answers in the negative.** The
constant-pool class took zero calls; the receiver's class took all four.
`pipe.source().getClass().getName()` reports `sun.nio.ch.SourceChannelImpl` on
CratonVM, the same string HotSpot gives.

### 3.4 Mode-independent

The same probe under `--real-jdk` (compatible) prints the same five lines. The
keying is a property of `execute_invoke_kind`, not of `dispatch_policy`, so
nothing here is contingent on strict mode.

---

## 4. MEASURED — the reachability census, 15 corpus vectors

One probe is one probe. To ask "does anything in the corpus reach an interface
registration", each vector below was compiled to the session scratchpad (NOT to
`regression-suite/build`, which another lane may be using) and run under the
prebuilt binary with `--dump-native-registry`, and the per-slot invocations were
unioned. All fifteen printed their `PASS <Class>` line on this binary, which is
also a clean pre-change baseline for H11-3.

Vectors: `RDataInputFastPull RJdkNio RJdkAsyncChannel RJdkWatchService
RJdkStrict RSerial RNioNoFollow RChannelInterrupt RSocketChannelInterrupt
RFileTimes RJdkNet RStrings RExceptions RJdkIntrinsics3 RChmKeySetView`.

| Triple | union `invocations` |
|---|---:|
| `java/io/Closeable.close()V` | **0** |
| `java/lang/AutoCloseable.close()V` | **0** |
| `java/io/DataInput.readInt()I` | **0** |
| `java/io/DataInput.readLong()J` | **0** |
| `java/io/DataOutput.writeInt(I)V` | **0** |
| `java/io/DataOutput.writeLong(J)V` | **0** |
| `java/nio/channels/Pipe$*.{read,write,close,isOpen}` (6) | **0** |
| `java/util/Scanner.close()V` | **0** |
| `java/io/DataInputStream.readInt()I` | **540** |
| `java/io/DataOutputStream.writeInt(I)V` | **53** |

The last two rows are what make the zeros mean something. `[zero@consumer]`
warns that a consumer's zero cannot explain itself — here the *producer* is
visible in the same table: the calls exist, in quantity, and they all went to
the **concrete class** rows. This is not "the corpus does not exercise
`DataInput`"; it is "the corpus exercises it 540 times and the interface row
never sees one".

---

## 5. What this settles, and what it does NOT

### 5.1 Settled

* **`H5-1` N1: answered.** Receiver-keyed.
* **The P1 row's prescription is wrong for every family the VM mints** — moving
  a registration to the `*Impl` strands the fabricated receiver. `H11-2` counts
  them.
* **The P1 row's `Pipe` example is wrong twice.** `H5-1` §6.3 already showed
  `Pipe` registers on both; this record adds that the abstract half takes zero
  calls, so the family was never an instance of the problem the row names.
* **An interface registration cannot intercept a user implementor.** Three
  in-tree comments implied it could (`register_data_stream_natives`'s "this shim
  decides dispatch for every USER implementor", `pipe.rs`'s "Java-side dispatch
  lands here either way", `native_scanner_close`'s "Interface natives only serve
  receivers whose resolved declaring class IS the interface" — that third one is
  *right*, and stronger than its own hedging admits). All three are corrected in
  H11-3's commits.

### 5.2 NOT settled — say this plainly

* **This does not make abstract-class registrations safe.** The superclass walk
  is real (§2.3): an application subclass of an abstract class carrying
  registrations, declaring none of the methods itself, **is** intercepted. That
  is the live hazard the interface rows never were, and `java/io/InputStream`
  (9 rows, 8 of them shadowing concrete bytecode) is its largest instance in
  this crate. Not probed.
* **The `recv_is_bare_object` rescue (§2.2 item 2) is not measured.** I could
  not construct a Java-side witness for it; it needs a VM-internal synthetic
  return landing as bare `Object`. Every deletion in H11-3 names it as the
  falsifier for exactly this reason.
* **JIT-compiled call sites are not covered.** Everything here is interpreter
  dispatch. `H4-1` O1 records six direct helpers in `vm/src/jit/helpers.rs` that
  reimplement natives, and `[door?]` says a tier-dependent answer is possible
  that no arm diffs for. Nothing in this record is evidence about a compiled
  frame.
* **One boot's registry is a reachability FLOOR, not a registration census.**
  §4's dump reports 925 `native-io` rows in a `--jdk-only` boot; `H5-1`'s static
  parse counts 1493 registrations in the source. The gap is registrars this boot
  did not reach (native-io's typed-buffer loop over `java/nio/{Char,Int,Long,…}
  Buffer` appears in **neither** mode's dump) plus `SyntheticStub` rows strict
  mode refuses. **The two numbers are floors of different things and must not be
  subtracted** — same warning `H5-1` §6.4 and `H1-1` give.

---

## 6. Where the tree and the records are WRONG about this

**6.1** `native-io/src/lib.rs`, `register_data_stream_natives`' header:
*"4 land on the `DataInput`/`DataOutput` INTERFACES (so this shim decides
dispatch for every USER implementor — worth removing on its own, once it can
be)"*. **False**, §3 case D. It decided dispatch for no user implementor, and
the parenthetical is why the four rows survived three census passes looking like
a blocker.

**6.2** `native-io/src/pipe.rs`: *"Abstract SourceChannel — same
implementations, different declared class so Java-side dispatch lands here
either way."* **False**, §3.3. Dispatch lands on the `Impl` every time.

**6.3** `H5-1` §3.5 — *"every try-with-resources on an `AutoCloseable`-typed
variable compiles to `invokeinterface java/lang/AutoCloseable.close:()V`"* — is
correct about javac and wrong about the consequence. The bytecode is exactly as
described (verified with `javap -c`); the native is never reached.

**6.4 A correction to my own reading, and the trap that produced it.** My first
draft of H11-3's comment described `native_scanner_close`'s decline as a live
silent-swallow. It is not: `e9f08d42b` (`H8-C`) replaced the `Ok(None)` with a
real yield through `invoke_virtual_bytecode_only` — **in the 58-commit gap this
worktree was cut behind**. `H5-1` N2 and `HANDOFF-20260820` §7 item 5 both still
describe the pre-`e9f08d42b` body, and I inherited it from them rather than from
the file. `[verify m]`, and the specific lesson: a record written yesterday
describes yesterday's tree, and in this directory the tree moves several times a
day. Corrected in commit 2 of this lane.

**6.5** `H5-1` N7 names `AsynchronousServerSocketChannel` and
`AsynchronousFileChannel` as "abstract with registrations and NO fabrication
site found … the two families that might genuinely be movable". Both are
fabricated, both inside `native-io` itself. See `H11-2` §2.

---

## 7. Method, so it is reproducible

1. `JDK="$(dirname "$(dirname "$(command -v javap)")")"` — never a path copied
   from a record.
2. Write `H11Dispatch.java` (§3) to a scratch dir. `javac -d <scratch>`.
   `javap -c -p` it and CONFIRM the emitted call sites name the classes you
   think they do; a probe that does not emit the opcode it claims measures
   nothing.
3. Run HotSpot first. If HotSpot does not print the RECEIVER-KEYED column, the
   probe is broken, not the VM.
4. `cratonvm --jdk-only -cp <scratch> H11Dispatch`, **stdout only** — `2>&1`
   puts VM tracing in the diff (`[stdout only]`).
5. `cratonvm --jdk-only --dump-native-registry reg.json -cp <scratch>
   H11Dispatch`, then read `natives[]` for `invocations` on the pairs in §3.3.
   Schema 5 fields used: `class name descriptor kind registered_by invocations
   invocations_complete owns_slot real_declaring_method{loaded, declared,
   acc_native, has_code}`.
6. For §4, loop the vectors, one dump per vector, union the counters. Compile to
   your OWN scratch dir — `regression-suite/build` is shared and another lane
   may be mid-run (`[test locks]`).
7. Check `slots_with_incomplete_invocations` (27 on these runs) and per-row
   `invocations_complete` before quoting any count. Every row quoted here is
   `invocations_complete: true`.

Scripts (~40 lines each) are in the session scratchpad and are faster to
re-create than to hunt for.

---

## 8. OUT-OF-FILE EDITS REQUIRED

**None.** This record asserts nothing that requires a change outside
`native-io/src/**` and `docs/known-issues/jdk-only/H11-*.md`. The one change it
*implies* outside those paths is nominated, not made — `H11-3` N1.

---

## 9. NOMINATIONS

**N1 — probe the superclass walk, which is the hazard the interface rows never
were.** §5.2. `java/io/InputStream` carries 9 `native-io` rows, 8 of them over
concrete bytecode, tagged `Bridge` and therefore live under `--jdk-only`. Any
application `InputStream` subclass that does not override, say, `readAllBytes()`
gets this crate's body instead of the JDK's. Probe: a user `InputStream`
subclass implementing only `read()`, then `readAllBytes()` /
`readNBytes(int)` / `skip(long)`, diffed against HotSpot. Same shape for
`java/io/OutputStream` (4 rows) and `java/lang/Process` (13). This is the
population `[G88-1]` warns about and it is now the only abstract-registration
hazard this record leaves open.

**N2 — construct a witness for the `recv_is_bare_object` rescue, or delete the
branch.** `invoke.rs`:1617's substitution of the CP class for a bare-`Object`
receiver is the sole remaining route to an interface registration, it is the
named falsifier for two deletions in `H11-3`, and I could not reach it from
Java. Either a VM-side test can construct one — a synthetic native return
stamped `java/lang/Object` fed to an interface-typed call site — or the branch
is dead and its removal would make the whole interface-registration population
provably unreachable in one edit.

**N3 — the same probe for a COMPILED frame.** §5.2. Everything here is
interpreter dispatch. `vm/src/jit/helpers.rs`'s direct helpers bypass the
registry; if any of them keys on the CP class the answer diverges by tier and no
current arm diffs for it. Probe: the §3 case-B shape in a loop hot enough to
tier up, with `--nojit` as the A arm (`[jit ladder]`).

**N4 — `--dump-native-registry` should report the KEY, not only the slot.**
Every question in this record took a bespoke probe because the dump says which
slot was invoked but not what name the lookup used. A `resolved_via` field —
`receiver` / `cp-class` / `superclass-walk:<ancestor>` — would have made §3.3
a one-line grep and makes the whole class of question answerable by census
instead of by construction.
