---
name: bytebuffer-accessor-gap-20260912
description: >
  Why java.nio.ByteBuffer scalar accessors run 100-1000x HotSpot. THREE
  independent causes, separated by measurement. (1) FIXED - the thin
  ByteBuffer.get/put(int) helper serves DIRECT receivers only, and its call
  sites were on the method-level `is_intrinsic_site` refusal list, so one
  `buf.get(i)` anywhere in a method cost that WHOLE method the optimizing tier
  while the helper declined every heap call anyway: 574 ns against 53 ns.
  (2) FIXED - the *Unaligned natives read the backing byte[] with one virtual
  get_array_element call PER BYTE plus a Vec allocation. (3) OPEN and now
  measured - every multi-byte heap accessor makes TWO native funnel crossings,
  and one of them is `Buffer.session()` returning a constant null.
metadata:
  type: known-issue
  area: jit, nio, throughput
---

# `java.nio.ByteBuffer` scalar accessors are 100-1000x HotSpot — three causes, two closed

| | |
|---|---|
| **Status** | PARTIALLY FIXED — cause 1, cause 2 and the `session()` half of cause 3 closed; the `getIntUnaligned` crossing is what is left |
| **Result** | heap `get(int)` **568 -> 25 ns** (22.9x); every other accessor 1.5-3.1x; direct receivers unregressed on a monomorphic site (52 -> 49.53 ns, `served=1599999 declined=1`) |
| **Verified** | `ByteBufferAccessorMatrixProbe` 119/119 and `UnsafeUnalignedMatrixProbe` 75/75 lines byte-identical to HotSpot |
| **Severity** | high — it is the floor under every HTTP codec, zip/header reader and netty buffer path |
| **Opened** | 2026-09-12 |
| **Supersedes** | the accessor half of `internal/fixed-suite-bugs/springboot/zipcontenttests-bytebuffer-accessor-call-cost-RETIRED-20260810.md`, whose numbers this page reproduces and whose proposed lever it does **not** re-litigate |

## The measurement

`probes/NioBufferCostProbe.java` (new) prices every scalar and bulk shape on
both buffer kinds, with a locals-only control arm and a raw `byte[]` arm so the
loop overhead and the array access are visible separately rather than folded
in. Windows 11, 24C/32T, JDK 25.0.3 both sides, min of 2 rounds x 1 000 000 ops,
`--real-jdk` (the default).

`before` is dev at 3e25a17f0; `after` is that plus causes 1 and 2. One box, one
HotSpot column measured in the same session as `after`.

| arm | HotSpot | before | after | gain | ratio now |
|---|---:|---:|---:|---:|---:|
| control (locals only) | 0.21 | 1.92 | 1.49 | 1.3x | 7x |
| raw `byte[]` get | 0.25 | 3.94 | 1.58 | 2.5x | 6x |
| **heap `get(int)`** | 0.52 | 568.51 | **24.78** | **22.9x** | 48x |
| heap `getShort(int)` | 0.29 | 963.71 | 434.94 | 2.2x | 1500x |
| heap `getInt(int)` | 0.58 | 1006.69 | 455.47 | 2.2x | 785x |
| heap `getLong(int)` | 0.34 | 1190.00 | 451.97 | 2.6x | 1329x |
| heap `getInt(int)` BE | 0.58 | 1035.65 | 538.09 | 1.9x | 928x |
| heap `getShort()` rel | 1.08 | 929.16 | 522.13 | 1.8x | 483x |
| heap `getInt()` rel | 1.24 | 971.31 | 584.19 | 1.7x | 471x |
| heap `putInt(int,int)` | 0.42 | 1053.08 | 556.09 | 1.9x | 1324x |
| heap `putLong(int,long)` | 0.28 | 1194.28 | 611.97 | 2.0x | 2186x |
| direct `get(int)` | 0.30 | 117.55 | *194.44* | *0.6x* | 648x |
| direct `getShort(int)` | 0.29 | 376.97 | 202.16 | 1.9x | 697x |
| direct `getInt(int)` | 0.58 | 439.28 | 184.76 | 2.4x | 319x |
| direct `getLong(int)` | 0.32 | 454.13 | 227.06 | 2.0x | 710x |
| direct `getInt()` rel | 1.09 | 791.57 | 523.34 | 1.5x | 480x |
| direct `putInt(int,int)` | 0.39 | 520.90 | 195.99 | 2.7x | 503x |
| heap `get(byte[256])` | 38.91 | 4967.10 | 1900.22 | 2.6x | 49x |
| direct `get(byte[256])` | 64.29 | 2435.91 | 1283.42 | 1.9x | 20x |
| heap `put(byte[256])` | 30.13 | 3566.43 | 1146.42 | 3.1x | 38x |
| direct `put(byte[256])` | 37.20 | 5937.31 | 2171.74 | 2.7x | 58x |

