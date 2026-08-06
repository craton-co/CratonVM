# Native shims in `native-builtins/` — the "shim wins and is wrong" audit

**Status: 🟡 PARTIALLY FIXED 2026-08-01.** Two shims fixed with regression
tests, one registration-time gate built (four tests), one handed-over item
verified-by-reading and pinned with a test, one handed-over item **blocked on a
cross-crate change I could not make** and specified below. Nine census rows are
confirmed correct so a later sweep can skip them; five residuals are left with a
recipe each, two of them in crates this lane does not own.

This is the C2 review's P1 lane where it touches the native overlay.

## The mechanism — why "registered on X" is not the same as "answers for X"

`NativeMethodRegistry::find` is an **exact** `(class, method, descriptor)`
lookup. It does no hierarchy walk. Every instance of this defect family comes
from the *caller*, `try_stackless_invoke`
(`vm/src/runtime/interpreter/invoke.rs:10971`), and three facts about it:

1. **A registered native wins over real bytecode, unconditionally, in the
   default dispatch mode.** `resolve_step1_native` (`:10838`) passes a
   hard-coded `true` for `compat_native_wins`, and its own doc comment says so:
   *"in `Compatible` mode … a registered native wins here"*. Only three later
   guards can veto it — a JVMTI redefine, the `SyntheticStub`-yields-to-real
   allowlist, and the `ThreadPoolExecutor.execute` receiver check.

2. **`class_name` is the RECEIVER's class name**, not the constant pool's
   (`:1833`, *"`invoke_class` is the receiver's class NAME"*).

3. **When the receiver does not declare the method, the walk climbs its
   SUPERCLASS chain, and at each ancestor asks for a native *before* it asks
   whether that ancestor has bytecode** (`:11292`–`:11341`):

   ```text
   let parent = cm.get_class(parent_id)?;
   let has_bytecode = parent.find_method(method_name, descriptor).is_some();
   if let Some(cb) = registry.find(&parent.name, method_name, descriptor) {
       return Some(cb);          // native wins over the parent's own bytecode
   }
   if has_bytecode { return None; }
   ```

Two consequences drive the whole census:

* **A native on an abstract class is inherited by every subclass that does not
  override the method** — JDK, third-party and user subclasses alike — **and it
  beats that base class's real implementation.** This is the high-risk shape.
* **A native on an interface is NOT inherited this way.** The walk follows
  `superclass` only. An interface registration fires only when the receiver's
  runtime class *is* the interface, i.e. a synthetic stand-in minted by
  `ensure_synthetic_class`. Much smaller blast radius. This is why the census
  below is ordered by *abstract class first*, not alphabetically, and why the
  automated gate scopes itself to abstract classes.

Fail closed: a shim that cannot answer correctly for an arbitrary subclass must
**not be registered**, so the real implementation runs. In this VM refusing is
usually strictly better than reimplementing, because the fallback is either the
class's own bytecode or `java/lang/Object`'s natives — both correct.

## Census — natives on inheritance-intercepting base classes

784 of this crate's ~11 000 registrations sit on a class that is abstract or
always subclassed (786 before the two deletions below). Ranked by *what a wrong
answer costs*, not by count.
`mode`: **E** = in the default/real-JDK registry (`register_essential_natives`),
**S** = only in `register_synthetic_overrides` (`#[cfg(feature =
"synthetic-jdk")]`), **ES** = both, **–** = registrar not reachable from either
entry point.

### Tier 1 — identity / equality / ordering semantics on an abstract class

These are the ones where a wrong answer is a wrong *program*.

