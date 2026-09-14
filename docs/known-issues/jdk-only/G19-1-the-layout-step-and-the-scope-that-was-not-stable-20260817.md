# G19-1 — the layout step, and the scope that was not stable

**Status:** two defects located by MEASUREMENT and fixed; **the fixes are not
measured.**

**Provenance, per row:**

* Every **oracle** row below is **MEASURED** on HotSpot 25.0.3+9-LTS (Temurin,
  `openjdk version "25.0.3" 2026-04-21 LTS`, build `25.0.3+9-LTS`).
* Every **"CratonVM before"** row is **MEASURED** on
  `C:/craton/target-fcheck/release/cratonvm.exe`, built from `d2e127930`, under
  `--jdk-only --enable-native-access=ALL-UNNAMED`, against
  `C:/craton/cvm-mergecheck/regression-suite/build`. That binary already
  carries G6-1's work, so it is a genuine "before" for this lane.
* Every **"CratonVM after"** row is **PREDICTED**. This lane was forbidden to
  run `cargo build`/`check`/`test` (the orchestrator owns the target-dir lock),
  so no binary has ever executed a line of the code below. HANDOFF-20260814 §2
  applies to this record's after-column in full: *a prediction is not a
  result.* The predictions are written to be falsifiable — exact check counts
  and exact `CK` lines — so one run settles each of them.

Probes: `scratchpad/g19/G19Probe.java` (oracle sweep, 3 sections),
`scratchpad/g19/G19Vm.java` (the same rows on both VMs, side by side).
Registry dumps: `/tmp/reg3.json` (`RForeignLayoutJdkInterfaces`),
`/tmp/reg4.json` (`RJdkForeign`). **Every `registered_by` line number quoted
from a dump is the `d2e127930` BINARY's, not the current tree's** — this
lane's own edits move both files. Line numbers given as "current tree" are
from the working tree at the time of writing.

Files owned and changed: `native-builtins/src/panama.rs`,
`native-builtins/src/phases_late/foreign_ffm.rs`. `panama_libffi.rs` is owned
and **unchanged** — see §7. Nothing else was touched.

---

## 0. The headline

Two vectors, two assertions, two unrelated defects — and both are the same
shape as everything else in this directory: a carrier this VM can MINT and
cannot READ.

| # | vector | the assertion | cause |
|---|---|---|---|
| 1 | `RJdkForeign` | `1 of 7 steps failed: [layouts]` | `FunctionDescriptor.returnLayout()` and `.argumentLayouts()` have **no registered body** in `--jdk-only`; the factories that mint the carrier do (§1) |
| 2 | `RForeignLayoutJdkInterfaces` | `a heap segment's scope is stable` | `scope()` minted a **fresh session on every call** for any carrier whose slot 2 does not hold an *Arena* — which is every heap segment and every slice (§2) |

Neither is a wrong number. Both are the fail-open/refuse-late pair this
directory keeps finding: one raises `AbstractMethodError` from inside a step
that had already produced a correct answer for its first eleven checks (of
sixteen), and the
other answers a *live* scope for a segment whose arena has closed.

---

## 1. `RJdkForeign` — what `[layouts]` actually was

### 1.1 The step name is not the assertion

`step("layouts", ...)` names SIXTEEN checks. MEASURED, CratonVM before:

```
FAIL RJdkForeign step layouts: java.lang.AbstractMethodError:
  method java/lang/foreign/FunctionDescriptor.returnLayout()Ljava/util/Optional;
  has no Code attribute
```

So the first ELEVEN of those sixteen — the seven `ValueLayout.*.byteSize()`
rows, both `carrier()` rows, `structLayout(int,int,long).byteSize() == 16` and
`byteOffset(groupElement("c")) == 8` — **passed**. The layout family that the
step is named after was already correct. The step died on the
`FunctionDescriptor` half.

Arithmetic that confirms it: HotSpot reports `checks=75`, CratonVM reports
`checks=70`. The five missing checks are exactly the five that follow the
throw, and the `CK RJdkForeign layouts struct=16` line is missing from
CratonVM's output for the same reason.

### 1.2 Which body runs — settled with the dump, not by reading

`--dump-native-registry /tmp/reg4.json` **before** the main class, under the
failing run:

