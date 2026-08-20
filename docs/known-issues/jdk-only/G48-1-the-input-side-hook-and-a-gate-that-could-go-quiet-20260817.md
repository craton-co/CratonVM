# G48-1 — the input-side hook, and a gate that could go quiet

**Status:** the registry audit is **MEASURED and its verdict changed a
documented premise**; the hook is **built, unit-tested and PROVABLY INERT**,
and the four `drain.conn.*` rows it exists to close are **NOT closed by this
lane** — the consumer half is `native-builtins/src/http_url_connection.rs`,
which is not this lane's file (N2).

**Provenance:** `C:/craton/target-rel3/release/cratonvm.exe` (`9ae371468`),
2026-08-17, JDK `C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`,
vectors from `C:/craton/cvm-mergecheck/regression-suite/build` (read-only).
`target-rel3` **predates this lane's edits and cannot contain them** — no lane
here may run `cargo`. Every number below is therefore a BEFORE measurement, and
every "after" is labelled.

**Owned files:** `native-io/src/lib.rs` and `native-api/src/registry.rs`.
Everything else is a NOMINATION in §5.

---

## 0. The headline

| | |
|---|---|
| **Does the hook add a registration?** | **No — PROVED by dump, §1.** `bridge-ratchet.sh` and `scripts/baselines/` do not move. |
| `RSslLiveSession` | **95 checks, 14 failing — before and after.** The hook is inert until N2 lands; §2 says why that is a property and not an excuse. |
| **Is a `SyntheticStub` reachable through a bypass?** | **YES in `compatible` mode — four of them. NO under `--jdk-only`, and NOT for the reason the record on file gives.** §3. |
| G33-1's "neither bypass family currently serves a `SyntheticStub`" | **EXPIRED.** A third family landed since and four of its fourteen triples are stubs. |
| What the gate now has | `incomplete_slots_of_kind`, `invocations_of_kind_checked`, `KindInvocations::is_clean` — it can refuse instead of passing quietly. §3.4 |

---

## 1. The hook adds no registration — MEASURED before it was built

The brief required this be proved with `--dump-native-registry` *before*
building, because it is the single constraint that killed every alternative
G44-1 §3 considered. `--dump-native-registry` under `--jdk-only` on
`9ae371468`, this lane's own run:

```text
java/io/ByteArrayInputStream  read  ()I     owns_slot=false  native-builtins/src/lib.rs:18554
java/io/ByteArrayInputStream  read  ()I     owns_slot=TRUE   native-io/src/lib.rs:6861   inv=7
java/io/ByteArrayInputStream  read  ([BII)I owns_slot=false  native-builtins/src/lib.rs:18589
java/io/ByteArrayInputStream  read  ([BII)I owns_slot=TRUE   native-io/src/lib.rs:6863   inv=1
java/io/ByteArrayInputStream  read  ([B)I   owns_slot=TRUE   native-io/src/lib.rs:6862
java/io/ByteArrayInputStream  close ()V     owns_slot=false  native-builtins/src/lib.rs:18700
java/io/ByteArrayInputStream  close ()V     owns_slot=TRUE   native-io/src/lib.rs:6867   inv=1
```

All three read shapes and `close` are **already registered** and **already
owned by `native-io`**. `owns_slot` is the trustworthy column; the `inv`
figures are a bonus and they are from *this vector's own run*, so the bodies
the hook sits in are not merely registered but demonstrably executing during
`RSslLiveSession`. A hook placed inside an existing body registers nothing:
`register()` is not called, `generation()` does not move, no row appears or
changes kind, and the ratchet has nothing to see.

**Two dispatch sites cover all three read descriptors.** `read([B)I`
(`native_bais_read_byte_array`) does not implement EOF itself — it delegates to
the three-arg form by *virtual dispatch*, deliberately (a long comment there
records the Jetty `StackOverflowError` that a direct Rust call caused). So
`native_bais_read`'s `pos >= count` arm and `native_bais_read_bytes`' cover
everything, and a third site would be dead.

## 1a. What was built

`native-api/src/registry.rs` — the mirror of the existing `BaosEvent` block,
placed beside it:

```rust
pub enum BaisEvent { Eof, Close }
pub type BaisEventHook =
    fn(&mut dyn NativeContext, ObjectRef, BaisEvent) -> Result<(), MethodCallFailed>;
pub fn install_bais_event_hook(hook: BaisEventHook);
pub fn dispatch_bais_event(..) -> Result<(), MethodCallFailed>;
```

`native-io/src/lib.rs` — three bodies, all pre-existing:

