# Compiled `ldc` re-derived its constant on every execution — FIXED 2026-08-20, and only half of it was worth the diff

**Status: FIXED.** Both compiled `ldc` helpers answer from JVMS §5.4.3's
recorded resolution now, default-ON, with
`CRATONVM_JIT_COMPILED_LDC_CONST_CACHE=0` to opt out.

**Read the verdict before the diff.** The two halves of this change did not
land the same way, and the page it retires predicted only one of them:

| rung | HotSpot 25 | before | after | verdict |
|---|---:|---:|---:|---|
| `ldc SomeClass.class` | 0.3 ns | 65.7 ns | **16.2 ns** | **4.05x** |
| `ldc "literal"` | 0.2 ns | 16.6 ns | **16.6 ns** | **no change** |
| `NettyZipBombPhases snappy 8` | 509 ms | 15 538 ms | 15 468 ms | wash |

The class half is the win. The string half — which is most of the diff, and the
only part that needed a helper-ABI addition — bought **nothing measurable**, and
the predecessor page's cost table is why it looked like it would.

**Reproducers:** `probes/LdcConstCostProbe.java` (cost),
`probes/LdcConstCacheOracle.java` (correctness: identity, GC survival, and that
a failed resolution is still raised every time).

## What the predecessor page got right

`jit_ldc_class_cp` did a full loader-faithful resolution and a mirror-map lookup
on every execution, it already received the recorded store's exact key
(`holder_class_id`, `cp_idx`), and the probe belonged at the top of the helper.
All three were correct, and that is the 4.05x.

## What it got wrong, and why the mistake was reasonable

The page priced `ldc "literal"` at 18.4 ns and attributed it to
`create_java_string`'s pool — "a read lock, a hash of the literal's whole
content, and a `memcmp`" — with a `perf` cluster to match
(`create_java_string` 2.42–4.09 % self, `__memcmp_evex_movbe` 1.44 %,
`HashMap<String, ObjectRef>::get::<str>` 1.10 %). That cluster is real. The
inference from it — that replacing the pool lookup with a `(class id, cp index)`
lookup would make the row cheaper — is not, and the measurement says so:

    ldc-string   merge-base 16.4 / 17.0 / 16.6 ns
    ldc-string   branch     16.4 / 16.9 / 16.6 ns

**Both lookups cost the same, because neither of them is the cost.** ~16.5 ns is
what a `ldc` site pays to leave compiled code at all: the `CALL` through the
helper slot, `note_jit_boundary`, one global `RwLock` read and one hash. Swapping
which map is read underneath does not move a number made of the machinery around
it. The class rung was 4x worse only because it did a full resolution *on top of*
that same floor, and removing the resolution is what removed 49 ns.

The lesson is the one this tree keeps relearning in a new costume: a profile
names what is *executing*, not what is *marginal*. `create_java_string` really
was 4 % of samples; making it free would still have left the row at ~16 ns,
because the pool lookup and the record lookup are the same size and the sampling
could not tell either from the call that reaches it.

## The measurement

Azure host 2 (Linux x86_64, 8 cores, JDK 25), release, load 1.75–1.81 recorded
beside every arm, three interleaved rounds. `LdcConstCostProbe` runs 16 copies
of ONE opcode per iteration and differences against a loop with 16 fewer, so the
number is the marginal cost of the opcode.

| arm | `ldc-string` | `ldc-class` | *ctrl* `iadd` |
|---|---:|---:|---:|
| HotSpot 25 | 0.2 / 0.2 / 0.2 | 0.4 / 0.3 / 0.3 | 0.2 |
| merge-base `eadd845c4` | 16.4 / 17.0 / 16.6 | 64.8 / 67.7 / 65.7 | 0.5 |
| branch, cache ON | 16.4 / 16.9 / 16.6 | **15.9 / 16.6 / 16.2** | 0.5 |
| branch, cache OFF | 32.7 / 32.6 / 33.5 | 65.1 / 65.7 / 66.9 | 0.5 |

The `iadd` control is 0.5 ns in every CratonVM arm, which is what says the rows
above it are the opcodes and not the host.

### The kill switch cannot A/B the string rung, and the OFF row shows it

