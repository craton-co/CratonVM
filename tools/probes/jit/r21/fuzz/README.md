# JIT round 21 wave 2, lane fuzz: differential probes

Differential Java probes for the compiled-code shapes JIT round 21 wave 1 added behind switches
(commit `7979320f8a`). Every probe is one file, default package, deterministic output (no timing
lines), and is diffed line by line against HotSpot 25. Each prints rows cold, after a warm-up long
enough for the optimizing tier (>= 200k loop trips or a method called >= 50k times), after a
deopt trigger (a class loaded late, a rare branch, an exception mid-loop), and again. The class
comment of each probe names the switch, the function and the edge it targets, and repeats its run
lines.

Not run in this lane (rules: no javac / java / VM). Nothing here has been compiled yet: expect a
first round of javac fixes from the orchestrator.

## How to run

```
javac -d out tools/probes/jit/r21/fuzz/*.java          # JDK 25 (java.lang.classfile is used by 4 probes)
java -Xmx512m -cp out <Probe> > hs.txt                  # HotSpot 25: the expected output
cratonvm -Xmx512m -cp out <Probe> > cvm.txt             # every arm below, then: diff hs.txt cvm.txt
```

Arms, unless a row says otherwise: `default` (the launcher's `--jdk-only`), the feature switch(es)
`=1` (or `=0` for a default-ON correctness switch: the kill arm, which may legitimately show the
old defect), `--compatible`, `--nojit` (facts only; slow on the long loops). A diff on any arm
except a kill arm is a finding. Probes using `java.lang.classfile` are marked (cf).

## Probes

| probe | targets | arms beyond default / `--compatible` / `--nojit` |
|---|---|---|
| `R21FzLoopElem1` | LOOPS21-1 array-element promotion: pre-header guards on a zero-trip loop, bad / MIN_VALUE index, null array, AIOOBE / NPE at the first access with the iteration's earlier static store done once, trap past the de-spec limit | `CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE=1` |
| `R21FzLoopElem2` | LOOPS21-1 aliasing: same array via two refs, two index nodes equal at run time, read of another element, int + long cells, int + float, array through an Object field, self-feedback | `CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE=1` |
| `R21FzLoopElem3` | LOOPS21-1 write-back: store under a branch (body memory phi), net-zero loop (skipped write-back), 64-bit LCmp, int wrap, narrow arrays (byte/short/char/boolean: refused kinds), exception mid-loop in a sibling | `CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE=1` |
| `R21FzLoopElem4` | OSR entry at promoted headers (element, receiver field, static, nested loop): the withheld OSR stub must not seed a stale cell | `..._ARRAY_ELEMENT_PROMOTE=1`; `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY=1`; both |
| `R21FzLoopField1` | HOIST13-2 receiver accumulators: alias through a second base, field shadowing (`Base.f` / `Sub.f`), inherited field with two receiver classes, long + int cells, a call reading `this.f` in the loop, nested loops | `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY=1` |
| `R21FzLoopField2` (cf) | un-narrowed `putfield` / `putstatic` / `bastore` (byte, short, char, boolean `& 1`) in promoted receiver, static and fresh-object cells, and narrow arrays | `..._FIELD_PROMOTE_METHOD_ENTRY=1`; `..._ARRAY_ELEMENT_PROMOTE=1`; control `CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE=0 CRATONVM_JIT_IR_LOOP_FRESH_FIELD_PROMOTE=0` |
| `R21FzLoopField3` | transfers out of a promoted receiver loop: ArithmeticException caught by the caller / in the method, AIOOBE, break, return, synchronized, volatile sibling, `this` published to a static, a late override that throws reading `this.f` | `..._FIELD_PROMOTE_METHOD_ENTRY=1`; + `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_POLL_WRITEBACK=1` |
| `R21FzLoopStatic1` | LOOPS21-3 fresh-promotion static feed: static read / written in the loop (also via `Sub.X`), a lazily initialized holder's `<clinit>` inside the loop, counter + fresh cell, long cells | `CRATONVM_JIT_IR_FRESH_PROMOTE_STATIC_FEED=0` (kill); `CRATONVM_JIT_IR_LOOP_STATIC_HOIST=0 CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE=0 CRATONVM_JIT_IR_LICM_STATIC_READ_NOT_BARRIER=0` |
| `R21FzPredSpec1` | LOOPS21-2 per-planter ids: range + class predicates on one header, mask, monotone (stride, decreasing), heap / direct ByteBuffer length predicate; each broken > 4 times in turn, exact exception + faulting trip | `CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION=1` |
| `R21FzPredSpec2` | LOOPS21-2 with the coarse charAt trap sharing the header: `s.charAt(i+off)`, `a[i+off2]`, clamp `b[min(i,lim)]`, CharSequence String / StringBuilder | `CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION=1` |
| `R21FzStrCodePoint1` | CP21-1 `codePointAt` prefix: -1 / length / MIN / MAX indexes (zero-extension), lone / trailing / doubled surrogates, U+10000 / U+10FFFF, LATIN1 >= 0x80, every construction route; SIOOBE text | `CRATONVM_JIT_IR_STRING_CODE_POINT_AT_PREFIX=1` |
| `R21FzStrCompareCI1` | STRINGS21-2 `compareToIgnoreCase` prefix: full 256x256 Latin-1 sweep, 0xB5 (answer `0x3BC - x`), 0xFF, 0xDF, signs, ordinals, length tails, equal-fold reload, null arg, UTF16 / mixed | `CRATONVM_JIT_IR_STRING_COMPARE_TO_CI_PREFIX=1`; + `CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT=1` |
| `R21FzStrCoder1` | STRINGS21-1 `coder()` / `isLatin1()` prefixes and `String.<init>([BB)V` admission through JDK String code (case maps, concat, repeat, strip, indent, join, replace, CI comparator, regionMatches), LATIN1 x UTF16 | `CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT=1`; all three string switches together; `-XX:-CompactStrings` on both VMs if the launcher accepts it |
| `R21FzStrLower1` | ZCP-3 `toLowerCase` unchanged prefix: identity for every byte next to the upper-case ranges, `(Locale) null` NPE on "" , tr / lt / az, UTF16 | `CRATONVM_JIT_IR_STRING_LOWER_UNCHANGED_PREFIX=1` |
| `R21FzTcArray1` | TYPECHECK21-3 `instanceof T[]` matrix: dimensions, primitive arrays, interface components, Cloneable / Serializable / Comparable arrays, late component class | `CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS=1`; + `CRATONVM_JIT_SP_INSTANCEOF_DISPLAY_ARM=1` |
| `R21FzTcArray2` | TYPECHECK21-3 `checkcast T[]`: a false hit surfaces as a later CCE / ASE; CCE messages | `CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS=1`; + `CRATONVM_JIT_CHECKCAST_ENSURE_DISPLAY=1` |
| `R21FzTcAastore1` | IR `aastore` display arm: covariant stores, interface components via superclass / superinterface, arrays into Cloneable[] / Serializable[] / Object[][], ASE messages, null | `CRATONVM_JIT_IR_AASTORE_DISPLAY_ARM=1` |
| `R21FzTcPattern1` | TYPECHECK21-6 checkcast-after-instanceof: negated test, reassigned variable, saved flag, other target, after a join, loop over previous element, `&&`, switch / record patterns | `CRATONVM_JIT_IR_CHECKCAST_AFTER_INSTANCEOF=1` |
| `R21FzTcFinalMiss1` | final-class miss verdict: `List.copyOf` / `Set.copyOf` / `Map.copyOf` identity (the `--compatible` stamps), lambda / Proxy / anonymous receivers vs final targets | `--compatible` is the key arm; `CRATONVM_JIT_INSTANCEOF_FINAL_MISS_VERDICT=0 --compatible` (kill); `CRATONVM_C2_ACCEPT=never` |
| `R21FzTcIface1` | single-pass display arm: diamond / inherited / generic interfaces; casts and instanceof inside the class's own `<clinit>` (display publication) | `CRATONVM_JIT_SP_INSTANCEOF_DISPLAY_ARM=1 CRATONVM_C2_ACCEPT=never`; `..._SP_INSTANCEOF_DISPLAY_ARM=1`; `CRATONVM_JIT_CHECKCAST_ENSURE_DISPLAY=1` |
| `R21FzUpdInt1` | byte-offset field cells via AIFU / ALFU: one offset across three class ids, same slot in two classes, int / long wide bit, every op, accessCheck failures at a hot raw site, a two-thread race | `CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS=1 CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL=1`; that + `CRATONVM_JIT_UNSAFE_RMW_VETO_NEEDS_FIELD_ARM=1 CRATONVM_JIT_IR_SPLICE_NEVER_RUN_PRIVATE_EXIT=1`; control `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS=0` |
| `R21FzUpdRef1` | UPDATERS21-1 reference volatile arm: young values into an old holder across System.gc(), VarHandle release / acquire / opaque, nulls, wrong-type values (valueCheck CCE) | the byte-offset pair + `CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD=1`, with Generational, `-XX:+UseG1GC`, `-XX:+UseZGC`; `CRATONVM_ARFU_VALUE_CHECK=0 --compatible` (kill) |
| `R21FzUpdUnsafe1` | the screen's `k != 0` decline: `getInt(longOff + 4)`, `getByte(intOff + 1)`, `getShort(intOff + 2)`, int / long views, array offsets, superclass offsets on a subclass, CAS / getAndAdd / getAndSet | the byte-offset pair; control `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS=0` |
| `R21FzUpdFactory1` | `--compatible` updater factory and value rules: volatile / type / static / final / missing / nestmate-private / vclass mismatch / null args; legal updaters hot | `--compatible` is the key arm; `CRATONVM_FIELD_UPDATER_JDK_RULES=0 CRATONVM_ARFU_VALUE_CHECK=0 --compatible` (kill) |
| `R21FzIndyBimorph1` (cf) | INDY21-1 bimorphic guard, static leaves: third target, in-loop rotation, throwing target through the second arm, VolatileCallSite, cross-thread flip, flip storm | `CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC=1`; `CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE=1`; both; control `CRATONVM_JIT_INDY_MUTABLE_GUARD=0` |
| `R21FzIndyBimorph2` (cf) | INDY21-1 virtual leaves: dispatch on Sub receivers, interface method target, static target with null receiver, bound (non-direct) target, NPE for null receivers | as `R21FzIndyBimorph1` |
| `R21FzIndyShared1` (cf) | one MutableCallSite linked by several indy instructions with different planned / second cells; a `dynamicInvoker()` target | as `R21FzIndyBimorph1` |
| `R21FzIndyClinit1` | INDY21-2 clinit barrier: a hot reader inside the holder's own `<clinit>` sees every write (int / long / double / byte / boolean / ref); another thread blocks and sees final values; failed `<clinit>` -> EIIE then NCDFE from both threads; a class B read from A's `<clinit>` | `CRATONVM_JIT_IR_CLINIT_BARRIER=1`; `CRATONVM_JIT_GETSTATIC_OWN_CLINIT_FAST=1`; both; `CRATONVM_JIT_STATIC_INIT_RACE_SERVE=0` |
| `R21FzIndyClinit2` | INDY21-3 static-init race serve: eight holders with `static final int[]` tables initialized by four threads while a hot reader compiled before their init spins; exact sums | `CRATONVM_JIT_STATIC_INIT_RACE_SERVE=0` (kill); `CRATONVM_JIT_IR_CLINIT_BARRIER=1 CRATONVM_JIT_GETSTATIC_OWN_CLINIT_FAST=1` |
| `R21FzKeepSplice1` | keep set at a spliced callee's loop exit without a debugger: a CHA break from another thread mid-loop, then `b.v` after the loop; post-call chain read | `CRATONVM_JIT_IR_SPLICED_LATCH_EXIT_READS=0` (kill); `CRATONVM_JIT_IR_KEEP_SET_MODELLED_CHAIN_READS=0` (kill) |
| `R21FzReflName1` | `Class.getName` mirror memo: 90+ mirrors (> 64 lines), arrays of every dimension, primitives, void, nested / local / anonymous / enum-body classes, identity, GC between passes, two threads | `CRATONVM_CLASS_GET_NAME_MIRROR_MEMO=1`, also with `-XX:+UseG1GC` / `-XX:+UseZGC` |
| `R21FzReflInvoke1` | reflective links: widening / unboxing / null-for-primitive / wrong count, box-cache identity, ITE causes, wrong receiver, virtual / default / private targets, ctor link with GC churn, InstantiationException, `<clinit>` on newInstance, reflective `MethodHandle.invoke` | `CRATONVM_JIT_REFLECT_ACCESSOR_LINK=1`; `CRATONVM_JIT_REFLECT_CTOR_LINK=1`; `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN=1`; `CRATONVM_REFLECT_SIG_POLY_UOE=0` (kill) |
| `R21FzWalk1` | StackWalker lines of compiled frames at depth 5 / 60 / 400, RETAIN / skip / limit / getCallerClass, reflection frames with and without SHOW_REFLECT_FRAMES through Method.invoke, Constructor.newInstance and a reflective `Class.forName` `<clinit>` | all walk opt-ins together (`CRATONVM_STACKWALK_DEEP_WALK_LINES=1 _DEFERRED_LINES=1 _PIPELINE_LAYOUT_ONCE=1 _OPTION_ORDINALS_ONCE=1 _PIPELINE_INVOKE_BY_ID=1 _METHOD_SLOT_FRONT_CACHE=1`), then each alone on a diff; `CRATONVM_STACKWALK_REFLECTION_SCREEN_EXACT=0` (kill). Compile with `javac -g`. |

## Notes for the orchestrator

* `R21FzUpdUnsafe1` uses `sun.misc.Unsafe`; JDK 25 prints a one-time deprecation warning on
  stderr (ignore stderr in the diff).
* `R21FzKeepSplice1` and `R21FzLoopElem4` run long single loops (tens of millions of trips) on
  purpose: under `--nojit` they take far longer than 2 s.
* Lines naming JDK-internal frames (`R21FzWalk1` reflect rows) or JDK class names
  (`R21FzReflName1`) are HotSpot 25's; a diff there may be a pre-existing frame-list or naming gap,
  so rerun with every r21 switch unset before attributing it to a wave-1 feature.
* A diff only on a kill arm (`=0` of a default-ON correctness switch) is the defect that switch
  fixed, not a new finding.

# JIT round 21 wave 3, lane fuzz2: probes for the wave-2 shapes

Same conventions and run recipe as above (`javac -d out ...`, HotSpot 25 is the expected output,
deterministic lines only, no timing). Targets: commits `942f8672b3` and `396b57e0c8` (wave 2 and its
hand-backs). Not compiled or run in this lane: expect a first round of javac fixes. (cf) = uses
`java.lang.classfile`; (-g) = compile with `javac -g` (lines are printed).

| probe | targets | arms beyond default / `--compatible` / `--nojit` |
|---|---|---|
| `R21Fz2EaCmp1` | EA21-1 ref-compare fold: every route by which `b` CAN be the fresh `a` (confined holder field, confined array by run-time-equal index, phi, loop save, identity callee, static, checkcast copy, throw/catch, lambda capture, clone, arraycopy, ArrayList, Optional, WeakReference, IdentityHashMap, HashMap, Object/Objects.equals) plus the never-equal rows | `CRATONVM_JIT_EA_REF_COMPARE_FOLD=1`; + `CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM=1 CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD=1` |
| `R21Fz2EaCmp2` | EA21-1 vs non-fresh "allocations": box caches (Integer/Long/Short/Byte/Character/Boolean valueOf, autobox, `new Integer`), string literals / intern, enum valueOf; holders through VarHandle, ARFU, AtomicReference, Field.get, MethodHandles.identity, ThreadLocal, List.of / Arrays.asList / Map.of, Throwable cause / suppressed | `CRATONVM_JIT_EA_REF_COMPARE_FOLD=1` |
| `R21Fz2EaCmp3` | EA21-1 downstream: objects scalar-replaced only because their compare folded, MATERIALIZED at a rare-branch deopt: one identity for aliases / phis / holder fields, identityHashCode, holdsLock, a new interface class at the call site | `CRATONVM_JIT_EA_REF_COMPARE_FOLD=1` (+ `CRATONVM_DBG_SCALAR_NEW=1` on stderr) |
| `R21Fz2EaLock1` | EA21-1 next to lock elision: identityHashCode / hashCode / toString hash agree, holdsLock in / out / nested, wait / notify IMSE texts outside the monitor, a synchronized alias | `CRATONVM_JIT_EA_REF_COMPARE_FOLD=1`; + `CRATONVM_JIT_LOCK_COARSEN=1` |
| `R21Fz2ArrLen1` | ALLOC21-4 length fold: the `newarray` must still throw once its length read folded (NASE "-1" / "-5" / "-3", multianewarray "-2", "Requested array size exceeds VM limit", "Java heap space" at -Xmx256m); 0 / 16 / 64 / 65 / 1 MiB, reference arrays, `new int[3][4]`, static final / spliced-constant lengths, `.length` in a try | `CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD=1`; + `CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM=1` |
| `R21Fz2ArrElim1` | ALLOC21-4 unused-array elimination (9..64 elements): arrays whose only reference is a rare branch's frame state, materialized at the deopt with full length, zero contents, class, single identity; every primitive type, cap edges 8 / 9 / 64 / 65, an OSR loop, a second deopt after a re-warm | `CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM=1`; + `CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD=1` (+ `CRATONVM_DBG_SCALAR_DEOPT=1`) |
| `R21Fz2AnewArr1` | ALLOC21-5 inline `anewarray` on Gen / G1: header (component class, length), all-null body, store checks through `Object[]` views (ASE texts), young arrays holding old and young refs across System.gc(), TLAB-busting 1<<16, a late component class | `CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC=1` with the default collector, `-XX:+UseG1GC`, `-XX:+UseZGC` (no change expected), `-Xmx64m`; run at -Xmx512m |
| `R21Fz2ZgcNew1` | ALLOC21-2 helper-first post-init `new` on ZGC: field defaults before the constructor, finalizable class / subclass / own finalize() all finalized (exact counts), Weak / MyWeak / Phantom references enqueued, soft ones kept, a throwing constructor, a list built under a System.gc() thread | `-XX:+UseZGC` (default ON); `CRATONVM_JIT_IR_ZGC_POST_INIT_HELPER_FIRST=0` (kill); `CRATONVM_ZGC_JIT_HELPER_BEFORE_COMMIT=0`; G1 / default collector (no change) |
| `R21Fz2ZgcNew2` | ALLOC21-2 under contention: four allocating threads + a System.gc() thread, nested `new` inside constructors, identityHashCode at birth, TLAB refills mid-burst; exact checksums | `-XX:+UseZGC`; `..._POST_INIT_HELPER_FIRST=0` (kill); `CRATONVM_ZGC_SKIP_COVERS_INFLIGHT=0`; `-Xmx128m` |
| `R21Fz2InstMem1` | TYPECHECK21-1 part A: loads around a header-only `instanceof` see the stores (field, alias, array, static, the tested field itself, taken arm, loop store + test, a call in the loop), a volatile spin terminates, unread off-chain tests keep their operand's CCE / AIOOBE / NPE | `CRATONVM_JIT_IR_INSTANCEOF_HEADER_READ=1`; + `CRATONVM_JIT_IR_INSTANCEOF_OFF_CHAIN=1`; + `CRATONVM_JIT_IR_CHECKCAST_AFTER_INSTANCEOF=1` |
| `R21Fz2InstMem2` | TYPECHECK21-1 part A, GC side: invariant loads hoisted out of a test-only loop live across the helper's GC point while another thread collects; eight interfaces x eleven classes, a lambda, a Proxy; exact sums | the two switches as above, each with the default collector, `-XX:+UseG1GC`, `-XX:+UseZGC`, `-Xmx64m` |
| `R21Fz2SpAastore1` | TYPECHECK21-5 single-pass `aastore` display arm: covariant stores into superclass / interface / superinterface / generic / Object / Cloneable / Serializable / Object[][] / Number / Comparable / enum / Runnable / Supplier / hidden-class-component arrays; slot and neighbour read back; ASE texts | `CRATONVM_JIT_SP_AASTORE_DISPLAY_ARM=1 CRATONVM_C2_ACCEPT=never` (key arm); the switch alone; + `CRATONVM_JIT_IR_AASTORE_DISPLAY_ARM=1`; control `..._SP_AASTORE_DISPLAY_ARM=1 CRATONVM_JIT_TYPECHECK_DISPLAY_ARM=0` |
| `R21Fz2SpArray1` | TYPECHECK21-3b single-pass `instanceof` / `checkcast` `[LT;` arm: `int[][]` is Object[] / Cloneable[] / Serializable[] but not Number[], `int[]` none, interface / enum / box / String components, lambda arrays, multi-dim; casts used; CCE texts | `CRATONVM_JIT_SP_TYPECHECK_ARRAY_TARGETS=1 CRATONVM_C2_ACCEPT=never` (key arm); + `CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS=1`; + `CRATONVM_JIT_SP_INSTANCEOF_DISPLAY_ARM=1 CRATONVM_JIT_CHECKCAST_ENSURE_DISPLAY=1` |
| `R21Fz2Cat2Join1` (cf) | keepset cat-2 split at a join (ECJ-style bytecode): long hi <- int, double hi <- float, long hi <- ref, long LOW <- int, a long PARAMETER's hi <- int, the split at a loop header; a deopt after the join (new Fn class), live locals before and after the pair | `CRATONVM_JIT_IR_MERGE_CAT2_PAIR_KILL=0` (kill); `CRATONVM_DEOPT_LOCALS_SPLIT_CAT2_TOP=0` (kill); both kills; `CRATONVM_DBG_DEOPT=1` (stderr) |
| `R21Fz2Cat2Deopt1` | the deopt backstop's false-positive side: javac slot reuse across scopes, interleaved long / double / int / ref locals, long params, handler frames, spliced callees with longs, an OSR loop deopted mid-way; every live long exact | `CRATONVM_DEOPT_LOCALS_SPLIT_CAT2_TOP=0`; `CRATONVM_DBG_DEOPT=1` (stderr must not say "treated as TOP"); `CRATONVM_C2_ACCEPT=never` |
| `R21Fz2ThrowCensus1` | THROWS21-1 census skipped: four threads of implicit / explicit / propagated / non-matching / finally / rethrow catches, exact per-thread sums, top frame, a late exception subclass | `CRATONVM_JIT_THROW_CENSUS_STATS_ONLY=1`; + `CRATONVM_DBG_JIT_METHOD_STATS=1` (stderr ignored) |
| `R21Fz2SpliceDeep1` (-g) | splice max-nest clamp: 14-level static / virtual chains and a 12-deep recursion; throws at depth 13 / 12 / 10 / 5 / 0 with full traces, catches at levels 9 and 4, StackWalker at the leaf, a subclass overriding a middle and the leaf level | `CRATONVM_JIT_IR_INLINE_MAX_NEST=12`; + `_FRAME_CLAMP=0` (kill: same lines, slower); + `CRATONVM_JIT_IR_SPLICE_LEAF_PAST_DEPTH=0`; `..._MAX_NEST=8`; `..._MAX_NEST=1` |
| `R21Fz2SpliceDeep2` (-g) | deep chains through JDK code at the clamp: Function andThen / compose tower 14 deep, Optional.map chain, Comparator.thenComparing tower in a sort, default-method chains; throw traces (lambda names normalized), new leaf lambda / L3 classes | as `R21Fz2SpliceDeep1` |
| `R21Fz2SpliceLoop1` | spliced-loop promotion exits: AIOOBE mid callee loop caught by the caller (partial sum), ArithmeticException caught in the callee, early return, aliasing (`acc.addFrom(acc)`), long cell, two call sites of one callee, a call reading `this.sum`, null receiver / null array | `CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED=1`; + `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY=1`; + `CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE=1` |
| `R21Fz2SpliceLoop2` | a deopt INSIDE a spliced promoted loop (a new Val class at trip 0 / 17 / 39 / 3000): instance, static (`Tot.total`), array-element and nested-loop cells; exact sums | `CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED=1`; + `..._ARRAY_ELEMENT_PROMOTE=1`; + `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_POLL_WRITEBACK=1` |
| `R21Fz2SpliceLoop3` | spliced-loop promotion vs the memory model: a volatile store and a monitor in the promoted loop body, a reader thread checking `sum >= T(vcount)` and `locked >= T(ltrips)`; prints violation counts (0) and exact finals; a subclass deopt | `CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED=1`; + `..._METHOD_ENTRY=1`; + `..._POLL_WRITEBACK=1` |
| `R21Fz2UpdBudget1` | updaters' settled-cell screen and publish budget: 12 holder classes x every AIFU / ALFU / ARFU / VarHandle op, classes with own fields at varying slots, a WeakReference / SoftReference subclass's own volatile fields (slots >= 4) with live referents, GC between passes | `CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS=1 CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL=1 CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD=1` with default / G1 / ZGC; the same without `_REF_VOLATILE_FIELD` |
| `R21Fz2ReflCtor1` | REFLECT21-3 ctor link: finalizable instances finalized (exact), WeakReference subclass via newInstance enqueued, trace / walks inside `<init>`, ITE causes (unchecked, checked, Error), widening / null / wrong-type / wrong-count IAE texts, inner / varargs / private / enum ctors, lazy `<clinit>`, a subclass ctor late | `CRATONVM_JIT_REFLECT_CTOR_LINK=1`; + `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN=1`; with `-XX:+UseZGC` / `-XX:+UseG1GC` |
| `R21Fz2ReflHidden1` (cf) | REFLECT21-5 hidden-class links: filterReturnValue / guardWithTest / catchException / asType widening + boxing / recursive invokeExact / foldArguments through LambdaForms, SHOW_HIDDEN_FRAMES walk (normalized), a user hidden class called before its `<clinit>` ran, a new Shape class inside the targets | `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN=1`; + `CRATONVM_JIT_MH_COMPILED_LINK=1`; + the reflect accessor / ctor links |
| `R21Fz2ReflNameReload1` (cf) | getName mirror memo vs class UNLOADING: 12 rounds of eight fresh-named classes in fresh loaders (dropped + collected), > 64 mirrors per round, a twin name in a second loader (interned identity), hidden classes, two threads | `CRATONVM_CLASS_GET_NAME_MIRROR_MEMO=1` with the default collector, `-XX:+UseG1GC`, `-XX:+UseZGC`, `-Xmx32m` |
| `R21Fz2ReflNative1` | `NativeAccessor.invoke0` VM texts: null / wrong receiver, arity, type mismatch, null primitive, widening, ITE, static with junk receiver, private, default method, virtual dispatch, MethodHandle.invoke via reflection, ctor checks | ALL arms with `-Djdk.reflect.useNativeAccessorOnly=true` on both VMs; `CRATONVM_REFLECT_INVOKE0_VM_TEXTS=0` (kill); `CRATONVM_JIT_REFLECT_ACCESSOR_LINK=1`; once without the property |
| `R21Fz2WalkLines1` (-g) | WALK21-4 deferred lines + WALK21-8 front cache: one method walking from four lines, overloads, a 300-deep recursion consumed past the first batch, an OSR loop frame, the same name+descriptor in nine classes, two threads, toStackTraceElement / getDescriptor agreement | `CRATONVM_STACKWALK_DEFERRED_LINES=1`; `CRATONVM_STACKWALK_METHOD_SLOT_FRONT_CACHE=1`; all walk opt-ins together |
| `R21Fz2WalkLines2` (-g) | the three capture doors agree (Throwable, Thread.getStackTrace, StackWalker) on compiled / lambda / stream / ctor / `<clinit>` / overloaded / OSR frames; another thread's parked stack | `..._DEFERRED_LINES=1`, `..._DEEP_WALK_LINES=1`, `..._METHOD_SLOT_FRONT_CACHE=1`, all together; `CRATONVM_STACKWALK_REFLECTION_SCREEN_EXACT=0` (kill) |

## Notes for the orchestrator (fuzz2)

* `R21Fz2ArrLen1` prints HotSpot's two OutOfMemoryError texts; a diff there on every arm, the default
  included, is a pre-existing message gap. Run it at -Xmx256m on both VMs.
* `R21Fz2ZgcNew1`, `R21Fz2ReflCtor1` count finalizations / enqueued references with a bounded
  `System.gc()` retry loop (at most 400 x 2 ms); a "false" / short count after the loop is a finding.
  Do not pass `--finalization=disabled`.
* `R21Fz2ReflHidden1`'s `walk` line lists JDK LambdaForm frames (HotSpot may use the CDS-archived
  `Invokers$Holder` / `DirectMethodHandle$Holder`); a diff there on the default arm too is a
  pre-existing form-shape gap. `R21Fz2SpliceDeep2`, `R21Fz2WalkLines2` print JDK frame names and
  lines: run CratonVM against the same JDK 25 library (`JAVA_HOME`).
* `R21Fz2ReflNameReload1`'s `sameString` (the interned `getName()` across loaders) is HotSpot's
  behaviour; a diff on the default arm too is a pre-existing gap, not the memo.
* `R21Fz2ReflNative1` and the run without the property print NPE messages from the reflection
  doors on purpose (the feature under test). Elsewhere NPEs are printed by class only.
* Long single loops on purpose (slow under `--nojit`): `R21Fz2ArrElim1` `osr`, `R21Fz2Cat2Deopt1`
  `osr`, `R21Fz2SpliceLoop2` `long`, `R21Fz2WalkLines1` / `2` OSR rows, `R21Fz2InstMem2`.
* Page filed from reading: `docs/internal/fixed-bugs/r21-fz-updater-settled-screen-ignores-the-table-epoch-FIXED-20261019.md`
  (perf only; `R21Fz2UpdBudget1` prints answers, which stay right).