| site | what changed |
|---|---|
| `native_bais_read`, `if pos >= count` | dispatch `Eof`, then return `-1` exactly as before |
| `native_bais_read_bytes`, `if pos >= count` | dispatch `Eof`, then return `-1`; **below** the EOF test, never above it |
| `native_bais_close` | dispatch `Close`; still `Ok(None)`, still a no-op for the stream |

**The one place this is NOT a mirror image, and it is deliberate.**
`BaosEventHook` returns `Result<bool, _>` where `true` means "consumed, skip
the ordinary path" — a real choice, because a BAOS observer legitimately takes
over the write and sends the bytes to a native sink. There is no such choice on
the input side: `ByteArrayInputStream.read()` **must** return `-1` at
`pos >= count` and `close()` **must** be a no-op, whatever any observer thinks.
A `bool` here would be a return value every caller is obliged to ignore — the
kind of parameter that is eventually honoured by someone who reads the type and
not the doc, silently turning an EOF into a non-EOF. The type says what is
true: observe, do not decide. `Err` still propagates for genuine internal
failure, and the doc states that an observer must not raise a Java exception
from there.

**`Eof` fires on every exhausted read, not only on the transition**, because
the transition is not observable from inside the read body: a stream
constructed empty is at `pos >= count` on its *first* read, and firing "once"
would make the empty-body case silent. Observers must be idempotent — which
they must be anyway, since a drained-then-closed stream produces `Eof` *and*
`Close`. Both facts are on `BaisEvent`'s doc and both are asserted.

`native-io` reaches the new items through `cratonvm_native_api::registry::…`
rather than the flat crate-root path the BAOS four use, because
`native-api/src/lib.rs` is **not this lane's file**. `pub mod registry;` makes
that path work today with no edit; the one-line re-export is **N1**.

---

## 2. The four `drain.conn.*` rows are NOT closed, and the hook is inert

MEASURED, `9ae371468`, `--jdk-only`:

```text
CK RSslLiveSession drain.body                      = ok
CK RSslLiveSession drain.conn.cipherSuite.raises   = none  WANT java.lang.IllegalStateException
CK RSslLiveSession drain.conn.cipherSuite.message  = none  WANT connection not yet open
CK RSslLiveSession drain.conn.sslSession.raises    = none  WANT java.lang.IllegalStateException
CK RSslLiveSession drain.conn.sslSession.message   = none  WANT connection not yet open
CK RSslLiveSession drain.session.isValid           = true
CK RSslLiveSession drain.session.getId.length      = 32
CK RSslLiveSession fails=14
CK RSslLiveSession checks=95
```

`drainTrap()` is `while ((b = in.read()) != -1) body.write(b); in.close();` —
so the single-byte EOF arm and `close()` are exactly the two events this lane
added, and they fire in that order. **What is missing is the consumer**: the
observer that maps the stream object back to its `HttpsURLConnection` carrier
and calls `https_recycle_carrier`. That lives in `http_url_connection.rs`,
alongside the `huc_live_baos_event` consumer it mirrors, and it is not this
lane's file. **N2.**

So the honest statement is: **14 before, 14 after, and the after is not a
prediction — it is a consequence.** With no hook installed,
`dispatch_bais_event` is one `OnceLock::get()` returning `None`, and every
value the three touched bodies produce is bit-identical to HEAD's. That is
asserted, not asserted-about: `an_unarmed_thread_sees_the_pre_hook_behaviour_exactly`
drains a stream, closes it, and closes a null receiver, with no observer armed.
`native-builtins` depends on `native-io` and everything depends on
`native-api`, so "inert until N2" is the property that matters most here.

**Not taken, again:** recycling at `getInputStream()`. It would turn these four
green today with no `native-io` change at all. HotSpot's `KeepAliveStream`
returns the connection at EOF, not at hand-out, so the accessors answer for the
whole window in between; four green rows bought with a new divergence is what
this directory exists to refuse. G44-1 §2 rejected it and this lane concurs
from the other side of the boundary.

---

## 3. The gate audit — and the premise that expired

### 3.1 What the four bypass families actually are