| shim | registered on | mode | wins over bytecode? | correct when it wins? | action |
| --- | --- | --- | --- | --- | --- |
| `equals`, `hashCode` | `java/util/AbstractMap` | S | **yes** — every `HashMap`/`TreeMap`/`LinkedHashMap`/`EnumMap`/`Collections$UnmodifiableMap`/user subclass inherits both | **NO** — identity, and `hashCode` was `this.as_ptr() as i32`, a raw heap address | ✅ **FIXED — registrations deleted** (`phases_late/collections.rs`) |
| `toString` | `java/util/AbstractMap` | S | yes, same set | partly — `{size=N}` with a *virtual* `size()`, not the JDK's `{k=v, …}` | ⚠️ residual, see *Residual 1* |
| `toString` | `java/nio/ByteBuffer` | ES | **yes** — `HeapByteBuffer`, `DirectByteBuffer`, `MappedByteBuffer` and their read-only siblings all inherit `Buffer.toString` | **was NO** — hard-coded the literal string `java.nio.HeapByteBuffer`, so a direct buffer rendered as a heap buffer | ✅ **FIXED — renders the receiver's own class** (`servlet.rs:6180`) |
| `equals`, `hashCode`, `compareTo` | `java/nio/ByteBuffer` | ES | yes, same set | yes — read the receiver's real storage via `s2_bb_read_window`; `hashCode` iterates backward over `[position, limit)` exactly like `Buffer.hashCode` | ✅ confirmed correct (one fail-open noted in *Residual 2*) |
| `equals`, `hashCode`, `compareTo`, `toString` | `java/lang/Enum` | ES / S | yes — no user enum declares them | yes — the JDK's own `Enum.equals`/`hashCode` are `final` identity, so identity is the *specified* answer; `toString` reads the real `name` field, `compareTo` the ordinal | ✅ confirmed correct |
| `equals`, `hashCode`, `toString` | `java/lang/Record` | S | yes | yes — the real `java.lang.Record` leaves all three **abstract** (JLS 8.10.3), so there is no bytecode to shadow | ✅ confirmed correct |
| `equals`, `hashCode`, `toString`, `clone` | `java/lang/Object` | ES / S | yes, universally | yes — `hashCode` uses `ctx.identity_hash_code` (stable across relocation, unlike a raw address) and `toString` calls `hashCode()` **virtually**, so a receiver that overrides it is rendered with its own value | ✅ confirmed correct — this is the fallback the refusals above depend on |
| `toString` | `java/nio/CharBuffer` | ES | yes | yes — remaining chars from the receiver's own backing store | ✅ |
| `toString` | `java/util/prefs/AbstractPreferences` | S | **yes** — every `Preferences` subclass that does not override it, including user ones | **was NO** — rendered `Preferences[<slot 1>]`, a hardcoded slot index that is not a node name on a foreign subclass, and not the JDK's text on any receiver | ✅ **FIXED — the JDK's own formula through virtual accessors** (`phases_late/beans_jndi.rs`), see *Fix 5* |
| `hashCode` | `java/util/AbstractSet` | — | yes — `TreeSet`, `LinkedHashSet`, `EnumSet`, `Collections$UnmodifiableSet` and user subclasses all inherit it | **suspect** — points at `native_hs_hash_code`, a `HashSet`-layout reader, where the real answer is the sum of element hashes | ❌ **CROSS-CRATE** — `native-collections/src/lib.rs:10499`, see *Cross-crate 1* |
| `toArray` ×2, `contains` | `java/util/AbstractCollection` | — | yes — every Collection that does not override them | **suspect** — `native_al_*`, ArrayList-layout readers | ❌ **CROSS-CRATE** — `native-collections/src/lib.rs:3235`, `:3246`, `:3252` |

### Tier 2 — behavioural natives on an abstract class (state readers, not identity)

Inherited by subclasses the same way, but a wrong answer is a wrong *value*
rather than a broken equality contract. Counted, spot-checked, not individually
re-derived.

| registered on | count | mode | note |
| --- | --- | --- | --- |
| `javax/net/ssl/SSLEngine` | 128 | ES + S | CratonVM *is* the implementation; `SSLEngine` has no useful JDK bytecode to shadow here |
| `java/nio/ByteBuffer` | 66 | ES | storage-aware (`s2_bb_storage`), so heap and direct receivers both work |
| `java/net/HttpURLConnection` | 49 | – | registrar unreachable from either entry point — dead registrations |
| `javax/net/ssl/SSLSocket` / `SSLSession` / `SSLServerSocket` / `SSLSocketFactory` | 88 | ES | same as `SSLEngine` |
| `java/lang/ClassLoader` | 48 | ES + S + – | the loader surface; audited separately in `classloading-identity-audit.md` |
| `java/net/InetAddress` | 34 | ES + – | `Inet4Address`/`Inet6Address` inherit; the shims read the layout-aware side table, so they answer for both |
| `java/nio/CharBuffer` | 30 | ES + S | |
| `java/util/Calendar` | 24 | S | `GregorianCalendar` inherits |
| `java/util/ResourceBundle` | 26 | ES + S | `PropertyResourceBundle`/`ListResourceBundle` inherit |
| `java/security/Provider` | 19 | ES + S | **widest third-party blast radius in the crate** — every JCA provider is a `Provider` subclass, including BouncyCastle |
| `java/security/cert/X509Certificate` / `Certificate` | 21 | ES | |
| `java/lang/Throwable` | 16 | ES + S | every exception class inherits `toString`/`printStackTrace` |
| `java/nio/file/spi/FileSystemProvider` / `FileSystem` | 28 | S | |
| `java/util/logging/Handler` / `Formatter` / `StreamHandler` | 18 | ES + S | `ConsoleHandler`/`FileHandler` inherit |
| `java/lang/ProcessHandle`, `java/lang/Process` | 20 | ES + S | |
| `java/util/concurrent/ForkJoinTask` | 19 | ES + S | `RecursiveTask`/`RecursiveAction`/user subclasses inherit |
| `java/util/TimeZone`, `java/time/ZoneId` | 23 | ES + S | |
| `java/util/concurrent/AbstractExecutorService` | 4 | ES | `submit` ×3 + `invokeAny`; inherited by `ThreadPoolExecutor`, `ScheduledThreadPoolExecutor`, `ForkJoinPool` and third-party executors |
| `java/security/Policy`, `java/lang/reflect/AccessibleObject`/`Executable`, `java/io/Filter*Stream`, `javax/net/*SocketFactory`, `java/net/URLConnection`/`ProxySelector`/`SocketAddress`, AQS/AOS, `java/util/EnumSet`, `java/security/MessageDigestSpi`, `java/nio/Buffer`, `java/nio/channels/SelectableChannel` | ~90 | mixed | no identity/equality methods among them |
| `java/util/prefs/*` | ~30 | S | **moved OUT of the row above on 2026-08-06**: it carried `toString`, so "no identity/equality methods among them" was wrong when written. Now a Tier-1 row, fixed. |

