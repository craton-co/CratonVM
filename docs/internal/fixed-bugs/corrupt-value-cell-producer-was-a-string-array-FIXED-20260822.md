# The corrupt `Value` cell's producer was a `String[]`, and no collector was involved

**Status: FIXED 2026-08-22.** Both halves. The crash was closed on 2026-08-20 by
giving all four heap readers one screened implementation; the PRODUCER that
page left open is closed here, and it is not a GC defect at all.

---

## 1. What the page said was still open

> Something hands `StringBuilder.append(Object)` a receiver whose memory has
> already been swept and reused. […] the fault is on the `java/lang/String` fast
> path, which has no allocation between entry and the read, so the reference is
> already stale when the native is entered, which points upstream of this
> function rather than at it.
>
> Next step is to find the producer, not to widen the guard.

The reasoning was sound and the conclusion was wrong in one specific way: the
reference was never stale, and **the read was never of a String**.

## 2. The instrument that settled it

The collector's guard can name the CELL — its address and the two words it holds
— and nothing else, because `heap::read_value_cell_checked` runs in the
collector crate and cannot see a Java frame. `CRATONVM_DBG_CORRUPT_CELL` adds
the other half on the VM side of the same read, where the receiver and the
owning thread's frames are both in hand
(`heap::corrupt_cell_hits`/`corrupt_cell_last` publish the hit;
`reclaim_guard::report_corrupt_cell_producer` reports it). One relaxed load per
`NativeContext::get_field` while armed, nothing at all while it is not.

Armed on the fixture, one run:

```text
the RECEIVER of the read that decoded a corrupt Value cell.
  obj=0x20040a803f8  slot_index=0
  raw0=0x0000020040775828  raw1=0x0000020040797b78
  receiver_class=java/lang/String   receiver_kind=Array   receiver_fields=2
  in_heap=true  in_young=false  collections_now=0
  holder=frame#69 Metadata$MetadataItemCondition.withDefaultValue pc=36 local[1]
  top_frame=Metadata$MetadataItemCondition.createDescription pc=110
```

Three fields end the investigation:

* **`collections_now=0`.** No collection has happened. Nothing was swept,
  nothing was re-served, and every sentence on the old page about a
  "swept-then-reused object" is describing something that did not occur.
* **`receiver_class=java/lang/String` with `receiver_kind=Array`.** Those two
  cannot both describe an object. They can both describe an ARRAY: a reference
  array has no class of its own in this VM's class store, so it carries its
  COMPONENT's class id.
* **`holder=… withDefaultValue local[1]`.** The fixture's
  `ItemMetadata.newProperty("e", …, new String[] { "y", "n" }, null)`.

So `sb.append(Object)` was handed a `String[]`, its `class_name == "java/lang/String"`
fast path matched, and it read slot 0 as `String.value` — which is the array's
FIRST ELEMENT POINTER, decoded as the `(tag, payload)` pair of a `Value` cell.
Both raw words are heap pointers because both are elements.

## 3. MEASURED, and it needs no fixture at all

`probes/AppendArrayProbe`, default heap, no GC:

```text
sb.append(new String[]{"y","n"})
CratonVM   ""   + `gc::guard: corrupt Value cell`
                + NullPointerException: Cannot read the array length
                  because "this.value" is null
HotSpot    "[Ljava.lang.String;@<hash>"
```

`String.valueOf((Object) arr)` is worse: its fast path returns the RECEIVER as
the answer (`String.toString()` is the identity), so the array escapes the
native AS a String and every later `.length()` on it faults.

## 4. Four doors, three of them missing the same check

| door | site | had the array check |
| --- | --- | --- |
| `sb.append(Object)` / the units renderer | `invoke_to_string_units_opt` | **no** |
| `String.valueOf(Object)` | `native_string_value_of_object` | **no** |
| `sb.append(CharSequence)` | `charsequence_fast_units` | **no** |
| `sb.insert(int, CharSequence)` | `native_sb_insert_charsequence` | **no** |
| the boxed-wrapper fast path | `invoke_to_string_units_opt`, 20 lines below the first | **yes** |
| the VM-side predicate | `vm_exec::is_string_object` | **yes** |

The wrapper path directly below the first door states the rule in its own words
— *"MUST exclude arrays: a heap array's `num_slots` is its LENGTH"* — and the
String path above it did not have it. That is the whole defect: one rule, six
places, four of which knew it.

`is_plain_string` is now the single home for it and all four doors call it. An
array falls through to `invoke_virtual("toString")`, i.e. `Object.toString()`,
which is the identity rendering HotSpot gives.

