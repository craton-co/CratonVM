# Bug 25 — `NPE: Cannot invoke cacheSerializedValue on null` (null serialization cache)

> **✅ FIXED (2026-06-14) — JIT escape-analysis scalar-replaced an escaping object.**
> Root cause is `jit/src/x64.rs::analyze_escapes` (the scalar-replacement escape pass). The
> `ObjectSerializationCache` is created in `MessageUtil.toByteBuffer` and **escapes** as an argument
> to `message.size(cache, version)` / `write(bytes, cache, version)`. The pass is a single linear
> walk with an abstract operand stack carrying each slot's `new`-provenance; `invokevirtual/
> interface/static` run `escape_all!()` to escape every tracked argument. BUT the catch-all `_` arm
> **forgot the provenance of EVERY operand-stack slot** ("clear provenance, keep depth") on any
> unmodeled opcode — including the primitive loads `iload`/`lload`/`fload`/`dload` and the
> const/`ldc`/`getstatic` family. `toByteBuffer` does `aload_2 cache; iload_1 version;
> invokeinterface size`: the `iload_1` between the cache load and the call erased `cache`'s
> provenance, so `escape_all!` at the call missed it → `cache` reported non-escaping → scalar-
> replaced (allocation elided, `<init>` skipped) → the JIT passed a null/zero where the cache should
> be → "Cannot invoke cacheSerializedValue on null". Proven via PC-annotated disasm (added to
> `vm/src/jit/disasm.rs`): local map `L2(cache)=r13`, and `bc@0 new OSC` emitted NO allocation while
> `astore_2` stored `[rbp-48h]=0` into `r13`.
>
> FIX: give the primitive-load / const / `getstatic` family their own arm in `analyze_escapes`
> (`0x00` nop; `0x01..=0x18 | 0x1a..=0x29 | 0xb2`) that pushes ONE untracked `None` slot WITHOUT
> touching existing slots' provenance. This is strictly corrective — it only makes objects that
> genuinely escape-via-a-later-call (with an intervening primitive arg) stay tracked → escape →
> heap-allocate; it can never CREATE a scalar replacement, so it cannot de-opt a legitimately
> non-escaping object. Verified: CreateAclsRequestTest 4/4, SimpleExampleMessageTest 21/21,
> RequestResponseTest 40/41 (all JIT-on, was 2/4, 16/21, 29/41). Residual: `RequestResponseTest.
> fetchResponseVersionTest` (expected 1, got 0) is a SEPARATE JIT bug — passes --nojit, not
> cacheSerializedValue. NOTE latent: the `_` arm still forgets provenance for arithmetic/array ops
> between an object-load and an escaping call (`aload o; iload a; iadd; invoke(o,..)`) — rarer; same
> class, a follow-up could escape-on-unknown instead of forget.

> **UPDATE 3 — JIT-ONLY now; precisely isolated to a JIT call-chain miscompile (2026-06-14, binary a2e3261f):**
> After the dev merge (collection-intrinsic fixes), the cluster is **GREEN under `--nojit`**
> (all 13 classes pass: RequestResponseTest 41/41, CreateAclsRequestTest 4/4, …). The remaining
> failures are **JIT-ON only** — so the earlier "reproduces under --nojit" no longer holds; the
> interpreter-side variant is fixed. Under JIT, the exact `cacheSerializedValue on null` NPE persists.
>
> **Root method (corrected):** `AbstractRequest.serialize()` calls
> **`MessageUtil.toByteBuffer(data(), version)`** (NOT `RequestUtils.serialize` — that was wrong in
> UPDATE 1/2). `toByteBuffer` is the method that does `ObjectSerializationCache cache = new …();
> message.size(cache,ver); message.write(bytes,cache,ver)`. So `toByteBuffer` is the cache holder.
>
> **Bisect (via `CRATONVM_JIT_BISECT_ONLY` / `CRATONVM_JIT_BISECT_SKIP`, on CreateAclsRequestTest):**
> - JIT-off / any single kafka package alone JIT'd → PASS.
> - `requests` + `protocol` JIT'd → FAIL; narrowed to **`protocol/MessageUtil`** (req+MessageUtil FAIL;
>   req+ObjectSerializationCache / +ByteBufferAccessor / +SendBuilder all PASS).
> - Within MessageUtil: skipping JIT for **`MessageUtil.toByteBuffer`** → PASS (skipping
>   `toVersionPrefixedByteBuffer` → still FAIL). So `toByteBuffer`'s compilation is the trigger.
> - **But `RequestUtils+MessageUtil` alone → PASS.** The bug needs the **whole JIT call chain compiled**:
>   minimal repro = `CreateAclsRequestTest` (shouldRoundTripV0) **+** `AbstractRequest` (serialize) **+**
>   `CreateAclsRequest` (data) **+** `RequestUtils` **+** `MessageUtil.toByteBuffer`, all JIT-eligible.
>   Any proper subset passes. → This is a **JIT→JIT call/inlining interaction**, not standalone
>   `toByteBuffer` codegen (cf. bug-H "JIT→JIT call bypassed callee's local catch").
>
> **Not GC, not the usual suspects:** `-Xmx8g` (suppresses young GC) does NOT help → not young-gen
> collection of the cache; the doc's "genuine null (Value::Object(None))" holds → a literal null slot,
> not a stale/collected pointer. `CRATONVM_SHADOW_STACK`, `CRATONVM_PRECISE_JIT_MAPS`,
> `CRATONVM_JIT_NO_INLINE_VCACHE`, `CRATONVM_JIT_DISABLE_INLINE_NEW`, `CRATONVM_JIT_MAIN_INLINE` — none fix it.
>
> **Disasm (`CRATONVM_DBG_JIT_DISASM=toByteBuffer`, failing config):** compiled `toByteBuffer`
> (entry 0x1580000, 935 bytes) holds the cache in **r12** (callee-saved), spills it to GC slots
> `[rbp-20h]`/`[rbp-28h]` before each invoke, and routes every Java call through a generic invoke
> helper `0x7FF78A21B310` that returns the `i64::MIN` (0x8000000000000000) deopt/exception sentinel.
> The `<init>` (incl. the inner `new IdentityHashMap`) is inlined. Suspicious: several
> `mov r12,[rbp-48h]` reloads where `[rbp-48h]` is reused as a scratch temp across calls — needs
> verification that r12 (cache) isn't overwritten with a wrong temp between create and the `size()`
> marshalling. **Next:** single-step the operand-stack spill/reload for the `cache` slot across the
> chained JIT→interpreter `size()` call; confirm whether the genuine null comes from (a) a clobbered
> r12 reload in `toByteBuffer`, or (b) the deep JIT-caller chain not preserving the spill frame.
> Repro env: `CRATONVM_JIT_BISECT_ONLY=org/apache/kafka/common/requests/RequestUtils,org/apache/kafka/common/protocol/MessageUtil,org/apache/kafka/common/requests/AbstractRequest,org/apache/kafka/common/requests/CreateAclsRequest,org/apache/kafka/common/requests/CreateAclsRequestTest`
> then `KRun org.apache.kafka.common.requests.CreateAclsRequestTest` (2/4 fail). Stopgap available:
> add `MessageUtil.toByteBuffer` to skip_list `is_known_miscompile` (runs real bytecode interpreted).
>
> **UPDATE 4 — NOT inlining; it is main-loop codegen / register allocation (2026-06-14):**
> JIT inlining is **default-OFF** (`CRATONVM_JIT_MAIN_INLINE`), and the inline body bails on `new`
> (0xbb has no handler → default arm at x64.rs:10610), so `ObjectSerializationCache.<init>` (which
> does `new IdentityHashMap; <init>; putfield`) is **never inlined**. The bug reproduces anyway →
> it is in the **main compile loop**, specifically the `new OSC; dup; invokespecial OSC.<init>;
> astore_2; … aload_2; invokeinterface size(cache,ver)` sequence. `OSC.<init>` is a non-inlinable
> **allocating** constructor (a real call-out). The `cache` local (local 2 in `toByteBuffer`,
> graph-colored to a callee-saved reg) reads back as a stale-zero register at the `size()` arg load
> — i.e. the freshly-`new`'d object held in a callee-saved register/operand slot is LOST across the
> out-of-line allocating `<init>` call (or `astore_2` writes a different home than `aload_2` reads).
> Clean UTF-8 disasm of compiled `toByteBuffer` (entry 0x1580000) confirms a `size`-shaped dispatch
> with `cache`=`r13`=0 and a later `astore_2` writing `r12` from a clobbered `[rbp-48h]` — a
> local-home / live-range inconsistency. Locus: `jit/src/x64.rs` main loop (aload/astore_n local
> emission + the `new`/operand handling around an out-of-line call) and/or `jit/src/regalloc.rs`
> interference across the call. **Blast radius = all JIT code**, so the fix needs ground-truth
> bytecode-PC↔native-offset annotated disasm (the dumper at vm/src/jit/disasm.rs currently emits no
> PC map) plus a minimal standalone repro of `new X(); dup; <init>(allocating); use(local)` before any
> edit — speculative regalloc/dispatch changes risk broad regressions. **Recommended next step:** wire
> `compiler.pc_to_native` into `disasm::maybe_dump` to label each native block with its bytecode PC,
> rebuild once, then read the exact emit for `astore_2`/`aload_2`/the `size` call.


**Severity:** High — the single largest CratonVM-only FAIL cluster: **~20 failing
tests across 13 classes**, all protocol request/response serialization. HotSpot OK.

> **ROOT-CAUSE CORRECTION (2026-06-13):** the receiver of `cacheSerializedValue` is
> **`org.apache.kafka.common.protocol.ObjectSerializationCache`**
> (`cacheSerializedValue(Object,[B)V`), NOT a message-array element. So the null is the
> **serialization cache itself** being null when a generated message's
> `size(cache,ver)`→`addSize(size,cache,ver)` runs (it caches each String/Bytes field's
> serialized bytes via `cache.cacheSerializedValue(field, bytes)`). The earlier
> "array null hole" theory below is superseded. The cache is created right before, e.g.
> `ObjectSerializationCache cache = new ObjectSerializationCache(); message.size(cache,…)`,
> so the bug is either (a) `new ObjectSerializationCache()` yielding null / a broken
> instance, or (b) the `cache` local/param being lost between creation and the
> `addSize` use. Reproduces under `--nojit` (so NOT the bug-21/22 JIT register-root
> issue). Next: reproduce a single `CreateAclsRequestTest.shouldRoundTripV0`, dump the
> `size()`/`addSize()` `cache` arg, and check `new ObjectSerializationCache()`.
>
> **UPDATE — not reproducible standalone (2026-06-13):** a standalone `CacheProbe`
> matching the test — `new ObjectSerializationCache()`, direct `Data.size/write`, full
> serialize→parse→re-size round trip, AND `new CreateAclsRequest.Builder(d).build(v)` →
> `request.serialize()` with multiple creations — **all succeed** on the current binary
> (cache non-null, no null array elements, correct byte sizes). So the core serialization
> path is sound; the null cache only manifests **inside the JUnit harness**
> (`shouldRoundTripV0/V1` still fail there). Most likely a **GC-timing / instrumentation
> interaction** under the harness's heavier allocation load (Mockito self-attach, JUnit
> reflection) that loses the `cache` local mid-serialization — NOT a deterministic
> serialization bug. Next: instrument the interpreter "invoke … on null" throw site to
> dump the caller method+pc when the method is `cacheSerializedValue` (frame-capped
> `DBG_ATHROW` only reaches the JUnit rethrow), run the real test, and capture the GC
> event around the throw. Remaining open item for the bug-25 cluster.
>
> **UPDATE — pinned via `CRATONVM_DBG_NPE_STACK=1` (existing gate, 30 frames):** the
> null-cache call chain is
> ```
> [51] CreateAclsRequestData$AclCreation.addSize(MessageSizeAccumulator, ObjectSerializationCache, S) pc=86  // -> cache.cacheSerializedValue(field, bytes)
> [50] CreateAclsRequestData.addSize(MessageSizeAccumulator, ObjectSerializationCache, S) pc=75
> [49] org/apache/kafka/common/protocol/Message.size(ObjectSerializationCache, S)I pc=17
> [48] org/junit/platform/commons/util/ReflectionUtils.invokeMethod(Method, Object, Object[]) pc=45
> ```
> Two hard facts: (1) the receiver is a **genuine null** — the interpreter's
> stale/all-zero-header salvage (interpreter.rs ~10440) did NOT fire, so this is
> `Value::Object(None)`, not a GC-collected stale pointer; (2) the test method's own
> frames (`shouldRoundTripV0` → `AbstractRequest.serialize` → `RequestUtils.serialize`)
> are **ABSENT** between JUnit's reflective `invokeMethod` [48] and `Message.size` [49]
> — `size` is reached *directly* from the reflective invoke with a **null cache arg**.
> So the bug is in CratonVM's **reflective invocation path** (`Method.invoke` /
> `ReflectionUtils.invokeMethod` argument marshalling, or a Method-resolution/frame
> anomaly), NOT in message serialization (which is byte-correct standalone). This is why
> it is JUnit-harness-only. **Next:** dump the `Object[] args` (and the resolved target
> Method) inside CratonVM's `Method.invoke` native for the failing call to see where the
> `ObjectSerializationCache` argument becomes null / the wrong method is dispatched.
>
> **UPDATE 2 — genuine null confirmed; degraded-Method ruled out:**
> - Code-path analysis of the throw site (interpreter.rs ~10868) shows it is the `_`
>   arm where the receiver is `Value::Object(None)` — a **genuine null**, definitively
>   NOT a GC-collected stale all-zero-header object (that path salvages at ~10440).
> - `CRATONVM_DBG_MINVOKE=1` + `CRATONVM_DBG_METHOD_INVOKE_BOX=1` on the failing test
>   produced **no degraded-Method hits** → the reflective dispatch resolves the right
>   `Method`; it is not a clazz-null Method bug.
> - The absent `serialize()`/test-method frames between `invokeMethod` and `Message.size`
>   indicate those calls run through CratonVM's **stackless cached-dispatch**
>   (`execute_invokevirtual_cached`/`execute_invokestatic_cached`), which does not push a
>   full scannable frame. **Leading hypothesis:** the `ObjectSerializationCache` local
>   created in `RequestUtils.serialize` is lost (read back as null) because the
>   stackless-dispatched serialize path's locals aren't GC-rooted/preserved under the
>   harness's allocation pressure — the interpreter-side analogue of bug-21/22, but on
>   the stackless path. **Next:** instrument `execute_invokevirtual_cached` (or disable
>   stackless dispatch for the serialize path via the existing bisect gate) and re-run;
>   if the cache survives with stackless dispatch off, the fix is to root/spill locals
>   across the cached-dispatch boundary. This is the remaining bug-25 work; the cluster
>   is otherwise byte-correct (proven by standalone probes).

## Symptom
```
=> java.lang.NullPointerException: Cannot invoke cacheSerializedValue on null
=> java.lang.RuntimeException: Failed to deserialize request {acks=-1,timeout=123,
   partitionSizes=[topic1-1=72]} with type class org.apache.kafka.common.requests.ProduceRequest
```
A Kafka generated-message struct (the elements of a message array field, e.g.
`ProduceRequestData.TopicProduceData[]`, ACL filters, etc.) is **null** when the
serializer walks the collection and calls `cacheSerializedValue()` on each element.

## Affected classes (13)
`RequestResponseTest`, `SimpleExampleMessageTest`, `NullableStructMessageTest`,
`CreateAclsRequestTest`, `DeleteAclsRequestTest`, `DeleteAclsResponseTest`,
`DescribeAclsRequestTest`, `DescribeAclsResponseTest`, `DeleteTopicsRequestTest`,
`OffsetCommitResponseTest`, `StopReplicaRequestTest`, `TxnOffsetCommitResponseTest`,
`UpdateFeaturesRequestTest`. (Related `NPE: Cannot invoke tags/topics/errorCode/value
/setArraySizeInBytes on null` — 10+ more failures — are almost certainly the same
null-hole defect on different generated fields.)

## Root cause (to pin down)
A CratonVM collection/array intrinsic introduces a **null hole** into a generated
message's array/list field during the deserialize→re-serialize round trip. This is
the same family as the already-fixed **bug-12** (`ImplicitLinkedHashCollection.toArray()`
null holes) and **bug-18** (`Uuid` zeroed) — a native collection/`toArray`/clone path
that returns a slot the JDK would have filled. Candidates:
- `ArrayList`/`Arrays.asList`/`toArray(T[])` producing a trailing or interior null
  (cf. bug-14 `removeAll` over-removal),
- the generated `ArrayOf`/`CompactArrayOf` protocol reader writing fewer elements than
  `size`, leaving nulls,
- an `ImplicitLinkedHashCollection`-backed field (ACLs use these heavily).

## Reproduce
```
cd apps/kafka/tests
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 ./kv2.exe -cp ".;$(cat cp.txt)" KRun \
  org.apache.kafka.common.requests.RequestResponseTest
```
Narrow to `testSerialization` / a single `ProduceRequest` round-trip; dump the array
field right after `read(...)` and find which index is null and which intrinsic filled
the backing collection. Fixing the null-hole source should clear all 13 classes at once.

## Note
Verify against the **current** binary (`9d8bba97`) first — the dev merge `b5c709c7`
touched collection intrinsics (CSLM/TreeMap/LinkedHashSet); some of this cluster may
already be reduced. Counts above are from the pre-merge `cv-full2` run.