`mark_invocations_incomplete` had **no callers** when G33-1 was written ("None
of them calls this yet"). It has four now, in three families:

| family | site | slots it can mark |
|---|---|---|
| interpreter intrinsic cache | `dispatch_static.rs:1050`, `:881` (`_by_triple`) | only triples in `native_builtins::intrinsics::lookup` — **bounded by that table**, not open over the registry |
| JIT direct-call helpers | `jit/helpers.rs`, `DIRECT_CALL_HELPER_NATIVES` | a fixed list of **8** |
| stackless exotic invoke | `interpreter/invoke.rs`, `UNCOUNTED_STACKLESS_NATIVES` | a fixed list of **14** |

The third family **did not exist when G33-1 was written.** That is where the
premise fails.

### 3.2 The measurement

Every triple in the two fixed lists, resolved against the census of
`9ae371468` in default (`compatible`) mode:

```text
DIRECT_CALL_HELPER_NATIVES (8)   — intrinsic ×3, bridge ×5, stub ×0
UNCOUNTED_STACKLESS_NATIVES (14) — bridge ×10, SYNTHETIC-STUB ×4:

  synthetic-stub  java/lang/foreign/DowncallHandle  type         ()Ljava/lang/invoke/MethodType;
  synthetic-stub  java/lang/foreign/DowncallHandle  invoke       ([Ljava/lang/Object;)Ljava/lang/Object;
  synthetic-stub  java/lang/foreign/DowncallHandle  invokeExact  ([Ljava/lang/Object;)Ljava/lang/Object;
  synthetic-stub  java/lang/foreign/DowncallHandle  invokeBasic  ([Ljava/lang/Object;)Ljava/lang/Object;

  owns_slot=true, registered_by native-builtins/src/phases_late/foreign_ffm.rs:4247…4265
```

**So the answer to "can a `SyntheticStub` now be reached through a marked
slot" is YES.** Four can, in `compatible` mode, through the family whose own
comment says it "memoize[s] the callback per registry generation and return[s]
`Handled` directly". Their census `invocations` reads **0** and will keep
reading 0 however many Panama downcalls run through them.

The reflection arm the brief flagged (`Method.invoke`,
`Constructor.newInstance`) is `bridge` on both rows — it under-reports itself,
not a stub, and a stub invoked *downstream* of a reflective call still goes
through a counted dispatch. Reflection is not the hole. Panama is.

### 3.3 Why the CI gate is nonetheless not lying today — and why that is worse

Census header, same binary, the two modes side by side:

```text
--jdk-only : intrinsic 645, bridge 10046, synthetic-stub    0, total 10691
             invocations: intrinsic 1270, bridge 8932, synthetic-stub 0
compatible : intrinsic 645, bridge 10073, synthetic-stub 1321, total 12039
             invocations: intrinsic  337, bridge 1630, synthetic-stub 199
```

Under `--jdk-only` there are **zero `SyntheticStub` slots at all**, so
`invocations_of_kind(SyntheticStub)` is structurally zero and no bypass can
change it. SOURCE-VERIFIED as to mechanism: `register_inner`'s first arm
refuses a `SyntheticStub` outright under `JdkOnly` and returns *without
inserting* — "nothing is pushed to `registrations` / `categories` /
`provenance` / `slots`". The retired-shadow re-tag runs **before** it, by
design, so it cannot smuggle one past either.

**That means the gate's zero has never been a measurement.** It is a
restatement of `counts.synthetic-stub == 0` — a fact about the *registration
refusal*, arriving by a route (`record_invocation` counters) that cannot
support it. G33-1's stated reason for the gate being safe was wrong even while
its conclusion was right, and it is now wrong in both halves.

The residual hole, narrow but real: `set_compatibility_mode` is a plain field
setter. A VM that registers stubs and *then* switches to `JdkOnly` keeps every
stub slot it already made, and the door §3.2 measured is standing open behind
it. **N6.**

### 3.4 What was done about it

`invocations_of_kind`'s signature is **unchanged** — `native-builtins`,
`vm_init.rs`, `jfr`, `difftest` and `scripts/jdk-only-bench.sh` all read it,
and a lane forbidden to build must not change a type five crates consume. Its
**doc** now carries §3.2's measurement in place of the expired claim, and three
items were added beside it:

```rust
pub fn incomplete_slots_of_kind(&self, kind: NativeKind) -> usize;
pub fn invocations_of_kind_checked(&self, kind: NativeKind) -> KindInvocations;

pub struct KindInvocations { pub kind: NativeKind, pub total: u64, pub incomplete_slots: usize }
impl KindInvocations {
    pub fn is_conclusive_zero(self) -> bool;  // total == 0 && incomplete_slots == 0
    pub fn is_measurable(self)      -> bool;  // incomplete_slots == 0
    pub fn is_clean(self)           -> bool;  // the gate predicate
    pub fn describe(self)           -> String;
}
```

A gate that reads `is_clean()` **cannot go quiet**. The state in §3.2 —
`total == 0`, `incomplete_slots == 4` — fails it, and `describe()` returns
`"…0 counted dispatches, but 4 slot(s) of this kind are declared incomplete —
this zero proves nothing…"` rather than a bare assertion failure that sends the
reader hunting for a stub that may not have run. `is_measurable` is kept
separate so a caller reporting a *non-zero* total can still say whether it is
the whole number. The gate call sites are **N4** — none of them is this lane's.

`is_clean` is a synonym of `is_conclusive_zero` on purpose: one is a statement
about the measurement, the other the verdict drawn from it, and a gate's source
reads better when it names which it means.

---

## 4. Verification

**`rustfmt --edition 2021 --check`, in place, against `git show HEAD:` of each
file:**

| file | hunks at HEAD | hunks now |
|---|---:|---:|
| `native-api/src/registry.rs` | 6 | **6** |
| `native-io/src/lib.rs` | 35 | **35** |

**Zero new hunks.** Four hunks this lane did introduce were folded back by hand
(the `BaisEventHook` alias and three `assert_eq!` calls). `native-io/src/lib.rs`
had to be baselined against a full copy of `native-io/src` with HEAD's `lib.rs`
substituted, because `rustfmt` follows `mod` declarations and a standalone copy
reports 0 by failing to resolve them. Zero CR bytes in either file.

**Neither file was compiled** — no lane here may run `cargo`. Instead the logic
of both changes was transcribed **verbatim** into a standalone harness outside
the crate tree (`scratchpad/g48.rs`; `rustc --edition 2021 --test`, nothing
written under `target/`) over reduced mocks: **8 tests, 8 passing.**

* the four gate tests, including
  `the_measured_registry_reproduces_the_two_verdicts` — §3.2's fourteen
  stackless triples with the kinds the dump reported, marked as `invoke.rs`
  marks them, asserting the gate goes **red** in `compatible` and **clean**
  under `--jdk-only`;
* the four hook tests, including
  `the_vectors_drain_loop_produces_exactly_one_eof_then_one_close` — the exact
  call shape `drainTrap()` performs, which is what the N2 consumer will see.

**Eight unit tests were added to the existing `#[cfg(test)]` modules** (four in
each file). The `native-io` four install a **process-wide** observer — the
`OnceLock` is irreversible — so the recorder is armed **per thread**: an
unarmed thread takes a `None` branch and records nothing, which is what keeps
the ~dozen other tests in that module that drain a stream to EOF from being
perturbed. `native-api`'s fourth test asserts the opposite invariant, that no
hook is installed from inside that crate's own test binary, through the public
surface rather than by reading the static.

**Vectors, all MEASURED on `9ae371468` under `--jdk-only`** (the binary cannot
contain this lane's edits; §2 is why "after" is a consequence rather than a
guess). Every denominator matches the brief:

| vector | checks | fails |
|---|---:|---|
| `RSslLiveSession` | **95** | **14** — unchanged, and §2 says why |
| `RSslNullSession` | 89 | PASS |
| `RJdkNio` | 101 | PASS |
| `RFileTimes` | 68 | PASS |
| `RDataInputFastPull` | 22 | PASS |
| `RJdkNet` | 81 | PASS |
| `RCrypto` | 57 | PASS |
| `RJdkAsyncChannel` | 141 | PASS |

---

## 5. NOMINATIONS

**N1 — `native-api/src/lib.rs`: re-export the four new items beside the BAOS
four. One line, zero risk.**
Line 110's `pub use registry::{ dispatch_baos_event, install_baos_event_hook,
…, BaosEvent, BaosEventHook, … }` should gain `dispatch_bais_event`,
`install_bais_event_hook`, `BaisEvent`, `BaisEventHook` and `KindInvocations`.
`native-io` uses `cratonvm_native_api::registry::…` today, which works because
the module is `pub`, but the asymmetry with the BAOS call two functions away is
the kind that gets "tidied" wrongly. Not this lane's file.

**N2 — `native-builtins/src/http_url_connection.rs`: the consumer. THE four
rows.** This is the half that closes `drain.conn.cipherSuite.raises`,
`.message`, `drain.conn.sslSession.raises`, `.message`. **Change:** beside the
existing `install_baos_event_hook(huc_live_baos_event)` in
`register_http_url_connection_real`, add
`install_bais_event_hook(huc_live_bais_event)`; the consumer maps the stream
object to the carrier that produced it (the `https:` body is minted in
`perform`, which holds both) and calls `https_recycle_carrier` — G44-1 §2's
function, already written and already active. **Constraints, each measured:**
(a) it must be **idempotent** — `Eof` fires on every exhausted read and `Close`
follows it, so one drain produces at least two events, and G44-1 §2 records
that a double recycle must not release a global root twice; (b) it must
**flag, not remove**, the peer-info row, or `https_ensure_exchanged` reads
"never handshaked" and re-issues the request over the network; (c)
`drain.session.isValid` and `drain.session.getId.length` are **green today and
must stay green** — that is the row separating "recycled" from "destroyed", and
HotSpot keeps the session object alive after the connection goes back to the
`KeepAliveCache`. This adds no registration (§1).

**N3 — the census dump does not print the completeness column at all.**
`NativeCensusRow::invocations_complete` **exists** in `registry.rs` and the
schema-4 JSON writer in `vm/src/vm/vm_init.rs` never emits it; nor does the
header carry `slots_with_incomplete_invocations()`. MEASURED: this lane parsed
both dumps and the key is absent from all 10,691 / 12,039 rows, which is why
§3.2 had to be answered by source analysis against the two fixed lists instead
of by reading the dump. So the instrument `HANDOFF-20260814` §4 recommends for
"which body runs" cannot currently tell its reader that an `invocations` figure
is a floor — the exact caveat G33-1 §4 exists to impose, unavailable at the
point of use. **Change:** one row key and one header key, both already
computed. Not this lane's file.

**N4 — the gate call sites should read `is_clean()`.**
`difftest/src/census.rs` (`assert_eq!(census.synthetic_stub_invocations(), 0)`),
`jfr/src/jdk_only.rs`, `tools/jdk-only-blockers/`, and `vm_init.rs`'s
`synthetic_stub_invocations` key. Depends on N3 for the number to be
transportable through the JSON at all. Until then the honest reading of that
key is "the registration refusal held", not "no stub ran".

**N5 — `vm/src/runtime/interpreter/invoke.rs`: the four `DowncallHandle` arms
are stubs sitting in a bypass list.** Either count them (the list's siblings in
`call_integer_native_raw` do, and that file's own doc says an uncounted
open-coded arm "would leave exactly the kind of unverifiable path the
acceptance criterion is written against"), or adjudicate them to `Bridge` if
Panama downcalls are genuinely bridged. Leaving them as they are is the state
§3.2 measured. **Note the trap:** re-tagging them `Bridge` makes them survive
into `--jdk-only`, where they would then be marked-and-bypassed *and* live —
which is precisely the configuration N4's `is_clean()` is there to catch, so
N4 should land first.

**N6 — `native-api/src/registry.rs`'s own `set_compatibility_mode` is a plain
setter, and this lane did not change it.** Switching to `JdkOnly` after
registrations have run leaves every existing `SyntheticStub` slot in place;
only *subsequent* registrations are refused. This lane's file, but changing a
setter five crates call, without being able to build, was judged the wrong
trade against a hole no measured configuration currently opens (the VM sets the
mode before the registrars run). Stated so the next lane knows the invariant
`§3.3` rests on.

---

## 6. What this lane could not settle

* **Whether either file compiles.** No `cargo`. The mitigation is real but is
  not the same thing: both files parse (`rustfmt` succeeded on each), the
  changed logic passes 8/8 as a standalone `rustc --test` binary, and every
  type touched was checked by hand against its definition —
  `ObjectRef: Copy + PartialEq` (`types/src/value.rs:105`), `MethodCallFailed`
  in scope in the `native-io` test module via `use super::*` (two existing
  tests already use it), `const {}` in `thread_local!` with four precedents in
  this crate, rustc 1.97.1.
* **The four `drain.conn.*` rows.** N2, and it is not this lane's file. The
  hook they need is built, tested and inert.
* **Whether any bypassed `SyntheticStub` has ever actually run.** By
  construction, unanswerable — that is the defect. `incomplete_slots_of_kind`
  makes the *unanswerability* visible, which is the most a counter-based
  instrument can offer without paying G33-1 §5's measured +9.2 ns/call.
* **The intrinsic-cache family's exact reachable set.** It is bounded by
  `native_builtins::intrinsics::lookup`, so it is not open over the registry —
  but this lane enumerated the two fixed lists exactly and the intrinsic table
  only by its bound. If a `SyntheticStub` triple ever enters that table it
  becomes a fifth way in, and N4's `is_clean()` catches it without anyone
  having to re-run this audit.
* **The `client.peerHost` / `peerPort` and `server.*` rows** (8 of the 14).
  G44-1 §4 and §6 N3/N6; `t27_tls.rs`, not this lane's.