### Tier 3 — interfaces (≈950 registrations, LOW risk, and why)

`java/nio/file/Path` (50), `javax/xml/stream/XMLStreamReader` (49),
`java/util/stream/*` (63), `java/sql/*` (58), `java/util/Set`/`List`/`Map`/
`NavigableMap`/`Iterator`/`Enumeration`/`Collection` (44),
`java/util/function/*` (14), `java/util/concurrent/{Future,Executor,
ExecutorService,Callable,BlockingQueue,locks/{Lock,Condition}}` (54),
`java/lang/{Runnable,Comparable,CharSequence}`, `java/lang/reflect/
InvocationHandler`, `java/lang/annotation/Annotation`, `javax/sql/DataSource`,
`java/util/logging/Filter`, `java/lang/module/*`.

**These do not intercept user subclasses.** The hierarchy walk climbs
`superclass` only, so an interface registration can only fire when the
receiver's runtime class *is* the interface — which in this VM means a synthetic
stand-in minted by `ensure_synthetic_class` for that interface name. That is
also why deleting one is rarely safe: it is often the *only* implementation
those stand-ins have. Not swept; recorded so a later reader does not re-derive
the reachability argument.

## What was fixed

### Fix 1 — `AbstractMap.equals`/`hashCode` were identity shims on an abstract base (HIGH)

`native-builtins/src/phases_late/collections.rs`, in `register_p60_abstract_map`.
They were:

```rust
r.register(am, "hashCode", "()I", |_ctx, args| {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(this.as_ptr() as i32)))
});
r.register(am, "equals", "(Ljava/lang/Object;)Z", /* this.as_ptr() == other.as_ptr() */);
```

Three defects, all reachable *because* `AbstractMap` is an abstract class and
neither `java.util.HashMap` nor `TreeMap`, `LinkedHashMap`, `EnumMap`,
`Collections$UnmodifiableMap` nor any user `class X extends AbstractMap`
declares `equals`/`hashCode` — they all inherit them:

1. **Wrong answer.** Real `AbstractMap.equals` is entry-wise and
   `AbstractMap.hashCode` is the sum of entry hashes. Identity made two maps
   with identical contents unequal *and* gave them different hashes — the
   map-as-a-key contract, inverted.
2. **Unstable hash.** `this.as_ptr() as i32` is a **raw heap address**, not the
   VM's identity hash. Under a moving young collection the object relocates and
   its `hashCode()` silently changes, so a map used as a key is lost from its own
   bucket across a GC. The sibling `Object.hashCode` native
   (`native_object_hash_code`) uses `ctx.identity_hash_code`, which *is* stable
   across relocation — the two natives disagreed about what "identity hash"
   means.
3. **It won over correct bytecode.** With the real `java.util.AbstractMap`
   loaded, its correct entry-wise bytecode was shadowed by (1).

**Refusing beats reimplementing here, in both modes**, which is why nothing
replaces them:

* real `AbstractMap` bytecode present → the walk finds no native on
  `AbstractMap`, sees `has_bytecode`, stops; the real implementation runs;
* bare synthetic `AbstractMap` stub (no bytecode) → the walk continues to
  `java/lang/Object` and lands on `Object.equals`/`Object.hashCode`, i.e.
  identity — the *same answer the shims gave*, minus defect (2).

Regression test (fails before the fix):
`abstract_map_refuses_equals_and_hash_code_but_keeps_its_state_readers`
(`native-builtins/tests/shim_inheritance_guard.rs`), which also asserts the four
registrations that *do* belong there (`isEmpty`, `containsKey`, `containsValue`,
`toString`) survive, so a future "delete the registrar" does not pass by
accident. Backed by
`object_supplies_the_identity_fallback_the_refusals_depend_on`, which pins the
fallback the refusal relies on.

### Fix 2 — `ByteBuffer.toString` told every buffer it was a heap buffer (MEDIUM)

`native-builtins/src/servlet.rs:6180`, in `register_s2_bytebuffer` — **in the
default registry**. `java.nio.ByteBuffer` is abstract and never instantiated
directly; `Buffer.toString()` has real bytecode whose whole body is
`getClass().getName() + "[pos=" …`. The shim answered:

```rust
format!("java.nio.HeapByteBuffer[pos={pos} lim={lim} cap={cap}]")
```

— a hard-coded class name, i.e. a question it could not know. A
`DirectByteBuffer`, `MappedByteBuffer`, read-only sibling or third-party
subclass all rendered as a heap buffer, which is exactly the string a "which
buffer kind is this?" diagnostic reads.

The replacement is three-step and never invents a concrete class name:

1. a **concrete** receiver class (anything but the abstract
   `java/nio/ByteBuffer` itself) *is* the answer — the real-JDK case;
