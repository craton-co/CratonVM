# Two VMs in one process corrupt each other's field reads: the compact-layout registry is keyed on a per-VM ClassId

**Status: OPEN, root cause PROVEN, deterministic reproducer landed
(`#[ignore]`d). Two candidate fixes were implemented and measured, and BOTH
are insufficient — see "What does not work". The fix that remains is
architectural.**

Impact: any process holding two live `Vm`/`SharedVm` instances reads wrong
field values. Production embeddings today create one VM per process and are
unaffected; the in-process test fixtures are not, and the codebase explicitly
supports "a second `Vm` built in the same process (a test fixture, or a real
embedding)".

## Mechanism

`ClassStore` issues ids as `ClassId::new(self.classes.len())`
(`classloading/src/class.rs`) — a per-VM index, so **every VM restarts from 0**.

The compact field-layout registry (`types/src/field_layout.rs`) is
**process-global** and keyed on `class_id` alone: the dense `CLASS_LAYOUTS`
array by `class_id`, `CLASS_LAYOUT_VERSIONS` by `(class_id, field_count)`. And
`register_class_layout` OVERWRITES.

So two live VMs that each define a one-field class collide on one entry, last
writer wins, and both then decode their objects through the other's storage
kinds and offsets. `compact_object_field_storage` is on every field read, and
`alloc_object` uses the same lookup to decide an object's shape.

The registry's own doc comment states the assumption being violated:

> The registry is slot-indexed and ClassIds are monotonic

They are monotonic *within* a VM. That is not the same property.

## Evidence

`vm::vm_exec::tests::two_vms_must_not_share_a_compact_layout_for_the_same_class_id`
reproduces it **deterministically**, no concurrency required: VM A defines a
one-field `F` class, VM B a one-field `D` class, both land on the same id, and
A's `Float(3.25)` reads back as `Double(0.0)`.

Before that reproducer existed this presented as a ~4% full-suite-only flake
whose symptoms looked unrelated to each other. All of them are this bug:

| Test | Observed | Why |
|---|---|---|
| `box_float_value` | `Float(3.25)` → `Int(1078984704)` | `0x40500000`, the same four bytes through the wrong storage kind |
| `t19_h6_cas_field_double_field_roundtrip` | `Double(2.5)` → `Double(0.0)` | wrong offset/width; 2.5's low word is zero |
| `t19_h6_cas_field_long_field_with_double_expected_arg_succeeds` | `Long(1)` → `Long(0)` | wrong offset |
| `read_thread_task_object_prefers_jdk17_direct_target_field` | `Some(ObjectRef)` → `None` | reference slot read where memory is zero |

Two hypotheses were tested and **falsified**, both by instrumentation added to
the failing assertions rather than by argument:

* **Not the field-descriptor memo.** It resolved correctly (`Some('D')`) in the
  failing run. Its thread-local tiers are keyed on `vm_identity`, a monotonic
  `NEXT_VM_IDENTITY` counter that is never reused, so nothing bleeds between
  VMs.
* **Not the collector.** The failing run reported `minor GCs so far in this VM:
  0`. Nothing moved or was reclaimed.

## What does not work

Both of these were written, compiled and measured. Neither is a fix.

**1. Stop publishing layouts once a second VM exists** (a one-way latch tripped
in `SharedVm::new`). Correct in principle — compact layouts are an
optimisation and `alloc_object` falls back to tagged 16-byte slots that carry
their own type — and it does make the reproducer pass. But in the test binary
the second VM appears almost immediately, so compact layouts are disabled for
essentially the whole suite: `test_hprof_instance_dump_reads_compact_ref_field_value`
fails **30 of 30 runs**, and a core representation stops being exercised at
all. Trading a correctness bug for a coverage collapse is not a fix.

**2. Refuse a registration that conflicts with the incumbent** (keep the first
layout, reject a structurally different one for the same key). This fixes the
FIRST VM — its objects keep decoding correctly — and does nothing for the
second: `alloc_object` still *finds* the incumbent layout for the colliding id
and allocates VM B's object compact under VM A's shape. The corruption simply
moves: the reproducer then fails in the other direction, `Double(2.5)` read
back as `Float(0.0)`.

The lesson from both: **refusing to publish cannot help, because the LOOKUP is
what is ambiguous.** `compact_object_field_storage(header, index)` and
`compact_object_body_size(class_id, n)` receive only `class_id` and a field
count, and no VM. Any scheme that privileges "the first VM" also breaks every
test that needs a compact object in a later one.

## The fix that remains

Make the layout domain explicit: key the registry on `(vm_domain, class_id)`
and thread the domain to the lookup. `Heap` belongs to exactly one VM and is
the receiver of `alloc_object`/`get_field`/`set_field`, and the GC scans are
per-heap, so the domain can travel on the `Heap` rather than being widened into
every `ObjectHeader`.

Cost and care:

* The dense `class_id`-indexed array exists for speed — `class_layout_for_fields`
  once measured **37.8% of all samples** on `BinTreesClassic d=18`. Per-domain
  dense arrays keep that; a `HashMap<(u32, u32), _>` on the hot path does not.
* Call sites to convert: `compact_object_field_storage`,
  `compact_object_body_size`, `class_layout_for_fields`, `with_class_layout`,
  `compact_field_slot`, `compact_oop_scan` (`gc/src/heap.rs`), plus the
  `VERSION_CACHE` thread-local, whose key must gain the domain too.
* `LAYOUT_GENERATION` and `layout_replace_counts` are also global and will need
  the same treatment or an explicit argument that they are safe shared.

Alternative considered and rejected as riskier: make ClassIds process-unique by
giving each VM a base offset. Ids are used directly as indices in many places,
and unbounded growth collides with `MAX_DENSE_CLASS_LAYOUTS` (1 << 20) unless
bases are recycled on VM drop.

## Interim state

The five test-hygiene flakes in this suite are fixed separately (`bfe5305cc`,
`ad828b8a1`); this defect is the residue. It surfaces as roughly 1 full-suite
run in 25. Until it is fixed, a `vm --lib` red naming
`box_float_value`, `box_double_value`, either `t19_h6_cas_field_*` test or
`read_thread_task_object_prefers_jdk17_direct_target_field` is very likely this
bug rather than a regression in the code under test — check whether the values
differ only by tag or offset before investigating further.