```
java/lang/foreign/FunctionDescriptor  of      (…MemoryLayout;[…MemoryLayout;)…  owns_slot=true inv=5  foreign_ffm.rs:4229
java/lang/foreign/FunctionDescriptor  ofVoid  ([…MemoryLayout;)…                owns_slot=true inv=0  foreign_ffm.rs:4245
```

**Those are the only two `FunctionDescriptor` rows in the whole registry.**
`panama.rs`'s `register_pe_function_descriptor` *does* carry a `returnLayout`
and an `argumentLayouts` — and it does not run in `--jdk-only`: it is reached
only from `register_pe_panama` / `register_synthetic_overrides`. Reading
`panama.rs` alone would have said this surface was covered. It is HANDOFF §5's
rule paying for itself again.

`ofVoid` is `inv=0` because `layouts()` throws before it reaches
`FunctionDescriptor.ofVoid(JAVA_INT)`.

### 1.3 And the dead copy has the wrong signature anyway

`panama.rs:5225` registers

```
argumentLayouts ()[Ljava/lang/foreign/ValueLayout;
```

which is the **pre-JDK-22 preview** spelling. MEASURED on the oracle,
25.0.3+9-LTS: `argumentLayouts()` returns `java.util.List`
(`java.util.ImmutableCollections$List12` for a one-argument descriptor). So
even if that registrar had been reached, `fd.argumentLayouts().size()` would
have raised the identical `AbstractMethodError`, on
`()Ljava/util/List;` instead.

### 1.4 The `FunctionDescriptor` family on the oracle

MEASURED, `G19Probe` §FD. Every row; `<null>` is printed distinctly from `""`.

| row | oracle |
|---|---|
| `of(JAVA_LONG, ADDRESS).getClass()` | `jdk.internal.foreign.FunctionDescriptorImpl` |
| `of(JAVA_LONG, ADDRESS).toString()` | `(a8)j8` |
| `.returnLayout()` | `Optional[j8]`, class `java.util.Optional` |
| `.returnLayout().get() == ValueLayout.JAVA_LONG` | `true` (identity, not just equals) |
| `.argumentLayouts().getClass()` | `java.util.ImmutableCollections$List12` |
| `.argumentLayouts().size()` | 1 |
| `.argumentLayouts().get(0).equals(ADDRESS)` | `true` |
| `.argumentLayouts().add(JAVA_INT)` | `UnsupportedOperationException`, null message |
| `.argumentLayouts() == .argumentLayouts()` | `true` — one stored list, handed back |
| `ofVoid(JAVA_INT).toString()` | `(i4)v` |
| `ofVoid(JAVA_INT).returnLayout()` | `Optional.empty` |
| `ofVoid().argumentLayouts().size()` | 0 |
| `of(JAVA_INT,JAVA_INT).equals(of(JAVA_INT,JAVA_INT))` | `true` — **structural** |
| `of(JAVA_INT) == of(JAVA_INT)` | `false` — not interned |
| `hashCode()` agrees across equal descriptors | `true` |
| `of(JAVA_LONG, ADDRESS).toMethodType()` | `(MemorySegment)long` |
| `ofVoid(JAVA_INT).toMethodType()` | `(int)void` |
| `of(JAVA_INT).toMethodType()` | `()int` |
| `of(structLayout(JAVA_INT,JAVA_INT)).toMethodType()` | `()MemorySegment` |
| `ofVoid(JAVA_INT).changeReturnLayout(JAVA_INT)` | `(i4)i4` |
| `of(JAVA_LONG,ADDRESS).dropReturnLayout()` | `(a8)v` |
| `.appendArgumentLayouts(JAVA_INT)` | `(a8i4)j8` |
| `.insertArgumentLayouts(0, JAVA_INT)` | `(i4a8)j8` |
| `.insertArgumentLayouts(9, JAVA_INT)` | IAE `Index out of bounds: 9` |
| `of((MemoryLayout) null)` | `NullPointerException`, null message |
| `of(JAVA_INT, (MemoryLayout) null)` | `NullPointerException`, null message |
| `ofVoid((MemoryLayout) null)` | `NullPointerException`, null message |
| `of(paddingLayout(4))` | IAE `Unsupported padding layout return in function descriptor: x4` |
| `ofVoid(paddingLayout(4))` | IAE `Unsupported padding layout argument in function descriptor: x4` |
| `of(sequenceLayout(2, JAVA_INT))` | `()[2:i4]` — accepted |