2. CratonVM's own `ByteBuffer.allocate`/`allocateDirect` mint a synthetic
   stand-in stamped with the **abstract** class name (`s2_bb_alloc`,
   `servlet.rs:2705`), so step 1 cannot name it; derive the kind from the
   storage the buffer actually has — the same `s2_bb_storage` source
   `equals`/`hashCode`/`compareTo` read — heap array → `HeapByteBuffer`,
   native window → `DirectByteBuffer`;
3. storage-less bare stub → the abstract class's own name. Unhelpful, but true.

Step 2 exists because `vm/src/vm.rs:28126` (`byte_buffer_to_string`) allocates
through `ByteBuffer.allocate` and asserts the exact string
`java.nio.HeapByteBuffer[pos=0 lim=16 cap=16]`. Without it this fix would have
broken a test in a crate this lane cannot edit — worth stating, because "just
use `getClass().getName()`" is the obvious patch and it is wrong here.

Regression test (fails before the fix):
`byte_buffer_to_string_names_the_receivers_own_class`
(`native-builtins/src/servlet.rs:7256`), which drives four concrete receiver
classes — including a third-party `com/example/VendorByteBuffer`, so the fix
cannot be mistaken for a JDK-name allowlist.

### Fix 3 — handover item 2: the loader-namespace store's GC contract had drifted out of its own doc comment

Verified by reading, **both halves are still wired**:

* `native-builtins/src/classloader.rs:255` `gc_reconcile_defining_loaders` →
  `:308`–`:321` `ns.retain_mut(...)`: drops an entry whose loader was not
  marked this cycle, remaps a survivor that moved through `pointer_map`.
* `native-builtins/src/classloader.rs:174` `gc_scan_loader_singleton_roots`
  deliberately does **not** root the store (rooting it would pin every user
  loader forever and defeat unloading), and
  `gc_update_loader_singleton_refs` (`:225`–`:231`) documents that it must not
  touch it either, because reconciliation already ran with the same survivor
  predicate.

What had rotted was the guard itself. The store's doc comment still described
its *previous* identity-hash-keyed form — *"Holds only `i32 → u32` (no
`ObjectRef`s) — no GC rooting"* — which had stopped being true: it is
`Vec<(ObjectRef, u32)>`. A reader who believed it would conclude there was
nothing for the collector to do here and could delete the `retain_mut` in good
faith. The comment is rewritten to state the two-part contract explicitly, and
the invariant is now pinned by a test rather than by prose:

`loader_namespace_store_is_pruned_and_remapped_by_gc_reconcile`
(`native-builtins/src/classloader.rs`, `classloader_tests`) drives one dead
loader and one relocated loader through `gc_reconcile_defining_loaders` and
asserts the dead entry is gone from both the forward
(`peek_loader_namespace_id`) and reverse (`loader_object_for_namespace_id`)
lookups, and that the survivor answers at its **new** address and not its old
one. Its survivor predicate reports every address it does not own as alive, so
it cannot prune entries another test put in these process-global stores.

This is a **pin, not a fail-before-the-fix test** — the behaviour was already
correct. It is what stops the regression the store's own comment records: an
entry outliving its loader, the address being reused, and a new loader
inheriting the dead one's namespace id **and**, via
`register_user_loader_parent`, its parent link.

### Fix 4 — the registration mechanism now refuses this defect family at build time

`native-builtins/tests/shim_inheritance_guard.rs` (new, 4 tests).

Answering the audit's question "*is there a way to tell, at registration time,
that a native is being attached to a class whose subclasses will inherit the
interception?*": **not from class metadata** — registration runs long before any
class is loaded, so the registry cannot ask whether a name is abstract. What
*is* available is `NativeMethodRegistry::dump_registrations()`, the same census
API the stub ratchet uses. So the gate is a curated table plus a ratchet:

* `INHERITANCE_INTERCEPTING_BASES` — base classes where a registration is
  inherited by every non-overriding subclass **and** where this crate's
  registration set has been fully enumerated, so the allowlist is exhaustive
  rather than hopeful.
* Any registration of `equals`/`hashCode`/`toString`/`compareTo`/`clone` on one
  of them must appear in `ALLOWLIST` with a written reason. Adding one fails CI;
  removing one deletes a row.
* `the_java_util_abstract_collection_bases_carry_no_natives_at_all` pins the
  strictest form for `AbstractList`/`AbstractSet`/`AbstractCollection`/
  `AbstractSequentialList`/`AbstractQueue`: this crate registers **nothing** on
  them, so a user `class X extends AbstractList` inherits only real bytecode.
* Both the default and the `synthetic-jdk` registry are gated (the synthetic arm
  is `#[cfg(feature = "synthetic-jdk")]`, which is where these shims are
  densest).

Deliberately **out of the gate's scope**, with the reason stated in situ, so it
cannot false-fail: `java/lang/Object` (its identity natives are the correct
fallback the fixes depend on — gating it would forbid the fix), and
`java/lang/Throwable`, `java/lang/AbstractStringBuilder`, `java/net/InetAddress`,
`java/security/Provider`, `java/lang/ClassLoader`, `javax/net/ssl/*`, whose rows
are registered through loops over a class-name list rather than string literals,
so a static enumeration of them is not trustworthy enough to freeze. All of them
appear in the census table above with an explicit verdict.

