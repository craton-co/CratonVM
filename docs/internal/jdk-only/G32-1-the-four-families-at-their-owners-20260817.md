# G32-1 — the four families of `RJdkIntrinsics3`, fixed at their owners

**Status:** FIXED, **before MEASURED, after PREDICTED**. This lane could not
build (§8), so no "after" here is measured. What *is* measured is every
"before" — 60 probe rows on both VMs — plus the registry ownership of all
15 triples touched, plus the three registered mechanisms the `fmtobj` fix
depends on. **The edits COMPILE**: the orchestrator ran
`cargo check --workspace --tests` clean over this tree, which covers the 18
unit tests as well as the bodies.

**Provenance:** MEAS on both VMs. Oracle: HotSpot 25.0.3+9-LTS
(`C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`). VMs under test:
`C:/craton/target-rel/release/cratonvm.exe` (commit `783685c34`) and, for the
final re-baseline, `C:/craton/target-rel2/release/cratonvm.exe` (commit
`9964ca733`, which carries the `StringBuilder` insert fixes, the timezone rule
layer, the `HttpRequest` accessors and the proxy return coercion — but **not**
this lane's edits). The `target-fcheck` binary named in the brief was
superseded mid-lane; its timestamp lied, see §8. **All four families' before
states are byte-identical across all three binaries**, and `regex` (42) and
`mathexact` (57) are green on each, so nothing here rests on which was used.
Probe: `scratchpad/g32/src/G32Probe.java`, compiled `-XDstringConcat=inline`,
60 rows, every row run on both VMs and diffed with line endings normalised.

**Source:** `G26-1-four-families-of-RJdkIntrinsics3-20260817.md`. That lane was
assigned these four families in files it did not own, established the real
owners with `--dump-native-registry`, and then measured all four families whole
anyway (402 rows). This record acts on its NOMINATIONS N1, N2, N3, N4 and N6.
Every row it transcribed was re-verified here before being acted on, and **one
of them turned out to be half a rule** (§2).

This lane owns exactly three files: `lib.rs`, `phases_early.rs`,
`deprecated_util.rs`. Everything else is a NOMINATION (§7).

---

## 0. The headline

| what | before (MEASURED) | after |
|---|---|---|
| `RJdkIntrinsics3 --only=fmtobj` | RED at check 1 of 22; **12 of 60 probe rows** | PREDICTED **22** |
| `RJdkIntrinsics3 --only=inet` | RED; **5 of 60 probe rows** | PREDICTED **34** |
| `RJdkIntrinsics3 --only=bufslice` | RED; **5 of 60 probe rows**, one body | PREDICTED **33** |
| `RJdkIntrinsics3 --only=misc` | RED; **3 of 60 probe rows** | PREDICTED **21** |
| `RJdkIntrinsics3 --only=regex` | **GREEN, 42** | unchanged (re-run gate) |
| `RJdkIntrinsics3 --only=mathexact` | **GREEN, 57** | unchanged (re-run gate) |

25 of 60 probe rows diverged before; all 25 are addressed. The four families
together are **110 checks** (22 + 34 + 33 + 21), matching G26-1 §6's arithmetic.

---

## 1. `fmtobj` — the sink's TYPE, and the closed flag that did not exist

`java/util/Formatter`, owner `lib.rs:21574`/`:21725`
(`register_string_format_real_jdk_natives`). Two independent defects, and the
second is much the larger.

### 1.1 `out()` answered a `java.lang.String`

The three constructors wrote `ctx.create_string("")` into slot 0. The real
`Formatter(Locale l, Appendable a)` is `this.a = (a == null) ? new
StringBuilder() : a`, so all four no-`Appendable` spellings answer a
`StringBuilder`:

| row | HotSpot | before |
|---|---|---|
| `new Formatter().out()` class | `java.lang.StringBuilder` | **`java.lang.String`** |
| `out() instanceof StringBuilder` | `true` | **`false`** |
| `out() instanceof Appendable` | `true` | **`false`** |
| `new Formatter(Locale.ROOT).out()` class | `java.lang.StringBuilder` | **`java.lang.String`** |
| `new Formatter((Locale) null).out()` class | `java.lang.StringBuilder` | **`java.lang.String`** |
| `new Formatter((Appendable) null).out()` class | `java.lang.StringBuilder` | **NPE — `out()` was null** |

The third row is the one that shows the shape of it: `out()`'s declared return
type IS `Appendable`, and a `String` is not one. The sixth is a separate bug in
the same slot — an explicit null `Appendable` was *stored*, where the JDK
substitutes a fresh `StringBuilder`.

### 1.2 `close()` was a no-op, and that is 6 of the 12 rows

| row | HotSpot | before |
|---|---|---|
| `toString()` after `close()` | `FormatterClosedException` | **returned** |
| `out()` after `close()` | `FormatterClosedException` | **returned** |
| `flush()` after `close()` | `FormatterClosedException` | **returned** |
| `format()` after `close()` | `FormatterClosedException` | **returned** |
| `locale()` after `close()` | `FormatterClosedException` | **returned** |
| the caller's buffer after a post-close `format` | `pre:005` | **`pre:0051`** |

The last row is the proof rather than a symptom: a `format()` that was
supposed to throw appended a `1` to the caller's `StringBuilder` instead. An
`out()`-only fix would have left the family red at check 15 of 22.

**The two defects are one fix.** The JDK needs no `closed` field —
`close()` is `finally { a = null; }` and `ensureOpen()` is
`if (a == null) throw new FormatterClosedException()`, so **slot 0 being null
IS the closed flag**. That identification is only sound *because* §1.1 is fixed
first: while `new Formatter((Appendable) null)` stored a null, a null slot 0
was an ordinary open Formatter and the guard would have mis-reported it. The
layout is two slots and has no room for a flag, so this is not merely the
tidier option — it is the available one.

### 1.3 Two rows that say where the guard STOPS

Both measured, both easy to get wrong in the same direction:

* **`close()` is idempotent.** A second `close()` must NOT raise
  `FormatterClosedException` — the JDK's `if (a == null) return`. Identifying
  "closed" with "the guard fires" gets this backwards, and the vector checks it
  explicitly (`fmtobj:close() twice` expects `none`).
* **`ioException()` does NOT throw after close.** It is the one public method
  with no `ensureOpen()`. It is not registered here at all, and a unit test now
  asserts that it stays unregistered, so a later lane does not "complete" the
  guard set by adding it.

### 1.4 A GC hazard widened by the fix, so repaired with it

`format`'s append path read slot 0 into a raw `ObjectRef` and *then* called
`create_string`, which allocates. The sink could move in between. This was
latent before (only a caller-supplied `Appendable` took that path) and would
have become the path EVERY Formatter takes, so it is fixed here: the text is
built first, both it and the receiver are pinned, and slot 0 is read after all
allocation. The `unpin` moved to the bottom for the same reason.

The read-modify-write branch beside it — `read_string(slot0)`, concatenate,
`set_field` a fresh `String` — is deleted, not merely bypassed. It was never
just redundant: replacing slot 0 wholesale means `out()` answers a **different
object** after every `format`, and `f.out() == f.out()` is measured true on
HotSpot across repeated `format` calls.

**The fix depends on three mechanisms, and all three are registered natives**
(`--dump-native-registry`, this lane's own workload):
`StringBuilder.<init>()V` → `lang_string.rs:159`,
`append(Ljava/lang/CharSequence;)Ljava/lang/Appendable;` → `lang_string.rs:300`
(registered under that exact descriptor, so it does not depend on bridge-method
resolution), and `toString()` → `lang_string.rs:306`. All `owns_slot=true,
overwrote=null`. The append mechanism was already exercised before this fix:
`new Formatter(sb).format("%d", 42)` appending `42` to a caller's
`StringBuilder` is a row that **already matched**.

---

## 2. `inet` — the ordering, and the half-rule that was inherited

Owner `phases_early.rs:12953` (`createUnresolved`) and `:12865`
(`<init>(String,int)`). G26-1's rule was: a null host is
`IllegalArgumentException: hostname can't be null`, and the port check runs
FIRST. **The second half of that is true of `createUnresolved` and FALSE of the
constructor**, and this lane measured both because the brief said not to
generalise from three rows:

| row | HotSpot | before |
|---|---|---|
| `createUnresolved(null, 80)` | `IllegalArgumentException: hostname can't be null` | **`NullPointerException: null object argument`** |
| `createUnresolved(null, -1)` | `IllegalArgumentException: port out of range:-1` | **`NullPointerException: null object argument`** |
| `createUnresolved(null, 65536)` | `IllegalArgumentException: port out of range:65536` | **`NullPointerException: null object argument`** |
| `new InetSocketAddress((String) null, 80)` | `IllegalArgumentException: hostname can't be null` | **`NullPointerException: null object argument`** |
| **`new InetSocketAddress((String) null, -1)`** | **`IllegalArgumentException: hostname can't be null`** | **`NullPointerException: null object argument`** |

Row 2 and row 5 are the same two invalid arguments, and HotSpot answers
**differently**. The JDK:

```java
public static InetSocketAddress createUnresolved(String host, int port) {
    return new InetSocketAddress(checkPort(port), checkHost(host));
}
public InetSocketAddress(String hostname, int port) {
    checkHost(hostname);
    ...
    holder = new InetSocketAddressHolder(host, addr, checkPort(port));
}
```

`checkPort` is `createUnresolved`'s first *argument*; `checkHost` is the
constructor's first *statement*. Neither ordering is derivable from the other,
and a shared guard written once gets one row right and the other wrong — the
same shape as the JUL rule (`Handler.setLevel` throws, `Logger.setLevel`
returns) that HANDOFF §4 warns about. **G26-1's table listed only
`createUnresolved(null, -1)`, so a lane implementing "port first" from that
table alone would have shipped row 5 wrong.**

Three further things are transcription, not derivation: the message is
`hostname can't be null` (an apostrophe, no article, no period); the port
message is `port out of range:-1` with **no space after the colon**; and
`new InetSocketAddress((InetAddress) null, 80)` is **legal** and must keep
answering the wildcard — measured NOTHROW on both VMs, unchanged here.

`p52_isa_check_host` is a new helper beside the existing `p52_isa_check_port`,
and its doc comment carries the two-orderings table so the next reader does not
"simplify" them into one. A unit test reproduces both orderings and fails if
they are merged.

---

## 3. `bufslice` — one body, four methods, three exception types

Owner `lib.rs:34972`. Five measured rows, all the relative `get()`:

| row | HotSpot | before |
|---|---|---|
| `ByteBufferAsCharBufferB.get()` at the limit | `BufferUnderflowException` | **`IndexOutOfBoundsException`** |
| the same view's `slice()` | `BufferUnderflowException` | **`IndexOutOfBoundsException`** |
| the same view's `duplicate()` | `BufferUnderflowException` | **`IndexOutOfBoundsException`** |
| `ByteBufferAsCharBufferL.get()` at the limit | `BufferUnderflowException` | **`IndexOutOfBoundsException`** |
| `ByteBufferAsCharBufferRB.get()` at the limit | `BufferUnderflowException` | **`IndexOutOfBoundsException`** |

The discriminating rows are the ones on the SAME class that already matched and
must keep matching — `get(int)`, `charAt(int)` and `put(int,char)` are
`IndexOutOfBoundsException`, `put(char)` past the limit is
`BufferOverflowException`, and `position` after a failed `get()` is unchanged
at 4. **Four methods on one class, three exception types.**

`CharBuffer.get()` is `get(nextGetIndex())`, and `Buffer.nextGetIndex()` raises
from the *position bookkeeping* before any index reaches `checkIndex` — which
is why the answer differs from the absolute forms at all. The body delegated to
`bbacb_char_at(ctx, this, 0)` and so reported the absolute class for all four.

**It really is one registration, not five.** `slice()` and `duplicate()` of a
`ByteBufferAsCharBufferB` are `ByteBufferAsCharBufferB`, and the dump confirms
`ByteBufferAsCharBufferRB.get()C` is **not registered at all** — it inherits
`B`'s body. Fixing the `B`/`L` loop covers all five rows.

`position`/`limit` are read through `bbacb_read_underlying_bytes`, the same
helper the read path uses, rather than a bare `get_field_by_name`: it carries
the slot-index fallbacks, and a `limit` that silently read 0 would make every
relative `get()` throw. `phases_late/nio_buffer.rs`'s `relative::<WIDTH>` is
the worked exemplar and was read, not edited.

---

## 4. `misc` — a Rust `str` in the middle, and a constructor that returned

### 4.1 `new String(StringBuilder)` lost every lone surrogate

`deprecated_util.rs:656`. The last line was
`String::from_utf16_lossy(&chars).into_bytes()`, and a Rust `str` cannot hold
an unpaired surrogate at all:

| row | HotSpot | before |
|---|---|---|
| `sb.append((char) 0xDC00); new String(sb).charAt(0)` | `56320` | **`65533`** |
| `new String(sb of "ab" + U+D800 + "c")` as units | `97,98,55296,99` | **`97,98,65533,99`** |

The builder was never lossy — `sb.charAt(0)` and `sb.toString().charAt(0)` both
already answered 56320 — so this constructor **disagreed with the very object
it was copying**. Note the second row keeps the same *length*, so a
length-only assertion passes on the broken code; the unit tests assert units.

**N4 proposed `crate::lang_string::sb_string_from_units`, and that call does not
fit here.** `sb_string_from_units` ALLOCATES a fresh `String` and returns it —
correct for a String-returning native, wrong for a constructor, which must
populate the receiver the caller already holds a reference to. The right
primitive is one level down: `ctx.init_string_from_units(this, &units)`, which
is what `sb_string_from_units` itself ends in. Both land in the same
`populate_java_string_fields`, so this adds no third spelling of the
construction — the thing `init_string_from_units`' own doc comment warns
against.

`new String(StringBuffer)` was measured too and **already matched** (55296 on
both) — it has no native and runs real bytecode. No action.

### 4.2 `new EnumMap((Class) null)` returned

`phases_early.rs:6084`. `EnumMap(Class)` reaches
`keyType.getEnumConstantsShared()` unguarded, so the JDK throws before storing
anything. This body fell through to its `_ =>` arm, built a zero-length
universe and returned, leaving a live map with a null `keyType` whose every
later `put` would fail somewhere else with an unrelated message.

| row | HotSpot | before |
|---|---|---|
| `new EnumMap((Class) null)` | `NullPointerException` | **returned** |

Message transcribed for a later lane that compares them:
`Cannot invoke "java.lang.Class.getEnumConstantsShared()" because "klass" is
null`. `RJdkIntrinsics3`'s `ckX` compares only the exception CLASS, so the text
is fidelity, not gate.

Two registrars name this triple (`register_enum_map_init_native` and
`register_enum_map_natives`) but **both point at the same `native_em_init`**,
so last-write-wins is harmless here — checked, because it is exactly the shape
that has bitten this repo.

---

## 5. What changed, by name

| file | site | change |
|---|---|---|
| `lib.rs` | `formatter_alloc_sink` (new) | allocates the `StringBuilder` sink, GC-safe |
| `lib.rs` | `formatter_ensure_open` (new) | the `ensureOpen()` guard; slot 0 is the flag |
| `lib.rs` | `formatter_closed` (new) | builds a real `java/util/FormatterClosedException` |
| `lib.rs` | `Formatter.<init>` ×3 | sink is a `StringBuilder`, incl. the null-`Appendable` substitution |
| `lib.rs` | `Formatter.format` | `ensureOpen`; append-only path; GC pin repair |
| `lib.rs` | `Formatter.{toString,out,flush,locale}` | `ensureOpen` |
| `lib.rs` | `Formatter.close` | nulls slot 0; stays idempotent and non-throwing |
| `lib.rs` | `ByteBufferAsCharBuffer{B,L}.get()C` | `BufferUnderflowException` on `position >= limit` |
| `phases_early.rs` | `p52_isa_check_host` (new) | the null-host refusal + the two-orderings contract |
| `phases_early.rs` | `InetSocketAddress.<init>(String,int)` | host checked FIRST |
| `phases_early.rs` | `InetSocketAddress.createUnresolved` | port checked FIRST |
| `phases_early.rs` | `native_em_init` | null key type → NPE, ahead of the real/synthetic split |
| `deprecated_util.rs` | `native_string_init_from_string_builder` | `init_string_from_units`, no `str` round trip |

**No registration was added, moved or removed.** All 15 triples touched were
verified `owns_slot=true, overwrote=null` and in this lane's three files by
`--dump-native-registry` against this lane's own workload (§6).

18 unit tests are added in the existing `#[cfg(test)]` modules
(`g32_formatter_tests` is new in `lib.rs`; the others extend `t2_tests` in
`phases_early.rs` and `tests` in `deprecated_util.rs`). The surrogate tests
assert **units**, never a `read_string` round trip — a test written through
`read_string` passes on the broken code, because that conversion is the bug.

---

## 6. Verification performed

* **`git status --short` on the three files** — all three modified, nothing
  else of this lane's touched. Pasted in the report.
* **`--dump-native-registry`**, `--jdk-only`, against this lane's own probe:
  all 15 edited triples `owns_slot=true`, `overwrote=null`, registered by this
  lane's three files. `ByteBufferAsCharBufferRB.get()C` confirmed **not
  registered** (inherits `B`). The three `StringBuilder` mechanisms the
  `fmtobj` fix depends on confirmed registered and owned by `lang_string.rs`.
* **Repo-wide grep for every touched `(class, name, descriptor)` triple.** One
  near-miss found and cleared: `phases_late/charset_buffers.rs:1384-1391`
  registers on `ByteBufferAsCharBuffer{B,RB,L,RL}` — but only `order()`, not
  `get()C`. `phases_early.rs:13421` is a *caller* of
  `InetSocketAddress.<init>(String,int)`, not a registrar, and its host is
  provably non-null (`unreachable!` guard), so the new host check cannot reach it.
* **`rustfmt --edition 2021 --check`**, in place, in this tree: `lib.rs` **0**
  hunks, `deprecated_util.rs` **0**, `phases_early.rs` **35** — identical to
  the pre-edit baseline. (`rustfmt` on `lib.rs` follows `mod` declarations and
  checks the whole crate; the counts above are filtered to these three files.
  The two hunks this lane's first draft introduced were fixed, not accepted.)
* **Zero CR bytes** in all three files.
* **Vectors, before, on the new attributable binary:** `RJdkHello` 41,
  `RStrings` 46, `RJdkNet` 81, `RDirectBufferElem` 506, `RJdkFormatLocale` 20,
  `RJdkLogging` 79 — all PASS. `RSimpleTimeZoneRaw` RED at
  `New_York inDaylightTime(JUL)`, which is the timezone rule layer being
  rebuilt in another lane and is not in this binary; it is not attributable to
  this lane and no file this lane touched is on its path.
* **60 probe rows on both VMs**, `scratchpad/g32/src/G32Probe.java`.

---

## 7. NOMINATIONS

**N1 — `native-builtins/src/phases_late/charset_buffers.rs`, `cb_read_text`.**
Carried forward unchanged from G26-1's N7: it answers a Rust `String`, so the
`java/nio/*CharBuffer*` arm of `lang_string`'s `charsequence_fast_units` is
still lossy for a lone surrogate. Not this lane's file.

**N2 — `native-builtins/src/lang_math.rs:771`, `String.valueOf(Object)`.**
Carried forward from G26-1's N5, unfixed and still measurable. Not this lane's
file.

**N3 — `obj_arg`'s NPE message, VM-wide.** MEASURED: `new String((StringBuilder)
null)` is `NullPointerException` on both VMs, but the message is HotSpot's
helpful-NPE text (`Cannot invoke "java.lang.AbstractStringBuilder.getValue()"
because "asb" is null`) against CratonVM's generic `null object argument`. The
class matches, so no vector row turns on it and this lane did not touch it —
but it is a whole-VM message-fidelity gap, not a one-site one, and it should be
scoped as such rather than patched per-native.

**N4 — `register_phase52_string_buffer` (`lang_string.rs:12637`).** Carried
forward from G26-1 §5 with its measurement (29 divergent Compatible-mode rows
against `--jdk-only`'s 28). Untouched and unverified here; this lane's gate is
`--jdk-only`, which does not reach it.

---

## 8. What this lane did NOT settle

* **It did not measure its own "after".** `cargo build`/`check`/`test` were
  forbidden, so the binary predates every edit here by construction. Every
  "after" in §0 is PREDICTED. What is not predicted is every "before", the
  registry ownership of all 15 triples, and the three registered mechanisms
  §1.4 depends on — those are measured on the mechanism rather than on the fix.
* **It did not run the 18 unit tests it wrote.** They COMPILE — the
  orchestrator's `cargo check --workspace --tests` was clean — but compiling is
  not passing, and no lane can claim a passing test it did not run. They are
  written against the `mock_ctx` / `MockNativeContext` idiom already used by the
  surrounding modules, with the `NativeHeapAccess` import block from
  `lang_class.rs`'s test module.
* **The binary named in the brief was wrong and the timestamp did not say so.**
  `target-fcheck/release/cratonvm.exe` was produced by a build that partly
  failed — cargo could not replace the exe because lanes held the Windows file
  lock, so its mtime advanced while its contents did not. Everything in §0 was
  re-measured on `target-rel` (`783685c34`) and again on `target-rel2`
  (`9964ca733`); the four families' before-states were **identical on all
  three**, so nothing here rests on the difference. Recorded because "verify
  behaviourally, not by timestamp" had a live counter-example this session.
* **`lib.rs` carried ~400 lines of another lane's uncommitted work** (the
  `InheritableThreadLocal` capture) for this lane's whole duration, and
  `native-api/src/registry.rs` was modified by a third lane mid-session. Neither
  was touched. The `git status` in the report is the proof.
* **It did not re-verify `RSimpleTimeZoneRaw`'s failure**, which belongs to the
  timezone lane (§6).