**The array access itself is fine at 6x.** Everything above it is call and
crossing cost, and it separates into three causes that have nothing to do with
each other.

**The one row that moves the wrong way, and why it is the right trade.**
`direct get(int)` goes 118 -> 194. This probe calls ONE method with BOTH buffer
kinds, so its `get(int)` site is bimorphic, and the interpreted window that
fills the receiver profile is heap-dominated — so the gate refuses the bind and
the direct half loses the fast path. That is the policy working, not failing:
the same site's heap half goes 568 -> 25, and the pair goes 686 ns -> 219 ns
together. Refusing is also the better default at a genuine 50/50 site, from the
trade table below: bound averages (237+52)/2 = 145 ns, unbound (18+135)/2 = 77.
A site whose receiver really is mostly direct keeps its bind — that is what
`probes/DirectBufferShapeControlProbe.java` measures, and it reads 49.53 ns
with `served=1599999 declined=1`.

## The control that made this tractable

`probes/HeapBufferShapeControlProbe.java` (new) is a hand-written clone of
`HeapByteBuffer.get(int)` —
same abstract-base/concrete-subclass shape, same `get -> checkIndex -> ix ->
array load` chain, in the application class loader. Same bytecode shape, and:

| | JIT, before | JIT, after | `--nojit` |
|---|---:|---:|---:|
| hand-written clone | 34–53 ns | 14.56 | 1921 |
| `HeapByteBuffer.get(int)` | **461–600 ns** | **18.41** | 2836 |

Interpreted they are 1.5x apart, which is the extra call and `getstatic` in the
JDK's version. Compiled they were **13x** apart, and are now 1.3x — i.e. the
JDK's accessor is back to costing what the same code shape costs when you write
it yourself, which is what said the gap was never in the accessor. So the cost is not the code
shape and not the interpreter: the JIT is an order of magnitude less effective
on this one chain, and that is a compiler-admission question, not an accessor
question. Everything below follows from asking why.

## Cause 1 — one `buf.get(i)` cost the WHOLE enclosing method the optimizing tier

**FIXED 2026-09-12.**

`CRATONVM_DBG=ir-compiles` answers it in one line:

```
[ir] invoke-plan HeapBufferShapeControlProbe.armJdk: NO invoke_info — pc=21: java/nio/ByteBuffer.get(I)B
     is a loop-shaped or memory call-site intrinsic; the IR tier has no node for it
[ir] IrBuilder::build returned None for HeapBufferShapeControlProbe.armJdk — no IR body
```

`java/nio/ByteBuffer.get(I)B` and `put(IB)Ljava/nio/ByteBuffer;` were rows on
the `is_intrinsic_site` predicate in `jit/src/lib.rs`'s eligibility loop. That
predicate's own comment states the trade it is making — *"a false positive costs
one method the optimizing tier, a false negative costs every call the
intrinsic"* — and for this family the trade was inverted, for two reasons that
compound:

* **The refusal is METHOD-level.** One `buf.get(i)` anywhere in an HTTP codec
  method takes that whole method off C2, including all the loops and arithmetic
  that have nothing to do with the buffer.
* **The thing being protected does not serve heap buffers at all.**
  `jit_dbb_get_byte_direct` reaches `dbb_direct_elem_addr`, which reads a direct
  buffer's `address`/`limit`/`isReadOnly` and requires
  `elem_fastpath::class_is_served`. A `HeapByteBuffer` receiver **always**
  declines, so every heap call paid the helper crossing, then the decline, then
  the generic funnel — worst of both — while the enclosing method was held out
  of the optimizing tier to buy it.

Measured with the existing kill switch, which takes the site off the helper
entirely and therefore also off the refusal list:

| | default | `CRATONVM_JIT_NIO_BYTE_DIRECT_HELPERS=0` |
|---|---:|---:|
| heap `get(int)` | 568 ns | **53** |
| direct `get(int)` | **118** | 393 |

So neither setting was right: the flag traded a 10.7x heap regression for a
3.3x direct one. The heap number lands on the hand-written clone's 40–53 ns,
i.e. with the method in the optimizing tier the JDK's chain is **no slower than
equivalent hand-written Java** and the accessor was never the problem.

### The fix

Both halves, because either alone is a trade rather than a win:

1. **The family is off the `is_intrinsic_site` refusal list**, so the enclosing
   method reaches the optimizing tier.
2. **The helper is bound at the OPTIMIZING door too** — the only thin-helper
   bind in that planner outside its `is_static || is_special` gate, which is
   sound here because the gate is a statement about what the *other* helpers can
   reach, not a soundness condition: the callee need not be known from the site
   (the helper's own prologue probes the receiver and declines to
   `jit_invoke_dispatch` for anything it is not certain of, exactly as the
   single-pass door already relies on), and `lower_data_node`'s direct-call arm
   already computes `has_receiver` from `invoke_kind` and admits `0 | 1 | 2`, so
   a virtual site's receiver is marshalled correctly with no lowerer change.
   A null receiver never reaches the helper at all: `emit_direct_cross_call`
   deopts on a null argument 0 and the interpreter owns the NPE.
3. **And only where it can win, at BOTH doors.**
   `nio_byte_element_bind_refused` refuses a site whose own profiled dominant
   receiver is one `elem_fastpath::class_is_served` says the funnel has never
   served — a heap buffer, in practice, since `dbb_get_abs` is registered on
   `java/nio/DirectByteBuffer` and nothing else, so no other class id can ever
   enter that table. A refused site keeps the ordinary inline cache, which
   reaches the *compiled* `HeapByteBuffer.get` — four bytecodes over a
   `byte[]` — and is the better path for it rather than a fallback.

   It refuses only on **positive evidence against**. No profile, or no receiver
   holding 80% of the site, binds — the behaviour every door had before this
   gate. `class_is_served` is a table the FUNNEL fills in, so it is empty until
   the interpreter has run the site, and a "prove it first" rule would refuse
   every bind on a method compiled early and never revisit it.

   Both inputs are observed rather than assumed: `dominant_receiver` is the
   same profile the IR planner seeds its MIC from twenty lines below, and
   `class_is_served` is the same table the helper's own prologue consults, so
   planner and helper cannot disagree about who gets served. The JIT crate
   cannot depend on `native-io`, so the predicate is published as a pointer by
   `build_helpers` (`NIO_BYTE_ELEMENT_SERVED_CLASS_FN`).

### The trade the gate exists to make, measured

Both control probes are monomorphic, so each names one receiver kind with no
profile ambiguity. One binary, the bind forced on (default) and off
(`CRATONVM_JIT_NIO_BYTE_DIRECT_HELPERS=0`), min of 4 rounds x 400 000 ops:

| receiver | helper BOUND | helper UNBOUND | which is right |
|---|---:|---:|---|
| heap (`HeapByteBuffer`) | 237 ns | **18–22 ns** | unbound, by 11x |
| direct (`DirectByteBuffer`) | **52 ns** | 132–138 ns | bound, by 2.6x |

Neither blanket policy is defensible: binding everywhere costs the heap
receiver ~219 ns a call, binding nowhere costs the direct receiver ~83 ns a
call. That is the whole argument for deciding per site from its own profile,
and it is also why the earlier `CRATONVM_JIT_NIO_BYTE_DIRECT_HELPERS` kill
switch could never be set right — it is one global answer to a per-site
question.

The reason unbound heap (18 ns) beats bound direct (52 ns) is worth stating:
`HeapByteBuffer.get(int)` compiles to a bounds check and an array load, while
a direct buffer's `get` has to reach off-heap memory that no Java array access
can express. The helper is not slow; the heap path simply does not need it.

### THREE doors bind this helper, and the third is the one that matters

This is the part that cost the most time, and it is the reason the page insists
on per-door counters.

The gate went on the two doors in `jit/src/lib.rs` first — the single-pass
ladder and the IR planner. The heap control probe then still read 237 ns, and
the census still read `served=0 declined=1595000`: a bind losing on every one
of 1.6 million calls. The bind count said `single-pass=1`, so the obvious
reading was "the single-pass door bound it before a profile existed".