### Fix 5 — `AbstractPreferences.toString` rendered a slot index, and had nothing better to render (MEDIUM)

Caught by the gate from *Fix 4*, not by this census — the census had put
`java/util/prefs/*` in the "no identity/equality methods among them" catch-all,
which was simply wrong: `register_p72_preferences` registers `toString` on both
`java/util/prefs/Preferences` and `java/util/prefs/AbstractPreferences`. The
latter is an inheritance-intercepting base, so the shim answered for every
`Preferences` subclass that does not override `toString`.

It rendered `Preferences[<slot 1>]`. Two things wrong with that, in order of
severity: slot 1 is this VM's own synthetic "name" field, which on a foreign
subclass is some other field or none at all; and even on our own node it is not
what a `Preferences` renders. Real `AbstractPreferences.toString` is

```java
(isUserNode() ? "User" : "System") + " Preference Node: " + absolutePath()
```

The shim could not have produced that, because neither accessor could answer:

* `isUserNode()` **was not registered at all**, and could not have been —
  `userRoot()` and `systemRoot()` both called `p72_alloc_prefs` and returned
  objects that differed in no observable way.
* `absolutePath()` returned slot 1, i.e. the same string as `name()`. A child
  of the root answered `alpha` where the spec says `/alpha`, and a root
  answered `""` where the spec says `/`. The comment on `parent()` directly
  below it already observed that "`absolutePath()`-style upward walks
  terminated immediately" — that walk had never been written.

So the fix is three parts, and the first two are what make the third possible:
slot 5 carries user-vs-system (set by the four static factories, inherited by
`node()` children); `isUserNode()` is registered against it; `absolutePath()`
walks the parent chain. `toString` is then the JDK's formula composed through
**virtual** calls to those two, so a subclass that overrides either is rendered
with ITS answer — which is what earns the allowlist row in
`shim_inheritance_guard.rs` rather than a waiver.
`abstract_preferences_to_string_composes_from_virtual_accessors` pins the two
accessors the row's justification depends on, so a later edit cannot delete one
and leave the row asserting something untrue.

Measured against HotSpot 25 (`PrefsProbe`, 13 printed values): **eleven of
thirteen** matched after this fix. The two that did not are the subject of
*Fix 6*, and they are recorded here rather than rounded off, because the first
draft of this paragraph claimed all thirteen and was wrong.

### Fix 6 — `node()` treated a whole path as one node name, and `AbstractPreferences` was not subclassable (MEDIUM)

The two residuals *Fix 5* left open, and they are one change because the second
is what makes the first testable.

**`node(path)` resolved a single name.** `node("x/y/z")` produced ONE node
literally called `x/y/z`. `absolutePath()` happened to render the same text —
one segment that contains slashes — so a probe that only checked the path saw
nothing wrong. `name()` answered `x/y/z` where HotSpot answers `z`,
`nodeExists("x")` was false immediately after creating `x/y/z`, and
`parent()` skipped two levels. None of the four malformed shapes the JDK
rejects were rejected. It now walks the path:

* a leading `/` resolves from the root of this node's tree, not from this node;
* `""` names this node and `"/"` names the root;
* an empty segment is `IllegalArgumentException`, with the JDK's own two
  messages — `"Path ends with slash"` when it is the last segment,
  `"Consecutive slashes in path"` otherwise;
* a segment longer than `MAX_NAME_LENGTH` (80) is refused, and 80 itself is
  legal.

`nodeExists` walks the same grammar with a lookup-only step, because the two
must agree; and both now raise `IllegalStateException` on a removed node, with
`nodeExists("")` the one query a removed node still answers (`!removed`) rather
than throwing, exactly as the JDK splits it.

The single-segment get-or-create is lifted out of the old closure into
`p72_prefs_child_or_create` so the walk reuses it rather than duplicating the
pinning discipline — every step allocates, and each live reference is carried
across it through `pin_native_root`/`read_native_pin`.

**`AbstractPreferences(AbstractPreferences, String)` is registered**, so
`class X extends AbstractPreferences` is constructible. It was not, and that
is why *Fix 5* could only assert its central claim — "a subclass overrides an
accessor and the rendering follows it" — at the registry level: a subclass
could not be built to test it. Constructor validation is the real one, message
for message (`Root name '…' must be ""`, `Name '…' contains '/'`,
`Illegal name: empty string`). Its slot-5 answer follows the real definition of
`isUserNode()`, which is `root == Preferences.userRoot()`: a node that roots
ITSELF is not in the user tree, so it renders "System" — measured, not assumed,
and the same reason the no-arg constructor's default flipped to 0.

`java/util/prefs/AbstractPreferences` needed its own `instance_fields(6)` row
for the same reason `Preferences` did; a subclass allocated through the new
constructor has to have the slots the constructor writes.