`CRATONVM_JIT_COMPILED_LDC_CONST_CACHE=0` reproduces the pre-change behaviour
for `ldc <Class>` exactly — 65.1–66.9 against the merge base's 64.8–67.7 — and
that agreement is what licenses the same-binary comparison for that rung.

It does **not** reproduce it for `ldc <String>`, and 32.6–33.5 against the merge
base's 16.4–17.0 is the proof. The reason is structural: the site is CP-indexed
now, so the literal's bytes are no longer baked anywhere, and with the record
switched off the helper must re-read the constant pool under the class-manager
lock on every execution. That is a configuration that has never shipped and
never will — strictly worse than either real one. **The string row is therefore
answerable only against the merge-base binary**, which is why one was built and
interleaved with the other two arms rather than quoted from a previous session.

## The counter the predecessor page asked for

> Anyone acting on this should add a counter to `jit_ldc_string` first and read
> it, rather than trusting either profile's chain.

Done. `CRATONVM_DBG_FIELD_SITE=1` reports a `jit-ldc` triple beside the
interpreter's, on `NettyZipBombPhases snappy 4`:

| arm | compiled `ldc` |
|---|---|
| cache ON | `jit-ldc: hit=23069877 miss=0 fill=0` |
| cache OFF | `jit-ldc: hit=0 miss=23069900 fill=0` |
| interpreter, same run | `ldc: hit=13934 miss=5561 fill=5561` |

Three things the counter settles that the profile could not:

* **23.07 M compiled `ldc` executions per 4 MiB** — the rate really is that
  high, so the attribution by elimination was right even though the DWARF chain
  that accompanied it was not.
* **`miss=0 fill=0` with the cache on.** The compiled helper never takes its
  cold arm on this workload: the interpreter has already resolved every site
  during warm-up (`ldc: fill=5561`) and both routes read one store. That
  sharing is a consequence of using the interpreter's store rather than a
  private one, and it is worth more than it looks — a private compiled cache
  would have paid 5 561 cold resolutions over again.
* **`fill=0` with the cache off**, which is the switch gating the WRITE and not
  only the read. It did not, at first: the bump sat at the call site, past the
  gated recorder, and reported `fill=4796969` on a run that recorded nothing.
  A counter that names the wrong fact is the same defect as one that never
  fires, and this one named the exact fault the switch exists to avoid.

## Correctness

`probes/LdcConstCacheOracle.java`, byte-identical across four arms — HotSpot 25,
CratonVM cache-ON, CratonVM cache-OFF, and CratonVM under **ZGC**, which is the
arm that matters because the record holds a live reference a moving collector
must remap:

* string identity: two `ldc`s of one CP entry, two entries with equal text in
  different classes, `LITERAL == LITERAL.intern()`, and
  `LITERAL == new String(TEXT).intern()` — all `true`, all read back out of
  compiled code;
* class identity: same site twice, across classes, and against
  `Class.forName`;
* **survival of a moving collection**: every value re-read after two forced
  collections and compared against the pre-collection reference, which is the
  comparison that fails if the store is remapped and the caller's copy is not,
  or the reverse;
* **a failed resolution is raised every time** — three attempts, three
  exceptions. JVMS §5.4.3 requires the error on each attempt, so a failure is
  deliberately not recorded.

### One correction to the predecessor page's prescription

> Both must keep what the interpreter fix keeps: … the value stored is a
> **global-root handle** rather than a raw `ObjectRef`.

The store does not hold handles. `MemberResolver::record_constant` files a plain
`Value::Object(Some(ObjectRef))`, and what makes that safe is that the
**collector scans and remaps the store** (`for_each_condy_root` /
`update_condy_refs`) as roots of that VM — which is also why the API is
`VmScoped`, so a value from another VM's heap cannot be filed against the wrong
heap. The distinction matters to anyone extending this: the reference is safe
because the GC owns it, not because it is indirected.

## What the change actually is

* **`jit_ldc_class_cp`** — probe at the top, record after
  `get_or_create_class_mirror`, failure not recorded. No ABI change; it already
  had the key.
* **`jit_ldc_string_cp`** — a new `helpers.ldc_string_cp` slot (ABI v7, appended;
  `ldc_string` stays because the table is append-only, and nothing calls it).
  The site is CP-indexed end to end: `JitLdcConstant::String { holder_class_id,
  cp_idx }`, `ldc_string_info: Vec<(usize, u32, u16)>`,
  `Op::ConstString { holder_class_id, cp_idx }`, both backends emitting through
  the same `emit_ldc_class_cp_stub` the class site uses.