### 1.5 The same rows on CratonVM, before

MEASURED, `G19Vm.java`, `--jdk-only --enable-native-access=ALL-UNNAMED`:

| row | CratonVM before | oracle |
|---|---|---|
| `fd.returnLayout()` | `AbstractMethodError: … returnLayout()Ljava/util/Optional; has no Code attribute` | `Optional[j8]` |
| `fd.argumentLayouts()` | `AbstractMethodError: … argumentLayouts()Ljava/util/List;` | `[a8]` |
| `voidFd.returnLayout().isEmpty()` | the same `AbstractMethodError` | `true` |
| `fd.getClass()` | `java.lang.foreign.FunctionDescriptor` | `jdk.internal.foreign.FunctionDescriptorImpl` |
| `fd.toString()` | `java.lang.foreign.FunctionDescriptor@4b8` | `(a8)j8` |
| `fd.equals(fd)` | `true` | `true` |
| `ValueLayout.JAVA_LONG == ValueLayout.JAVA_LONG` | `true` | `true` |
| `JAVA_LONG.equals(JAVA_LONG)` | `true` | `true` |
| `JAVA_LONG.equals(JAVA_INT)` | `false` | `false` |
| `JAVA_LONG.toString()` | `java.lang.foreign.ValueLayout$OfLong@4aa` | `j8` |
| `JAVA_LONG.name()` | `Optional.empty` | `Optional.empty` |
| `JAVA_LONG.getClass()` | `java.lang.foreign.ValueLayout$OfLong` | `…layout.ValueLayouts$OfLongImpl` |

Two things this table settles that a one-row look would not have:

1. **Layout `equals` is IDENTITY on CratonVM and it coincides with the oracle
   on every row `layouts()` asserts.** `ValueLayout.JAVA_LONG` is *stable*
   across reads on this VM (`==` is `true`), and the object stored in the
   descriptor is the very one the call site passed, so
   `fd.returnLayout().get().equals(ValueLayout.JAVA_LONG)` is satisfied by
   reference equality. **No `equals` work is needed for this vector.** It is
   still a divergence for constructed layouts — see §6.
2. **`toString` and `equals` cannot be fixed from these files.** Both have a
   concrete `java.lang.Object` implementation, and virtual dispatch finds it
   before any native registration; that is exactly why they answer an identity
   hash today while `returnLayout()` — abstract on the sealed interface, no
   `Object` fallback — raises `AbstractMethodError` instead. A registration for
   `returnLayout`/`argumentLayouts` wins; one for `toString`/`equals` would be
   dead. See **NOM-2**.

### 1.6 The fix

`foreign_ffm.rs`, immediately after the `of`/`ofVoid` pair that mints the
carrier (so the reader and the writer of slots 0 and 1 are three lines apart):

* `returnLayout ()Ljava/util/Optional;` → `p67_optional(ctx, field 0)`. The
  void carrier's null slot 0 becomes an EMPTY `Optional`, which is the measured
  answer, and `p67_optional` is the helper `name()`/`targetLayout()` already
  use — measured working against a real `java.util.Optional` in `--jdk-only`,
  because `JAVA_LONG.name()` prints `Optional.empty` on this binary today.
* `argumentLayouts ()Ljava/util/List;` → `List.of(field 1)` with an
  `Arrays.asList` fallback, the idiom `lang_invoke::vh_coordinate_types` uses.
  That idiom is measured working in this binary: `RJdkForeign`'s
  `layoutVarHandles` step asserts
  `coordinateTypes().equals(List.of(MemorySegment.class, long.class))` and is
  green.