Measured against HotSpot 25: `PrefsPathProbe`'s 18 values — path splitting,
parent chain, `nodeExists` at every level, node memoisation identity, absolute
resolution, all five `IllegalArgumentException` texts, and three subclass
shapes including one that overrides only `absolutePath`/`isUserNode` — are
**identical**, and `PrefsProbe` is now 13 of 13. The path grammar also has
hermetic coverage (`prefs_path_tests`), which asserts that a name of exactly 80
characters is legal rather than only that 81 is refused.

## Residual 5 — `Preferences.userRoot()` returns a FRESH tree on every call

Found while measuring *Fix 6*; **not fixed here**, and it is the more serious
of the two remaining. `userRoot()`/`systemRoot()` allocate a new node per call
instead of answering a per-VM singleton, so the idiomatic
write-here-read-there pattern loses data with no exception:

| | HotSpot 25 | CratonVM `--synthetic-jdk` |
|---|---|---|
| `userRoot() == userRoot()` | `true` | `false` |
| `userRoot().put("k","v")` then `userRoot().get("k","MISSING")` | `v` | **`MISSING`** |
| `userRoot().node("n1")` then `userRoot().nodeExists("n1")` | `true` | **`false`** |

This is the shape this repository keeps filing: the call succeeds, nothing
throws, and the state is gone. It also means real `isUserNode()`'s definition
(`root == Preferences.userRoot()`) could never have been implemented literally
here — the slot-5 flag *Fix 5* added is the model that works without a
singleton, and it stays correct once one exists.

The fix has an established shape in this crate: hold the two roots in a
VM-scoped side table keyed by `ctx.vm_identity()`, registered through
`register_var_handle_root` / read back through `read_var_handle_root`, which is
the documented remedy for a raw `ObjectRef` singleton going stale after a
moving collection (the `ASYNC_POOL` shape). `userNodeForPackage` /
`systemNodeForPackage` then become a `node()` walk of the package path under
the right root, which is what they are in the JDK — today they are four
identical calls to the same allocator and ignore their `Class` argument
entirely.

## Handed-over item 1 — `ensure_synthetic_class` must become fallible: BLOCKED, cross-crate

**The premise is right and the recipe in `classloading-identity-audit.md`
*Open 2* is right. The entry point is not in this crate**, so this lane could
not make the change. Verified by reading:

* `ClassManager::ensure_synthetic_class` — `classloading/src/class_manager.rs:3092`,
  returns a bare `ClassId`. Its fallible sibling
  `try_ensure_synthetic_class` (`:3115`) already exists and returns
  `Result<ClassId, _>` — with **zero callers**.
* `native-builtins` never calls `ClassManager` directly. Every one of its ~35
  call sites goes through the **`NativeContext` trait method**
  `ensure_synthetic_class` — declared at **`native-api/src/registry.rs:3569`**
  with a `ClassId::new(0)` default body, overridden by the VM at
  **`vm/src/vm/vm_exec.rs:12601`**. `native-api/`, `vm/` and `classloading/` are
  all outside this lane's write scope.
* There is no ambiguity channel available to a native today:
  `NativeContext::class_id_by_name` (`native-api/src/registry.rs:578`) returns
  `Option<ClassId>`, which collapses *absent* and *ambiguous* exactly the way
  `get_loaded_class_id` does.

**The change the orchestrator needs to apply, in dependency order:**

1. `classloading/src/class_manager.rs`, `fabricate_class` (`:3197`): replace the
   opening `if let Some(id) = self.get_loaded_class_id(name)` with
   `match self.classify_loaded_name(name)` — `Unique(id)` takes the existing
   early return, `Absent` falls through to fabrication, and **`Ambiguous`
   returns an error** instead of minting a `(Bootstrap, name)` stub that
   outranks both real classes. `classify_loaded_name` / `NameResolution` already
   exist (added by the classloading lane, *Fixed 2*).
2. `native-api/src/registry.rs`, next to `ensure_synthetic_class` (`:3569`): add

   ```rust
   fn try_ensure_synthetic_class(
       &mut self, name: &str, num_fields: usize,
   ) -> Result<ClassId, cratonvm_types::error::MethodCallFailed> {
       Ok(self.ensure_synthetic_class(name, num_fields))
   }
   ```

   The default body keeps every existing implementor (the two test mocks in
   `native-collections/tests/common/mod.rs:1052` and
   `native-io/src/test_support.rs:721`) compiling untouched.
3. `vm/src/vm/vm_exec.rs:12601`: override it to call
   `ClassManager::try_ensure_synthetic_class` and map the
   `ClassNotFoundException` into a `MethodCallFailed`. Leave the infallible
   `ensure_synthetic_class` override delegating to the same place with
   `.expect`, so nothing changes until a caller migrates.