## 5. What stays from the original page, unchanged and still true

* **The crash was four readers and one screen.** `gen_heap::read_slot` screened
  the discriminant since `HIB-CV-32`; the shared `Heap`, G1 and ZGC readers did
  not, and ZGC's was additionally a non-atomic `ptr::read`. All four share
  `heap::read_value_cell_checked` now. The Generational arm's PASS really was
  masked, not clean.
* **The Linux confirmation.** Rebuilt on the Azure host at `509710ba8`, same
  class, same crash, same collector passing — so none of it was a Windows
  artefact.
* **"A run that is quiet here is not a run that is correct."** Still true: the
  guard reports only cells whose discriminant lands out of range, and a
  `String[]` whose first element happens to form a valid `Value` tag would have
  been invisible to it. That is why the fix is at the four doors and not at the
  guard.

## 6. What it got wrong, and why the trail led there

* **"A swept-then-reused object."** Both raw words being heap pointers into the
  same arena is consistent with a two-reference payload, and the page inferred
  a recycled block from it. It is equally consistent with a two-element
  reference ARRAY, which is what it was. `collections_now` was the field that
  could tell them apart and nothing was printing it.
* **The `NativeHandleScope` asymmetry in `native_sb_append_object`** — `this`
  rooted, `args[1]` read raw — was named as "the obvious suspect". It is a real
  asymmetry and it is not this defect; the page said as much and was right to.
* **`--nojit` and `CRATONVM_COMPACT_REF_FIELDS=0` both still crashed.** Both
  controls were correct and both were consistent with the real cause, which is
  neither JIT nor layout.

## 7. Gate

`RStringBuilderContent`, new group *"an ARRAY is not a String, whatever its
component type is"*: eight array shapes (`String[]`, `String[0]`, `String[1]`,
`Boolean[]`, `Object[]`, `String[][]`, `int[]`, `char[]`) each asserted through
all four doors, plus `Object.toString()`, with the four required to AGREE — so
one door regressing is visible where four matching wrong answers would not be.
Asserted on SHAPE (the descriptor prefix), never on the identity hash, so
nothing depends on the host.

Four negative controls keep the fast paths the doors exist for: `append`,
`valueOf`, `insert` over a real String, and `String.valueOf((Object) s) == s`
by IDENTITY, which is the property the `valueOf` fast path is there to preserve.

It FAILS on the pre-fix binary — `String[] via append(Object): expected
[[Ljava.lang.String;@4af] got []` — and PASSES on HotSpot, 86 checks.

## 8. Left open — a NOMINATION, not a residual of this defect

`class_id_of_object` answering the COMPONENT class for a reference array is a
property of this VM, not a bug, and every `class_name == "java/lang/String"`
test in the tree is a candidate for the same mistake. The four on the rendering
path are fixed and gated. Three more read String slots off that test in
`vm_exec`'s annotation-proxy renderers (`annotation_value_to_string`,
`annotation_value_hash`, `annotation_value_equals`); an annotation member whose
value is a `String[]` reaches them, and none was measured here. Worth one probe.

**And one observation this page cannot absorb.** The two-arm Spring Boot sweep
that verified this fix (1975 classes per arm, Windows, 2026-08-22) shows the
`JsonMarshallerTests` hit GONE — which is this fix working — and one hit in a
different class, `KafkaMetricsAutoConfigurationTests`, whose `raw0` decodes as
the ASCII text `"t/Proxy\0"` rather than as two heap pointers. Different shape,
different producer, and it did not reproduce in 36 targeted runs per binary with
the instrument armed. It is filed separately and OPEN as
`known-issues/gc/corrupt-value-cell-one-unreproduced-hit-in-kafkametrics-20260822`.
Its most actionable line is about the instrument added here: that read did NOT
come through `NativeContext::get_field`, which is the only door
`CRATONVM_DBG_CORRUPT_CELL` watches, so widening it to the interpreter's own
`getfield` is the cheap next step.

## Reproduce (on a pre-fix binary)

```bash
cratonvm --java-home "$JDK" -cp . AppendArrayProbe
```

No heap flag, no collector flag, no fixture. The Spring Boot witness —
`configuration-metadata/spring-boot-configuration-processor`,
`JsonMarshallerTests.marshallAndUnmarshal` — reaches the same line through
`ItemMetadata.newProperty(.., new String[]{"y","n"}, ..)` and AssertJ's
`createDescription`, and fires the guard exactly once per run.