**PREDICTED after:** `layouts` passes, `RJdkForeign` prints
`CK RJdkForeign layouts struct=16`, reaches `checks=75` (HotSpot's number) and
`PASS RJdkForeign (75 checks, 7 steps)`.

---

## 2. `RForeignLayoutJdkInterfaces` — the scope that was not stable

### 2.1 The assertion

MEASURED, CratonVM before:

```
CK RForeignLayoutJdkInterfaces xml=12 dos=12
CK RForeignLayoutJdkInterfaces confinedArena=closed
CK RForeignLayoutJdkInterfaces sharedArena=closed
AssertionError: RForeignLayoutJdkInterfaces: a heap segment's scope is stable
        at RForeignLayoutJdkInterfaces.foreignArenaLifetime(…:600)
```

Line 600 is `check(heap.scope() == heap.scope(), …)` on
`MemorySegment.ofArray(new byte[16])`. Everything before it — including
`confined.scope() == confined.scope()`, `live.scope() == confined.scope()`, and
both closed-arena arms — is green. The vector's own comment calls this row *the
one scope-stability row that was ALREADY green before the repair*; it is not,
and that comment is the PREDICTED half of W7-89 that measurement falsifies.

HotSpot: 172 checks, PASS. The failing check is the second-to-last of the 172.

### 2.2 The scope family on the oracle

MEASURED, `G19Probe` §SC. This is the section that says what the answer has to
BE, and two of its rows are counter-intuitive enough that a plausible fix
fails them.

| row | oracle |
|---|---|
| `ofArray(byte[16]).scope().getClass()` | `jdk.internal.foreign.GlobalSession$HeapSession` |
| `heap.scope() == heap.scope()` | **true** |
| `heap.scope().isAlive()` | true |
| `heap.asSlice(4,4).scope() == heap.scope()` | **true** |
| `heap.asReadOnly().scope() == heap.scope()` | **true** |
| `ofArray(a).scope() == ofArray(a).scope()` (same array) | **false** |
| `heapA.scope() == heapB.scope()` | **false** |
| `heap.scope() == Arena.global().scope()` | **false** |
| `MemorySegment.NULL.scope() == MemorySegment.NULL.scope()` | true |
| `MemorySegment.NULL.scope() == Arena.global().scope()` | **true** |
| `ofAddress(16).scope() == Arena.global().scope()` | true |
| `Arena.global().scope() == Arena.global().scope()` | true |
| `Arena.ofAuto().scope().getClass()` | `jdk.internal.foreign.ImplicitSession` |
| `auto.scope() == Arena.global().scope()` | false |
| `auto.allocate(8).scope() == auto.scope()` | true |
| `conf.allocate(16).scope() == conf.scope()` | true |
| `cs.asSlice(4,4).scope() == cs.scope()` | true |
| `cs.asReadOnly().scope() == cs.scope()` | true |
| `cs.reinterpret(8).scope() == cs.scope()` | true |
| `conf.scope()` after `conf.close()` | the SAME object; `isAlive()` false |
| `cs.scope()` after `conf.close()` | the same object; `isAlive()` false |
| shared arena: all of the above | identical |
| `ofBuffer(ByteBuffer.allocate(8)).scope()` twice | false (two buffers, two scopes) |

> **A heap segment's scope is one session per SEGMENT.** Not per call, not per
> array, and not the global session. A fix that handed every heap segment one
> shared singleton satisfies rows 2–5 and fails rows 6 and 7.

### 2.3 The same rows on CratonVM, before

MEASURED, `G19Vm.java` §S:

| row | CratonVM before | oracle |
|---|---|---|
| `heap.scope() == heap.scope()` | **false** | true |
| `heap.asSlice(4,4).scope() == heap.scope()` | **false** | true |
| `heap.asReadOnly().scope() == heap.scope()` | **false** | true |
| `cs.asSlice(4,4).scope() == cs.scope()` (native, arena) | **false** | true |
| `heap.scope().isAlive()` | true | true |
| `heap.scope().getClass()` | `jdk.internal.foreign.MemorySessionImpl` | `…GlobalSession$HeapSession` |
| `heapA.scope() == heapB.scope()` | false | false |
| `conf.scope() == conf.scope()` | true | true |
| `cs.scope() == conf.scope()` | true | true |
| `conf.scope().isAlive()` after close | false | false |
| `Arena.global().scope() == Arena.global().scope()` | **false** | true |
| `MemorySegment.NULL.scope() == …NULL.scope()` | **false** | true |
| `heap.byteSize()` / `get` / `isNative` / `heapBase` / `toArray` | 16 / 0 / false / true / 16 | identical |

Row 4 is the one that shows this is **not** a heap-only bug: a slice of an
ordinary confined-arena segment had an unstable scope too, and no vector was
asking.

### 2.4 The cause, and it is a disagreement between two files

`MemorySegment.scope()` is served by `p67_receiver_session`
(`foreign_ffm.rs:2920`, `owns_slot=true`, **`inv=5`** under this vector —
`/tmp/reg3.json`). Its resolution order was:

1. a real-JDK receiver's named `scope`/`session` field;
2. the receiver as a synthetic **Arena**;
3. slot 2 of a synthetic segment, read as the **Arena** that allocated it;
4. otherwise **mint a fresh session**.

Step 4 is the bug's delivery mechanism, but step 3 is the bug. Since W7-89,
`panama::pe_segment_slice` has stamped a slice's slot 2 with the **parent's
SESSION**, not with an arena — and `panama::pe_segment_session` has carried an
explicit *"tolerate a segment stamped with the session directly"* arm for
exactly that shape the whole time. `foreign_ffm`'s reader never grew the
matching arm. So the two files disagreed about what slot 2 can hold, and every
slice fell through to step 4.

A freshly minted session is always open. So this was never only an identity
divergence: **`slice.scope().isAlive()` answered `true` for a slice of a closed
arena**, and any liveness check that resolved through this reader passed.

`MemorySegment.ofArray(byte[]/short[]/char[])` (`panama.rs:1478`,
`pe_of_array_alias`, `owns_slot=true`, `inv=1` under this vector) had nothing in
slot 2 at all, so it reached step 4 unconditionally.

### 2.5 The fix — four edits, one rule

The rule is: **a segment's scope is an object the segment carries, minted with
it.**

1. `panama::pe_of_array_alias` — mint one session at `ofArray` time and stamp
   it into slot 2. Per segment, which is what §2.2 rows 6–7 demand.
2. `panama::pe_segment_slice`, **heap arm** — propagate the parent's session
   into the slice's slot 2. `asReadOnly()` reaches the same body, so one write
   covers both measured rows.
3. `foreign_ffm::p67_receiver_session` — accept a modelled session sitting
   directly in slot 2, mirroring `panama::pe_segment_session`.
4. `foreign_ffm::p67_segment_check_scope` — the same arm, so the newly
   resolvable session is **checked** and not merely reported. Without (4), the
   one shape whose scope resolves would be the one shape that skips the
   validity check — the fail-open W7-89 closed one branch up.

**Why slot 2 and not a ninth slot.** Slot 2 already has two tenants: the
owning Arena, and (on an `ofArray` *mirror* carrier) the Java backing array.
Every other reader of it — `sync_heap_backed_segment`,
`pe_segment_heap_base`'s fallback, and the `isNative` discriminator — gates on
`ctx.object_is_array`, which a session is not, so none of them changes its
answer. A test asserts exactly that (§4).

**Why `panama::pe_session_modelled` and not the local `p67_session_modelled`
in the two new `foreign_ffm` arms.** The local predicate is width-and-state-
word only; it has no class-name test. Slot 2's other tenant is a Java array,
and asking `object_num_fields`/`get_field` about an array is not a question
this VM answers the same way everywhere. `panama`'s copy pins the class name to
`jdk/internal/foreign/MemorySessionImpl` and memoises the id, so the extra
precision costs an integer compare. It was made `pub(crate)` for this; there is
now ONE session-recognition predicate reached from both files, which is the
drift this whole area keeps paying for.

**PREDICTED after:** `RForeignLayoutJdkInterfaces` prints
`CK RForeignLayoutJdkInterfaces neverCloses=ok`, `checks=172` and
`PASS RForeignLayoutJdkInterfaces (172 checks)` — HotSpot's exact numbers,
because the failing check is the last one in the method.

---

## 3. One more thing the fix does to the memo, and it is test-only

`PeClassMemo` remembers one hit class id and one miss class id for the whole
process. That is sound on the real VM, where class ids are process-stable. It
is **not** sound under `MockNativeContext`, where ids are a per-context counter
— id 7 names `MemorySegment` in one test and `MemorySessionImpl` in the next,
and the second test is handed the first test's answer for a class it never saw.
Tests run in threads, so which answer you get depends on which sibling ran
last.

`PeClassMemo::matches` now takes the name-comparison path directly under
`cfg!(test)`. The memo is a pure cache, so no answer changes — only the number
of `class_name_arc_of_id` calls, and only in test builds. Without it the three
tests in §4 would be non-deterministic, which is worse than not having them.

---

## 4. Tests

`panama.rs`, existing `mod tests`:

* `a_heap_segments_scope_is_one_session_shared_by_its_slices` — the four
  measured identity rows, including the **negative** one (`ofArray(a)` twice
  over the same array gives two distinct scopes), which is what stops a
  singleton "fix" from passing. Asserted both by slot and through
  `pe_segment_session`, because the slot write is only half the repair.
* `stamping_the_scope_does_not_disturb_the_other_tenants_of_slot_two` —
  `heapBase()` still hands back the caller's array, the carrier still decodes
  as a heap view, and the read-only contagion still withholds the array on a
  carrier whose slot 2 is now occupied.

`foreign_ffm.rs`, new `mod g19_scope_tests` (the file had none):

* `a_stamped_session_is_the_scope_and_it_is_the_same_object_every_time`.
* `an_array_in_slot_two_is_not_a_scope` — the negative half; a backing array
  must never be handed out as a scope, and the fallback is still a session.
* `a_stamped_session_that_has_closed_refuses_the_access` — the §2.5(4) arm,
  asserted by raising `IllegalStateException: Already closed`.

---

## 5. Registration evidence (HANDOFF §5)

`--dump-native-registry` placed **before** the main class, both vectors, on the
`d2e127930` binary.

| triple | owns_slot | inv | registered_by |
|---|---|---|---|
| `FunctionDescriptor.of(MemoryLayout,MemoryLayout[])` | true | 5 | `foreign_ffm.rs:4229` |
| `FunctionDescriptor.ofVoid(MemoryLayout[])` | true | 0 | `foreign_ffm.rs:4245` |
| *any other `FunctionDescriptor` row* | — | — | **none exists** |
| `MemorySegment.scope()` | true | 5 / 2 | `foreign_ffm.rs:2920` |
| `MemorySegment.ofArray([B)` | true | 1 | `panama.rs:1478` |
| `MemorySegment.asSlice(JJ)` | true | 1 | `panama.rs:943` |
| `MemorySegment.asSlice(JJ)` (twin) | **false** | 0 | `foreign_ffm.rs:2830` |
| `MemorySessionImpl.isAlive()` | true | 7 | `foreign_ffm.rs:2700` |

Every body this lane edited is on a row with `owns_slot=true` and a non-zero
`invocations` under the vector it is meant to fix. The `asSlice(JJ)` twin at
`foreign_ffm.rs:2830` is the *loser* of that slot — worth knowing, because a
fix applied there would have been invisible.

---

## 6. Measured surface this lane did NOT act on

All MEASURED, none fixed. Recorded so the next lane does not re-measure.

### 6.1 `Arena.global()` and `MemorySegment.NULL` are not singletons

```
Arena.global().scope() == Arena.global().scope()        CratonVM false   HotSpot true
MemorySegment.NULL.scope() == MemorySegment.NULL.scope() CratonVM false   HotSpot true
```

Both static accessors MINT A NEW OBJECT ON EVERY READ
(`Arena.global` at `foreign_ffm.rs:2491`, `MemorySegment.NULL` at
`foreign_ffm.rs:2976`, current tree), so `MemorySegment.NULL !=
MemorySegment.NULL` as well. On the oracle both `NULL.scope()` and
`ofAddress(n).scope()` are `Arena.global().scope()` itself. Fixing it needs a
process-lifetime GC root for the singleton, which is a different kind of change
from anything here and is not something to attempt without a running VM. **No
scheduled vector asserts it today**, which is the only reason it is being left.

### 6.2 The four copying `ofArray` arms have no scope either

`ofArray(int[]/long[]/float[]/double[])` (`panama.rs:1260/1304/1352/1393`) mint
the *mirror* carrier, whose slot 2 holds the backing ARRAY. They therefore
still reach the fresh-session fallback, and `ofArray(new int[8]).scope()` is
still unstable. They were not given a session because their slot 2 is already
occupied and G6-1 has already NOMINATED unifying those four onto the alias
carrier; doing both at once would make one change unattributable.

### 6.3 `toString` on every FFM carrier prints an identity hash

```
ValueLayout.JAVA_LONG.toString()   CratonVM java.lang.foreign.ValueLayout$OfLong@4aa   HotSpot j8
fd.toString()                      CratonVM java.lang.foreign.FunctionDescriptor@4b8   HotSpot (a8)j8
```

`p67_layout_render` already produces the oracle's spelling for nine layout
shapes (G6-1 §8.5). It cannot be reached: `toString` has a concrete
`java.lang.Object` body and wins dispatch. This is a **cross-VM diff hazard**
as well as a divergence — the hash is nondeterministic, so any future vector
that prints a layout is a flapping diff. See **NOM-2**.

### 6.4 Layout and descriptor `equals` are identity, not structural

```
of(JAVA_INT,JAVA_INT).equals(of(JAVA_INT,JAVA_INT))       HotSpot true
JAVA_INT.withName("a").equals(JAVA_INT.withName("a"))     HotSpot true
```
CratonVM answers `false` for both (`Object.equals`). It coincides with the
oracle on every row `RJdkForeign` asserts only because `ValueLayout.JAVA_LONG`
is stable across reads and the descriptor stores the caller's own object. Same
dispatch problem as §6.3; same nomination.

### 6.5 The rest of the `FunctionDescriptor` interface is unregistered

`toMethodType`, `changeReturnLayout`, `dropReturnLayout`,
`appendArgumentLayouts`, `insertArgumentLayouts` and `withOrder` are all
abstract-only, so all six raise `AbstractMethodError` today. §1.4 transcribes
the oracle's answer and its exact refusal message for each. They are NOT
implemented here: this lane could not compile, and six new registrations
written blind is a worse trade than six recorded rows. `jextract` bindings use
`of`/`ofVoid` and `Linker` only, which is why the two readers were the whole of
the vector.

### 6.6 The two factories accept what the oracle refuses

```
FunctionDescriptor.of((MemoryLayout) null)   HotSpot NullPointerException
FunctionDescriptor.ofVoid(paddingLayout(4))  HotSpot IAE: Unsupported padding layout argument in function descriptor: x4
```
CratonVM accepts both and builds a descriptor a downcall will then marshal from
garbage. Transcribed, not fixed, for the §6.5 reason.

---

## 7. What this lane did NOT do

* **It did not build, and it did not run its own fix.** `cargo build`,
  `cargo check` and `cargo test` were all forbidden. The whole "after" column
  of this record is PREDICTED. `rustfmt --edition 2021 --check` was run in
  place on all three owned files and parses; the pre-existing hunk counts are
  unchanged (`panama.rs` 51, `foreign_ffm.rs` 17, `panama_libffi.rs` 4), which
  is evidence about formatting and nothing else.
* **It did not touch `panama_libffi.rs`**, though it owns it. G6-1's NOM-2
  (prefer the stored element count at slot 4 in `layout_to_ffi_type`) and NOM-3
  (a five-slot sibling for `sequence_carrier_reports_its_total_not_its_alignment`)
  are still open and still correct. They are unrelated to either failing
  assertion, and a blind edit to the FFI type builder is the single most
  dangerous change available in these three files.
* **It did not implement layout or descriptor `toString`/`equals`,** because
  they are unreachable from these files (§6.3, §6.4) — that is NOM-2, not an
  omission.
* **It did not evaluate G6-1's NOM-1 by running it.** `test_utils.rs` is not
  this lane's file and `cargo test` was forbidden, so the PREDICTION that the
  two F35 heap tests are red or vacuous remains a prediction. This lane
  sidestepped it the same way G6-1 did: every new test is built on the H2 alias
  carrier via `pe_of_array_alias`, never on `make_real_heap_segment`.
* **It did not change `native_override.rs`'s force-route list.** G6-1's NOM-4
  asymmetry (`asSlice` routed, `maxByteAlignment`/`heapBase`/`toArray`/
  `elements`/`spliterator` not) **does not bear on either failure here**:
  `scope` and `ofArray` are both ON that list (confirmed by `owns_slot=true`
  with non-zero `invocations` in §5), and `FunctionDescriptor` is not a
  `MemorySegment` method at all. Checked, and it is a non-issue for G19.
* **It did not remove `panama.rs`'s dead `register_pe_function_descriptor`.**
  It has a wrong `argumentLayouts` signature (§1.3) and never runs in a
  shipping binary. Deleting it is right and is NOM-1 below rather than a
  drive-by, because the `--synthetic-jdk` feature build is the one thing this
  lane cannot even parse-check.
* **It did not measure the performance cost** of the extra session resolution
  now on the heap `get`/`set` path (`pe_segment_check_scope` resolves and reads
  a session where it previously returned early on `Object(None)`). It is four
  field reads and one memoised integer compare per access, and it is a
  correctness requirement — a heap segment whose scope is real must be checked
  — but nobody has timed it.

---

## 8. NOMINATIONS

**NOM-1 — `native-builtins/src/panama.rs` is this lane's file, but the fix is
not.** `register_pe_function_descriptor` (`panama.rs:5162`) registers
`argumentLayouts ()[Ljava/lang/foreign/ValueLayout;`, the pre-JDK-22 preview
signature (§1.3). It runs only under `--synthetic-jdk`. Either correct it to
`()Ljava/util/List;` to match `foreign_ffm`'s new row, or delete the registrar
— but whoever does it must be able to build the `synthetic-jdk` feature, which
this lane could not.

**NOM-2 — `vm/src/runtime/interpreter/native_override.rs`.** `toString`,
`equals` and `hashCode` on `java/lang/foreign/MemoryLayout`,
`java/lang/foreign/ValueLayout$Of*`, `java/lang/foreign/AddressLayout` and
`java/lang/foreign/FunctionDescriptor` resolve to `java.lang.Object`'s concrete
bodies, so **no registration in `panama.rs` or `foreign_ffm.rs` can ever serve
them**. The consequences are measured in §6.3 and §6.4: layouts print an
identity hash where the oracle prints `j8`, and structural equality is
identity. The identity hash is a cross-VM **diff hazard** — a vector that
prints a layout can never be compared. Force-routing those three names for
those four classes is the only place this can be fixed, and
`p67_layout_render` already produces the oracle's output for nine shapes.
Do not do it without the registry dump.

**NOM-3 — `native-builtins/src/panama_libffi.rs` (owned, deliberately
untouched).** G6-1's NOM-2 and NOM-3 stand unchanged: `layout_to_ffi_type`'s
`LAYOUT_SEQUENCE` arm should prefer the stored count at slot 4 when
`object_num_fields(layout) > 4`, and
`sequence_carrier_reports_its_total_not_its_alignment` should gain a five-slot
sibling. Restated so they are not lost between records.

**NOM-4 — `native-builtins/src/test_utils.rs`.** G6-1's NOM-1 is unevaluated
and should be applied before anyone trusts
`heap_alignment_is_enforced_the_way_the_oracle_enforces_it` or
`heap_refusals_use_the_oracles_exception_classes`. Nothing in this lane depends
on it, and nothing in this lane fixes it.

---

## 9. What the orchestrator must check at build time

1. `cargo build` — three new registrations, two new arms, one `pub(crate)`
   visibility change, five new tests, one new `#[cfg(test)] mod` in
   `foreign_ffm.rs`. None of it has ever been compiled.
2. `cargo test -p cratonvm-native-builtins panama` and `… foreign_ffm` — five
   new tests (§4).
3. Re-run, in this order, and compare against the numbers in §1.6 and §2.5:
   * `RJdkForeign` → expect `checks=75`, `steps=7`, `PASS`, and the
     `CK RJdkForeign layouts struct=16` line that is missing today. **Needs
     `--enable-native-access=ALL-UNNAMED`;** without it every downcall raises
     `IllegalCallerException` and the vector reddens for the wrong reason.
   * `RForeignLayoutJdkInterfaces` → expect `checks=172` and `PASS`.
   * `RForeignLayoutCollections` (42 checks) and `RDirectBufferElem`
     (506 checks) — both MEASURED **green before** this lane. Neither source
     file contains the string `MemorySegment` or `java.lang.foreign`
     (`grep -c` = 0 for both), so the surface is disjoint; re-run them anyway.
4. `--dump-native-registry` for
   `(java/lang/foreign/FunctionDescriptor, returnLayout, ()Ljava/util/Optional;)`
   and `(…, argumentLayouts, ()Ljava/util/List;)`: confirm `owns_slot=true`,
   `registered_by` is `foreign_ffm.rs`, and `invocations` is non-zero under
   `RJdkForeign`. If either is `invocations=0` while the vector is green, the
   green came from somewhere else and this record is wrong.
5. Zero CR bytes in all three owned files (verified: `tr -cd '\r' < f | wc -c`
   is 0 for each).