4. `native-builtins/`: migrate the call sites. The ~15 uniform ones —
   `Err(_) => ctx.ensure_synthetic_class(class_name, min_slots)` in
   `agroal_pool.rs:420`, `infinispan_local.rs:834`, `ironjacamar_pool.rs:127`,
   `wildfly_datasources_tx.rs:533`, `keystore.rs:2110`/`:2134`,
   `lang_string.rs:4167`/`:4902`, `lang_system.rs:1381`,
   `regex_matcher.rs:1273`, `wildfly_naming.rs:1496`, `lib.rs:26330` — already
   sit in `MethodCallResult`-returning functions, so they become
   `Err(_) => ctx.try_ensure_synthetic_class(class_name, min_slots)?`. The
   remainder (`lang_system.rs:2157`/`:2366`/`:2402`/`:2403`,
   `util_concurrent_ext.rs:785`/`:820`, `cglib_enhancer.rs:4743`,
   `atomic_updater.rs:241`, `shared_secrets_bridge.rs:192`,
   `reflect_annotations.rs:3483`, `antlr_intrinsics.rs:6056`) need their
   enclosing signature checked one at a time.

Step 1 is the only one that changes behaviour; steps 2–4 are the error channel
it needs. Doing 4 before 1 is a no-op, and doing 1 before 2–4 makes
`fabricate_class`'s `.expect` an abort — so they must land together.

## Residuals, with a recipe

### Residual 1 — `AbstractMap.toString` renders `{size=N}`, not `{k=v, …}`

`phases_late/collections.rs:311` (synthetic registry only). It now reads the
size from a **virtual** `size()` call on the receiver rather than raw slot 1, so
it no longer reports a populated map as empty — but `{size=3}` is still not the
JDK's rendering, and it wins over the real `AbstractMap.toString` bytecode where
that is loaded. Left in place because `vm/src/vm.rs:45748` asserts the exact
string `"{size=5}"`; changing the shim and that assertion is one coordinated
edit across two lanes. **Recipe:** render entries by walking
`entrySet().iterator()` through `invoke_virtual`, fall back to `{size=N}` only
when the receiver has no reachable `entrySet`, and update the `vm.rs`
expectation in the same change.

### Residual 2 — `ByteBuffer.compareTo` answers "equal" when it cannot read

`servlet.rs:6135`–`:6142`: when `s2_bb_read_window` returns `None` (a storage
shape the helper does not understand) the shim returns `0`, i.e. *these two
buffers are equal*. That is a fail-**open** guess in a comparator, and a
comparator that lies about equality corrupts every sorted structure it is used
in. `equals` right above it correctly returns `false` in the same situation.
Not changed here because the only fail-closed answer is to throw, and a
`compareTo` that throws is a behaviour change on a hot path this lane cannot
run. **Recipe:** return an `IllegalStateException` from the `None` arm, then run
the Tomcat HTTP/2 and Jetty NIO suites, which are the buffer-heavy consumers.

### Residual 3 — by-name field reads are not descriptor-aware (crate-wide, unswept)

`ctx.get_field_by_name` appears **1628 times** in `native-builtins/src/`. Per
the standing note
(`native-field-by-name-read-is-not-descriptor-aware`), an **unwritten reference
slot reads back as `Value::Int(0)` by name but `Value::Object(None)` by index**,
so any shim that writes `matches!(ctx.get_field_by_name(o, "f"),
Value::Object(None))` as a null test gets `false` for a field that has never
been assigned — the opposite of the truth. A live instance is visible at
`lib.rs:15540`, `java/lang/Enum.toString`, whose fallback
`Ok(Some(ctx.get_field_by_name(this, "name")))` can hand an `Int(0)` back to a
caller expecting `Ljava/lang/String;`.

Not swept: 1628 sites is a lane of its own, and the failure is only reachable on
the *fallback* arm of each site (the indexed read is tried first almost
everywhere). **Recipe:**

```text
rg -n 'get_field_by_name' native-builtins/src/ | wc -l           # the surface
rg -n -B2 'Value::Object\(None\)\s*(=>|==)' native-builtins/src/ \
  | rg 'get_field_by_name'                                        # the null tests
rg -n -A3 'resolve_field_index\(' native-builtins/src/ \
  | rg 'get_field_by_name'                                        # the fallback arms
```

Then, at each hit: if the field's descriptor is a reference type, compare
against *both* `Value::Object(None)` and `Value::Int(0)`, or resolve the index
first and read by index.

### Cross-crate 1 — `AbstractSet.hashCode` in `native-collections` (SUSPECT, not this lane's file)

`native-collections/src/lib.rs:10499` registers
`java/util/AbstractSet.hashCode()I` → `native_hs_hash_code`, a `HashSet`-layout
reader. `AbstractSet` is abstract, and `TreeSet`, `LinkedHashSet`, `EnumSet`,
`Collections$UnmodifiableSet` and every user `extends AbstractSet` inherit
`hashCode` from it. This is the same shape as Fix 1: the real answer is the sum
of element hashes, read through the receiver's own iterator. **For the
`native-collections` owner:** confirm what `native_hs_hash_code` reads, and if
it is layout-specific either drive it through `invoke_virtual` on
`iterator()`/`size()` or delete the `AbstractSet` registration and keep only the
`HashSet` one.

### Cross-crate 2 — `AbstractCollection.toArray`/`contains` in `native-collections`