* **The SATB flush moved to the cold arm** of both helpers. A recorded hit
  allocates nothing, reaches no safepoint and stores no reference, so it has
  nothing to flush; `note_jit_boundary` stays on both arms because the
  per-thread scan cache is invalidated by crossing the boundary.
* **Three consequences** worth naming: the `ir_ldc_strings` / `owned_jit_strings`
  keep-alives are gone with the bytes they kept alive; an unwired
  `ldc_string_cp` makes both backends REFUSE a string-`ldc` site rather than
  `CALL` address 0; and the CP-indexed form can fail where the bytes form could
  not, so its site takes the same zero-test-and-pending-exception convention
  `ldc <Class>` takes.

### Why the string half was kept despite measuring flat

It is a wash on time, so the case for it is not time:

* it puts both `ldc` kinds on one mechanism and one key, so the next person
  changing either does not have to discover that they differ;
* it removes a per-artifact lifetime hazard — compiled code no longer bakes the
  address of a `Box<str>` the `CompiledMethod` must outlive;
* it is the prerequisite for the only remaining fix (below), which needs the
  site to *have* a key.

Reverting it would restore two mechanisms and a keep-alive to buy back 0 ns.

## What is left open

**~16 ns against HotSpot's 0.2**, and it is now the same 16 ns for both tags,
because what remains is the floor rather than either lookup: a `CALL` out of
compiled code, `note_jit_boundary`, a global `RwLock` read and a hash.

HotSpot pays 0.2 ns because it does not look anything up — it bakes the oop into
the code and the collector **patches the code** when the object moves. This VM
has no mechanism for a GC-visible oop embedded in an artifact, which is exactly
why `jit_ldc_string`'s original doc comment gave "the pool is rewritten after a
moving collection" as the reason to re-consult per execution. That mechanism —
a relocatable oop slot per `ldc` site, in the `CompiledMethod`, in the
collector's root set — is the remaining 80x, and it is a collector change rather
than a JIT one. It is not attempted here.

A cheaper intermediate that is NOT worth taking without measuring first: moving
the condy map out from under the shared `ClassRealm::resolution_cache` lock. The
lock is read-mostly and readers do not contend with each other, which is
consistent with the workload measuring flat, so there is no evidence it is
costing anything.

## The workload

`NettyZipBombPhases snappy 8`, G1, **ABBA** ordering (old, new, new, old) so a
monotone drift in host load cancels between the arms instead of accruing to
whichever runs later, six runs per arm:

| arm | totals (ms) | mean |
|---|---|---:|
| merge-base | 16 331 · 14 930 · 15 332 · 15 534 · 15 417 · 15 682 | 15 538 |
| branch | 15 431 · 15 292 · 15 371 · 15 539 · 15 875 · 15 301 | 15 468 |

−0.45 %, against a run-to-run spread of 1 401 ms in the old arm. **A wash**, and
predictably so: this workload's `ldc` traffic is string literals
(`ObjectUtil.checkPositive(increment, "increment")` inside `RefCnt.retain0`),
which is the half that did not move.

An earlier two-round, non-ABBA pass of the same comparison read as a consistent
+2 % REGRESSION (15 175 → 15 463, 15 324 → 15 651). It was noise; the ABBA rounds
put both arms inside one spread. Two rounds in a fixed order is not enough to
call a 2 % effect on a 15-second workload, in either direction.

## Not to be confused with

* `interpreter-ldc-re-derived-its-constant-and-never-interned-the-wide-literals-FIXED-20260818.md`
  — the interpreter half, fixed two days earlier, whose store this adopts.
* The surrogate-literal interning half of that page is **not** duplicated here,
  and the question it left open is now answered: compiled code cannot `ldc` a
  lone-surrogate literal at all. Every `ldc` resolver guards on
  `get_utf8_wide(..).is_none()`, so such a site returns `None` from
  `cp_ldc_resolver` and takes the RBC.7 *permanent* bail — the enclosing method
  is never compiled. Unchanged by this fix, and unchanged by the CP-indexing:
  the guard is at the resolver, not at the helper.

## Related

* `httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md` —
  where the cluster was measured, and whose snappy phase is the workload above.