**That reading was wrong**, and only a per-door trace of the gate's own inputs
showed it. `CRATONVM_DBG_JITC=1`:

```
[cratonvm-jitc] nio-byte-bind door=ir pc=21 write=false profile=true dominant=Some(470) served=false
```

One line. The single-pass door never reached the gate at all — because the
site was never bound there. There is a **third** binding loop, in
`vm/src/runtime/interpreter/jit_bridge.rs`, and its own comment says what it
is:

> `ByteBuffer.put(int,byte)` / `get(int)` … **AT THIS DOOR TOO, and this is the
> door that mattered.** … a hot LOOP body is compiled HERE, by the OSR door,
> which runs its own callee-binding loop rather than that ladder.

That door had been given the bind for exactly the reason it now needs the gate:
it is where hot loops are compiled. `osr_entered_optimizing=0` on the same run
is the corroboration — the loop runs in an OSR body, so a gate on the other two
doors never sees it.

The gate is therefore on all three, and the census names all three:
`ByteBuffer.byteElement=sp:N/ir:N/osr:N (profile-refused=N)` under
`CRATONVM_DBG=jit-method-stats`. **A single aggregated count would have hidden
this**, and did: `byteElement=1` was the OSR door's bind being attributed to a
shared counter that the single-pass ladder also incremented.

Bound sites are counted per door, with the refusals beside them, because this
family's whole history is a bind reading as landed while being inert or
actively losing. A claim about this change is not believable without those
three numbers and the `served=/declined=` pair beside them.

## Cause 2 — the `*Unaligned` natives read the backing array one virtual call per byte

**FIXED 2026-09-12.**

`unsafe_read_bytes_from_array` in `native-builtins/src/unsafe_natives_ext.rs`
allocated a `Vec<u8>` and then, for each of `width` bytes, made a virtual
`ctx.get_array_element` call and shifted the byte out of the returned `Value`.
For `HeapByteBuffer.getLong` that is one malloc plus eight virtual calls, on
top of the three (`heap_kind_of`, `heap_element_type_of`, `array_length`) the
prologue already makes — to read eight adjacent bytes.

`NativeHeapAccess` has had the right primitive the whole time, and its own
doc comment names this migration: `read_byte_array_into` /
`write_byte_array_from` are overridden in the VM to `copy_nonoverlapping`
against the array payload, with a per-element fallback of their own for a G1
humongous array so the caller does not have to know about that case.

The fix adds a `byte[]`/`boolean[]` fast path (one `memcpy`) and a stack buffer
in place of the `Vec`, keeping the generic per-element loop for the wider
element types — `char[]`/`int[]`/`long[]`, which a multi-byte read may
legitimately SPAN. That property is not decoration: it is what
`ArraysSupport.vectorizedMismatch` and therefore `Arrays.equals(char[])` depend
on, and a truncating read of it is the bug the original comment records.

## Where the end-to-end HTTP stack sits after this

`probes/NioHttpThroughputProbe.java` (new), loopback, JDK-only dependencies,
`TCP_NODELAY` on both ends, min of 2 rounds:

Arms **interleaved HotSpot/CratonVM, three pairs, min of each** — see the
caution below for why that matters here more than anywhere else on this page:

| arm | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| socket echo, 1 KiB direct buffer | 38.10 us | 84.67 | 2.2x |
| HTTP POST, 64 B body | 342.54 | 2532.17 | 7.4x |
| HTTP POST, 256 KiB body | 1322.63 | 28562.06 | 21.6x |

**CAUTION — this arm is the noisiest thing on the page, and it produced a wrong
number once already.** An earlier revision of this table read 2.1x / 3.7x /
7.3x. Those ratios were arithmetic over a HotSpot column measured in a
different session, where the same `java` command on the same box read 57.88 /
850.43 / 4705.51 — a 2.4-3.2x swing against the numbers above. Windows loopback
plus a per-request `HttpClient` connection is simply not a stable measurement,
and BENCHMARK.md's rule about comparing within one window is what the first
revision broke. **Interleave, or do not quote a ratio from this probe at all.**