`native-collections/src/lib.rs:3235`, `:3246`, `:3252` register
`toArray()`, `toArray([Ljava/lang/Object;)` and `contains` on
`java/util/AbstractCollection`, pointing at `native_al_*` (ArrayList-layout)
helpers. The in-situ comment says this is deliberate — *"register on
AbstractCollection (where the inherited bytecode resolves) so the dispatch path
that walks the superclass chain finds the native before reaching the broken
bytecode"* — i.e. the interception is the *point*. Recorded here because it is
the widest single interception in the workspace: every `Collection` subclass in
the VM, JDK and third-party alike, inherits at least one of these. **For the
`native-collections` owner:** verify `collect_collection_elements` really is
layout-agnostic for a receiver it has never seen, and add the receiver-shape
refusal if it is not.

### Residual 4 — `ScopedValue.hashCode` seeds from a raw address

`jdk25_concurrency.rs:303` derives its hash from `this.as_ptr()`. Unlike the
`AbstractMap` case it **caches the result in a field**, so it is stable after
the first call and a relocation cannot change it. The residual is narrow: the
`h != 0` guard means a seed that hashes to exactly `0` is recomputed on every
call, and after a relocation it would recompute to a *different* value. Cheap
fix: seed from `ctx.identity_hash_code(this)` and store `h | 1`.

## Confirmed correct — a later sweep can skip these

1. **`java/lang/Object`'s identity natives.** `hashCode` →
   `ctx.identity_hash_code` (stable across relocation). `toString` calls
   `hashCode()` **virtually**, so a receiver that overrides it in bytecode is
   rendered with its own value — the previously reported "uses identity rather
   than the virtual hashCode" defect is fixed in the current source
   (`lib.rs:10901`). `equals` is identity plus a documented structural case for
   the synthetic `java.lang.reflect` generic-type stubs.
2. **Interface registrations do not intercept user subclasses.** Derived from
   the walk climbing `superclass` only (`invoke.rs:11302`–`:11341`); ~950
   registrations move from "high risk" to "low risk" on that one fact.
3. **`Enum` and `Record` identity shims match the specified JDK behaviour** —
   `Enum.equals`/`hashCode` are `final` identity in the real JDK, and
   `java.lang.Record` leaves all three abstract.
4. **`ByteBuffer.equals`/`hashCode`/`compareTo` are storage-aware**, so they
   answer for a direct receiver as well as a heap one; `hashCode` iterates
   backward to match `Buffer.hashCode` byte-for-byte.
5. **The loader-namespace side table's GC contract** — see *Fix 3*.
6. **`register_p60_abstract_map` is synthetic-only.** `register_phase60_natives`
   is called at `lib.rs:22915`, inside `register_synthetic_overrides`
   (`:20447`, `#[cfg(feature = "synthetic-jdk")]`), not inside
   `register_essential_natives_with_shims` (`:6667`–`:19964`). This bounds the
   blast radius of Fix 1 to the synthetic-JDK build — which is now a blocking
   gate, so it does ship.
7. **A registered native wins over real bytecode in the default mode** —
   `resolve_step1_native` passes `compat_native_wins: true` unconditionally
   (`invoke.rs:10863`). Do not re-derive; every "does it win?" column above
   rests on it.
8. **`java/util/AbstractList`/`AbstractSet`/`AbstractCollection`/
   `AbstractSequentialList`/`AbstractQueue` carry no `native-builtins`
   registrations at all** — the string literals do not appear anywhere in the
   crate. Pinned by test.
9. **`java/net/HttpURLConnection`'s 49 registrations are dead** — their
   registrar is not reachable from either `register_essential_natives` or
   `register_synthetic_overrides`. Not deleted here (deleting a registrar is a
   separate risk), but do not spend audit time on them.

## How to reproduce / re-run this census

The census is a static read of the registration call sites, resolving
`let`/`const` class-name bindings, cross-referenced against a call-graph
reachability walk from the two public registrars. There is no runtime dump that
distinguishes "abstract" from "interface" at registration time — that is
precisely why *Fix 4* is a curated table rather than a metadata query.

| what | how |
| --- | --- |
| every registration target | `rg -n '\.register(_with_kind\|_field\|_static)?\s*\(' native-builtins/src/` |
| identity shims specifically | filter the above to `"equals"`, `"hashCode"`, `"toString"`, `"compareTo"`, `"clone"` as the second argument |
| is a registrar in the default build? | it must be reachable from `register_essential_natives_with_shims` (`lib.rs:6667`–`:19964`); anything under `register_synthetic_overrides` (`:20447`) is `synthetic-jdk`-only |
| does a native win here? | `resolve_step1_native`, `vm/src/runtime/interpreter/invoke.rs:10838` |
| does a subclass inherit it? | the walk at `vm/src/runtime/interpreter/invoke.rs:11292`–`:11341` — superclasses only, native checked before the ancestor's bytecode |
| the gate | `cargo test -p cratonvm-native-builtins --test shim_inheritance_guard` (and again with `--features synthetic-jdk`) |

## Related

* `docs/known-issues/c2/classloading-identity-audit.md` — *Open 2* is the
  handed-over item 1 above; *Confirmed 3*'s caveat is the handed-over item 2.
* `stub-ratchet.md` — the sibling ratchet this gate is modelled
  on.
* `docs/synthetic-vs-real-explained.md` — why a synthetic stub can win over a
  real JDK class in the first place.