**No same-binary before/after exists for this table either** — the branch's
pre-change binary was not kept. It is here as the current state of the thing
the investigation was opened about, and as the shape of what is left: the
small-body arm is header parse/format, and the large-body arm is bulk copy,
which is the `heap get(byte[256])` / `put(byte[256])` row of the matrix above
still sitting at 38-49x.

## Cause 3 — TWO native funnel crossings per multi-byte heap accessor, one of them a constant `null`

**FIXED 2026-09-12** for the `session()` crossing. The `getIntUnaligned`
crossing is the remaining one and is a different problem — see
[What is still there](#what-is-still-there).

`HeapByteBuffer.getInt(int)` compiles to:

```
 0: getstatic  SCOPED_MEMORY_ACCESS
 4: invokevirtual session:()Ljdk/internal/foreign/MemorySessionImpl;   <-- crossing 1
15: invokevirtual checkIndex:(II)I
19: invokevirtual byteOffset:(J)J
26: invokevirtual ScopedMemoryAccess.getIntUnaligned:(...)I            <-- crossing 2
```

`--dump-native-registry` over `probes/OnlyHeapGetInt.java`, 200 000 accessor
calls and nothing else, divides exactly:

```
200000  java/nio/HeapByteBuffer.session()Ljdk/internal/foreign/MemorySessionImpl;  [bridge]
200000  jdk/internal/misc/ScopedMemoryAccess.getIntUnaligned(...)I                 [synthetic-stub]
  1906  jdk/internal/util/Preconditions.checkIndex(IILjava/util/function/BiFunction;)I [bridge]
```

Exactly one of each per accessor. (`checkIndex` at 1 906 of 200 000 is the thin
direct helper working: the census counts only the dispatches that resolved by
name, so the residue is the pre-compilation warm-up.)

**`session()` is the cheap one, and it is pure loss.** Its registered body is
`|_ctx, _args| Ok(Some(Value::Object(None)))` — a constant null, on every
multi-byte heap accessor, through the ~160 ns generic native funnel. That is
the same shape as `Reference.reachabilityFence`, whose page says of an equally
empty body that *"paying ~160 ns of generic native funnel for that is pure
loss"* and which was given a thin direct helper for exactly this reason.

### The trap, and the shape of the fix that avoids it

`session()` is dispatched on the RECEIVER's class, and the shim is registered
for eleven named classes that do **not** include `HeapByteBufferR` or
`DirectByteBufferR`. For a receiver outside that set the real
`java.nio.Buffer.session()` bytecode runs instead — and that method is
**`final` on `java/nio/Buffer`**, with the body:

```
0: aload_0
1: getfield segment:Ljava/lang/foreign/MemorySegment;
4: ifnull  18          // -> aconst_null; areturn
7: ... checkcast AbstractMemorySegmentImpl; invokevirtual sessionImpl()
```

i.e. it returns a real session for a buffer that has one. A thin helper that
returned null unconditionally would answer for receivers whose real method the
registry never took over, silently skipping scope validation on an FFM-derived
buffer. **Do not bind this on the class-name list** — the list's own omissions
are the bug.

### What was built instead

The helper answers only for a class id that
`native-builtins`' `buffer_session::class_is_served` says **the shim itself has
already run for**. The shim records each class it serves; the helper serves
that set and no other, and declines everything else to the generic dispatcher.

That makes the change *exactly* behaviour-preserving rather than
argued-to-be-equivalent: for any receiver the helper answers, it returns
byte-for-byte what the shim returned for that same receiver, because the shim
answering for that class is the precondition for the helper answering at all.
The population is **learned from the funnel, never guessed from a name list** —
the same discipline `native-io`'s `elem_fastpath` uses, and for the same
reason.

`CRATONVM_JIT_BUFFER_SESSION_DIRECT=0` (declared in `flag_groups.rs` as the
JIT token `buffer-session-direct`, so `CRATONVM_JIT=buffer-session-direct` also
reaches it) restores the funnel, which is what the
self-diff gate below uses to prove the two arms agree.

### What it is worth

A/B on that switch, ONE binary, arms interleaved, two rounds each, min of each
arm (`probes/NioBufferCostProbe.java`, 800 000 ops):

| arm | direct=1 | direct=0 | |
|---|---:|---:|---:|
| heap `getShort(int)` | **318.14** | 401.91 | −21% |
| heap `getInt(int)` | **295.09** | 375.38 | −21% |
| heap `getLong(int)` | **301.19** | 399.19 | −25% |
| heap `putInt(int,int)` | **352.06** | 437.56 | −20% |
| direct `getInt(int)` | 145.47 | 152.01 | — |

**Every heap row separates completely**: the `on` arm's MAXIMUM is below the
`off` arm's MINIMUM in all four (e.g. `getInt` 352.64 against 375.38). The
direct row does not move and is not expected to — this is the heap accessor's
crossing.

Engagement, which is the half a timing cannot supply:

```
[cratonvm] JIT Buffer.session direct: sites sp:1/ir:1/osr:0 served=398042 declined=0
```

398 042 served against 400 000 accessor calls and **zero** declines — the
receiver screen admits the whole population here rather than quietly refusing
it, which is the failure mode `byteElement` had and the reason this line is
printed at all.

**It does NOT move the two control probes**, and that is worth recording so
nobody re-runs it: interleaved A/B on the same switch reads heap `get(int)`
23.3-23.9 on against 22.5-23.7 off, and direct `get(int)` 57.3-58.5 against
55.8-63.9 — overlapping in both directions. Neither control's hot path reaches
`session()`: the single-byte `get(int)` accessors do not call it, and a direct
receiver's is served by the `byteElement` helper before the Java body runs.
A first reading of this pair recorded direct `get(int)` at 68.02 and looked
like a 30% regression; it was one noisy sample on a box that had drifted ~20%
slower between sessions, and the interleaved A/B above is what settled it.

## What is still there

One native funnel crossing per multi-byte heap accessor:
`jdk/internal/misc/ScopedMemoryAccess.get*Unaligned`. Unlike `session()` it
does the actual work, so removing the crossing means moving the read itself
into the fast path rather than deleting a constant — which is the
`CRATONVM_BYTEBUFFER_INTRINSIC` lever that
`internal/fixed-suite-bugs/springboot/zipcontenttests-bytebuffer-accessor-call-cost-RETIRED-20260810.md`
built and left default-OFF on a measurement.

**Re-measure that lever before rebuilding it, because its evidence is now
stale in the VM's favour.** It was parked because a class-level A/B could not
separate it from zero, on a binary where the enclosing methods were being
refused the optimizing tier by cause 1 and every accessor was paying cause 3's
second crossing. Both of those are gone. The arithmetic that bounded it at
~10% was computed against an 18.3%-of-leaf-samples accessor cost that this page
has since cut by 2-3x, so the ceiling has moved too — in which direction is a
measurement, not an argument.

The bulk arms are the other open front: `heap get(byte[256])` is still 49x
HotSpot and `direct put(byte[256])` 58x, which is where the 256 KiB HTTP body
arm's 21.6x comes from — by far the largest remaining end-to-end gap.

### The bulk cost is the CROSSING, not the copy or the lookups (measured)

Two plausible-looking causes were built, measured and **reverted**. Both are
recorded here so the next person spends their build cycles elsewhere.

The decomposition that reframes it (`probes/BulkSplitProbe.java`, one binary):

| arm | CratonVM | HotSpot |
|---|---:|---:|
| `System.arraycopy(byte[256])` | 14.8 | 7.3 |
| `limit()` alone | 113 | 0.03 |
| `position(0)` alone | 209 | 0.00 |
| `heap get(byte[256])` + `position(0)` | 601 | 26.7 |
| `direct get(byte[256])` + `position(0)` | 1837 | 12.0 |

Read the first row first: **the copy is only 2x off HotSpot.** Whatever the
bulk arms are paying, it is not the memory traffic. And `limit()` — a shim
whose whole body is one indexed field read — costs 113 ns, which prices the
generic native funnel crossing on its own. The CratonVM column is also nearly
FLAT in length: a 16-byte `get` and a 4096-byte `get` are within noise of each
other, which is the signature of a fixed per-call cost.

`java/nio/ByteBuffer.get([B)`, `put([B)` and `position(I)` are all registered
bridge natives (`native-builtins/src/servlet.rs`) standing in front of real
Java methods that have code, so the loop pays two crossings per iteration.

**Hypothesis 1 — the by-name field lookups. WRONG.** `s2_bb_arr`,
`s2_bb_heap_base` and `s2_bb_direct_addr` resolve `hb` / `offset` / `address`
through `get_field_by_name`, which takes `class_manager.read()` and then walks
the class chain string-comparing every non-static field. A direct buffer does
three of those per call. Memoizing `(class_id, name) -> slot` — positives only,
since `resolve_field_index_in_hierarchy` returns `None` for a chain that is
merely not loaded YET — moved nothing.

**Hypothesis 2 — the per-call staging allocation. ALSO WRONG.** Every bulk
shim moved its bytes through a `vec![0u8; len]` allocated and freed per call,
including the heap-to-heap path (`s2_bb_bulk_array_copy`). Replacing all six
sites with a reused thread-local buffer moved nothing either.

Both were A/B'd on their own kill switches, four arms interleaved in ONE
binary, min-of-2 (ns/op):

| arm | both on | both off | cache only | scratch only | spread |
|---|---:|---:|---:|---:|---:|
| `heap get(byte[256])` | 601 | 602 | 608 | 616 | 2.4% |
| `heap get(byte[4096])` | 711 | 670 | 669 | 725 | 8.3% |
| `direct get(byte[256])` | 2031 | 1837 | 1772 | 1897 | 14.6% |
| `direct put(byte[256])` | 1991 | 2009 | 1926 | 2142 | 11.2% |
| **`arraycopy` (CONTROL)** | **14.6** | **12.3** | **14.9** | **12.8** | **20.8%** |

The control row is the one that decides it. `System.arraycopy` is untouched by
either change and spreads 20.8% across the four arms — WIDER than any arm under
test. Nothing here is separated from zero.

The budget says the same thing without the A/B. `heap get(byte[256])` + `pos`
is 601; `position(0)` is 209; a bare crossing is 113; so the whole `get` shim
BODY — every field lookup, the bounds math, the allocation and the copy
together — is about 280 ns. Two by-name lookups could not have been the bulk of
that and then vanish without a trace when removed.

An earlier reading of these same two changes as a 25-40% REGRESSION was also
wrong, and wrong the same way the first HTTP ratios were: it compared two
binaries built 90 minutes apart, and `System.arraycopy` — which neither change
touches — moved 1.7x between them. The box drifts on this scale. **Anything
measured here needs its kill switch and a control row in the same binary.**

So the lever for the bulk arms is the crossing itself: a thin direct helper for
`get([B)` / `put([B)` / `position(I)`, bound at the call site the way the scalar
element helpers already are — and therefore needing the same treatment as
cause 1, all THREE doors and a per-door census, not just the two in
`jit/src/lib.rs`.

## Reproducers

```bash
javac -d /tmp/cls probes/NioBufferCostProbe.java && cratonvm --java-home <jdk> -cp /tmp/cls NioBufferCostProbe 1000000 2
```

The per-door bind census AND the served/declined pair, which together are the
engagement proof for cause 1 — neither is sufficient alone, which is the whole
lesson of this page:

```bash
CRATONVM_DBG=jit-method-stats cratonvm --java-home <jdk> -cp /tmp/cls HeapBufferShapeControlProbe 400000 2>&1 | grep -E 'byteElement|byte-element helper calls'
```

The heap probe should show the bind REFUSED (`profile-refused` non-zero) and
`served=0 declined=0`; the direct probe should show it bound and
`served` ~= the call count with `declined` near zero:

```bash
CRATONVM_DBG=jit-method-stats cratonvm --java-home <jdk> -cp /tmp/cls DirectBufferShapeControlProbe 400000 2>&1 | grep -E 'byteElement|byte-element helper calls'
```

Why a method was refused the optimizing tier, which is how cause 1 was found:

```bash
CRATONVM_DBG=ir-compiles cratonvm --java-home <jdk> -cp /tmp/cls NioBufferCostProbe 200000 1 2>&1 | grep -E 'refused|NO invoke_info'
```

The crossing census behind cause 3:

```bash
cratonvm --java-home <jdk> --dump-native-registry census.json -cp /tmp/cls OnlyHeapGetInt 200000
```

## Contract gates

The accessors' observable behaviour is pinned against HotSpot by two
differential probes, both diffed line-for-line rather than spot-checked:

* `probes/ByteBufferAccessorMatrixProbe.java` — every buffer shape
  (`allocate`/`wrap`/`wrap(off,len)`/`slice`/`duplicate`/read-only/direct), both
  byte orders, bounds against `limit` rather than `capacity`, the exception
  CLASS and its null message per width, and NaN / `-0.0` bit patterns.
* `probes/DirectBufferShapeControlProbe.java` — the arm that the bind gate
  could silently cost. The thin element helper exists for direct receivers and
  only for them, so a gate that fixed the heap case by refusing the bind
  everywhere would look like a win on every other probe and show up only here.
  Monomorphic direct receiver, so its profile is unambiguous — which is the
  same input the gate reads.
* `probes/BulkMatrixProbe.java` — the BULK arms, which the accessor matrix does
  not reach at all: `get`/`put` with and without an explicit offset, and
  buffer-to-buffer, on both receiver kinds, at nine lengths run DESCENDING then
  ascending. The length order is the point. Both reverted experiments above
  were "invisible by construction", and the specific way a staging buffer stops
  being invisible is that it is not re-filled to the CURRENT length and leaks
  the previous call's tail — which only a shrinking length sequence catches.
  82 lines, byte-identical to HotSpot, exception class and message included.
* `probes/UnsafeUnalignedMatrixProbe.java` — the layer cause 2 touches: every
  width at every alignment on both buffer kinds, and the multi-element spanning
  reads (`char[]`/`int[]`/`long[]`) that `ArraysSupport.vectorizedMismatch`
  needs, exercised through `Arrays.equals` on arrays that differ in exactly one
  late element — the shape a truncating read reports as equal.

### Rust test gates, run on this binary

```
cratonvm-jit             --lib                  2278 passed;  0 failed
cratonvm-native-builtins --lib                  4200 passed;  4 failed  (see below)
cratonvm-native-builtins --test stub_ratchet      12 passed;  0 failed
cratonvm-types --test doc_citation_paths           2 passed;  0 failed
```

The two new `buffer_session` unit tests pass. The four `native-builtins`
failures are **not** from this lane, and the check that says so is cheap enough
that there is no reason to assert it instead:

    g23_nomination_witnesses::only_set_parent_refuses_null_among_loggers_one_argument_setters
    g23_nomination_witnesses::the_log_record_constructor_requires_the_level_and_permits_a_null_message
    g23_nomination_witnesses::every_thread_constructor_captures_inheritable_thread_locals
    throwable_ctor_single_table_witness::the_blanket_four_ctor_loops_stay_collapsed_onto_the_table

All four are source-SCANNING witnesses: they `read_to_string` their own crate's
`lib.rs` at RUN time and search it for literals that embed `
`, e.g.

```rust
src.find("\"setParent\",
        \"(Ljava/util/logging/Logger;)V\"")
```

This working copy is CRLF, so the needle is present only as `
` and the
`.expect` fires. Three experiments, all against the SAME already-built test
binary — these tests read the file at run time, so the source can be swapped
underneath them without a rebuild, which is what makes the check cheap:

| source on disk | result |
|---|---|
| this lane's `lib.rs`, CRLF (as checked out) | 4 fail |
| **`git show HEAD:...`, CRLF** — i.e. without this lane's diff | **the same 4 fail** |
| this lane's `lib.rs`, `tr -d ''` | all pass |

The middle row is the one that settles it. This lane's diff to that file is
14 added lines at offsets 4241 and 20243; every needle above is past 48700.

Two things worth recording rather than fixing here. `core.autocrlf` is `input`
in this worktree, which checks out LF — so something other than git converted
the tree to CRLF, and the LF state these tests need was the accident, not the
norm. And `.gitattributes` opens by saying the repo is developed with
`core.autocrlf=true`; if that is right, these witnesses fail for every Windows
developer and the durable fix is in the helpers (strip `` in `lib_rs()` and
its siblings), not in anyone's checkout.

## Related

* `../perf/per-call-blind-gpr-spill-20260826.md` — the per-compiled-call cost
  this page's calls also pay, and the page that established the ABBA / one-binary
  protocol used here.
* `../netty/adaptivebytebufallocator-searchprocessor-per-call-throughput-wall.md`
  and the two exhaustive-loop pages — netty workloads sitting on this floor.
* `internal/fixed-suite-bugs/springboot/zipcontenttests-bytebuffer-accessor-call-cost-RETIRED-20260810.md`
  — the earlier page. Its attribution was right and its numbers reproduce; what
  it could not see is that the accessors are slow for a reason that is not in
  the accessors.
