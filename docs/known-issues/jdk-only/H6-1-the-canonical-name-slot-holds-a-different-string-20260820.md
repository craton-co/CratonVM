# H6-1 — `_canonicalName` holds a different string in the two models, which is what makes the JMX P0 row all-or-nothing

**Status: FIXED-UNVERIFIED — no binary carrying these changes has been built or
run.** Three source commits landed in `native-builtins/src/jmx.rs`. Every
"after" statement below is labelled **PREDICTED** with what would falsify it.
The only MEASURED numbers here come from **HotSpot 25.0.3+9 run as the oracle**
and from **`javap -p`**, neither of which involves a CratonVM binary.

Lane H6, 2026-08-20. Worktree
`C:/craton/cratonvm/.claude/worktrees/agent-aed70d02b11e3f330`, branched from
`claude/jdk-only-mode-handoff-09b48c`.

Direct predecessor:
[`H0-1`](H0-1-the-jmx-pin-and-a-jdk-that-was-not-there-20260820.md). This
record does its N2 and N3, and corrects two of its inherited premises.

---

## 0. Before anything else — a base-commit correction that would have wasted the lane

This worktree was created from `26e4b5db4`. The branch it names,
`claude/jdk-only-mode-handoff-09b48c`, was already at `59e5fd8d0`, and
`e300a1fc8` — **the commit carrying all of `H0-1`'s work** — was in the gap.
Verified before touching anything:

```bash
$ git log --all --oneline -- "docs/known-issues/jdk-only/H0-1*"
e300a1fc8 jdk-only H0-1: pin the two JMX registrars, name-resolve ObjectName's
          canonical-name slot, correct the handoff's dead JDK path
$ git branch -a --contains e300a1fc8
+ claude/jdk-only-mode-handoff-09b48c
```

Had the lane started from its own `HEAD` it would have found `object_name_text`
still hard-coding slot `0`, "discovered" H0-1's finding a second time, and
produced a conflicting duplicate of a fix that had already landed — the
`[dup-fix]` shape, arrived at through the base commit rather than through a
merge. Fast-forwarded with `git merge --ff-only` before any edit.

> **The rule.** A worktree's base commit is a snapshot of a branch that keeps
> moving. `git log --all --oneline -- <the predecessor's own file>` costs one
> command and settles whether your predecessor's work is under you or ahead of
> you. `README.md` Rule 1 says do not believe a "not applied"; this is its
> sibling — **do not believe your own tree is current.**

## 1. The JDK, resolved rather than copied

```bash
$ command -v javap
/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot/bin/javap
```

Microsoft, not Eclipse Adoptium. `H0-1` §1 already established this and fixed
`HANDOFF-20260819.md`; it held today. Every `javap` and every oracle run below
used `JDK="$(dirname "$(dirname "$(command -v javap)")")"`.

---

## 2. H6-A — the row's step 2. The answer is **(a)**, and (b) is not merely laborious, it is **unsound**

### 2.1 The layouts, quoted so nobody re-derives them

`javap -p --module java.management javax.management.ObjectName`, JDK 25.0.3+9,
instance fields in declaration order:

```text
  0  private transient java.lang.String                       _canonicalName
  1  private transient javax.management.ObjectName$Property[] _kp_array
  2  private transient javax.management.ObjectName$Property[] _ca_array
  3  private transient java.util.Map<String,String>           _propertyList
  4  private transient int                                    _compressed_storage
```

`javap -p --module java.management 'javax.management.ObjectName$Property'` —
**this is the class the whole decision turns on**:

```text
class javax.management.ObjectName$Property {
  int _key_index;
  int _key_length;
  int _value_length;
  javax.management.ObjectName$Property(int, int, int);
  void setKeyIndex(int);
  java.lang.String getKeyString(java.lang.String);
  java.lang.String getValueString(java.lang.String);
}
```

### 2.2 Three facts, each measured, that together decide it

**Fact 1 — `ObjectName` has no VM leaf at all.**

```bash
$ javap -p --module java.management javax.management.ObjectName | grep -c native
0
```

Zero `ACC_NATIVE` methods. All 23 of `register_object_name`'s registrations
shadow ordinary bytecode; there is nothing in this class that a native *must*
supply. Option (a)'s "keep natives only where a genuine VM leaf is needed"
resolves, for this class, to **keep none**.

**Fact 2 — `_kp_array` and `_ca_array` are not data. They are an index into
`_canonicalName`.** `Property` holds three `int` offsets, and
`getKeyString(String)` / `getValueString(String)` take the name string **as a
parameter**. `ObjectName.java:1558` (`getSerializedNameString`) and its helper
`writeKeyPropertyListString(char[] canonicalChars, …)` both pull substrings out
of `_canonicalName` using those offsets.

**Fact 3 — the two models put DIFFERENT STRINGS in `_canonicalName`.** Measured,
HotSpot 25.0.3+9 on this host (probe source in §7):

```text
new ObjectName("d:b=2,a=1,c=3")
  getCanonicalName()         = d:a=1,b=2,c=3      <- SORTED
  getKeyPropertyListString() = b=2,a=1,c=3        <- SOURCE ORDER
  toString()                 = d:b=2,a=1,c=3      <- SOURCE ORDER

new ObjectName("java.lang:type=GarbageCollector,name=G1 Young Generation")
  getCanonicalName()         = java.lang:name=G1 Young Generation,type=GarbageCollector
  getKeyPropertyListString() = type=GarbageCollector,name=G1 Young Generation
```

`ObjectName.java:1452` is `public String getCanonicalName() { return
_canonicalName; }`, so the real `_canonicalName` is the **key-sorted** string,
and source order survives only in `_kp_array`'s *array ordering*.
`native-builtins/src/jmx.rs`'s `object_name_set_text` stores the **source**
text in that same slot, and
`native_object_name_get_key_property_list_string`'s own doc comment says it
"returns the source-order property list (unlike the canonical accessor, it does
not sort keys)" — i.e. that native is **correct only because** of the
disagreement.

### 2.3 The conclusion, and it is the deliverable

> **(b) — populate `_kp_array` / `_ca_array` / `_propertyList` by hand — cannot
> be done correctly.** Doing it requires `_canonicalName` to hold the canonical
> string, because the Property offsets index into it. Changing `_canonicalName`
> to the canonical string breaks every native in the file at once, starting
> with `getKeyPropertyListString`. Leaving `_canonicalName` as the source text
> and filling the arrays anyway aims the offsets at the wrong characters, so
> real `getKeyProperty()` bytecode returns a **silently wrong substring**
> instead of today's NPE. **A wrong answer that looks like an answer is worse
> than the null it replaces**, and that is a defect class this directory has
> measured before (`[default=wrong write]`, `[decline masks]`).

So **(a)** — and (a) is **all-or-nothing for this class**. There is no subset of
`jmx.rs` that can be converted independently, because all 23 natives share the
one `_canonicalName` convention. That is not a preference; it is the shape of
the object.

**I was asked to check the brief and I am confirming it, with a sharper reason
than the brief gave.** `H0-1` N3 said (b) "needs `ObjectName$Property`
instances, which means the parse the real constructor does" — true, and an
argument from *effort*. The argument from *soundness* above is stronger and it
also rules out the tempting hybrid (fill the three fields, keep the text model)
that an effort argument leaves open. I nearly implemented that hybrid before
reading `Property`.

### 2.4 (a) already has a switch in the tree, and it is one environment variable

Nothing in `jmx.rs` needs to change to *perform* (a). Source-verified:

* `vm/src/vm/vm_exec.rs:1090`, `resolve_native_dispatch_wave1`:
  `NativeKind::Bridge if bytecode_available => { record…; None }` — under
  `--jdk-only` a `Bridge` yields to concrete bytecode.
* `vm/src/runtime/interpreter/native_override.rs`, `resolve_step1_native`:
  the yield is gated on
  `enforce = strict_bridge && jdk_only_enforce_shadow_for(class_name)`.
* `vm/src/runtime/env_cache.rs`, `parse_enforce_shadow_scope`:
  `CRATONVM_ENFORCE_NATIVE_SHADOW` is **`Off` when unset**, and accepts a
  comma-separated prefix list. That file's *own tests* use
  `"javax/management/"` as the worked example, twice
  (`enforce_shadow_scope_parses_the_three_spellings`,
  `enforce_shadow_scope_covers_only_the_named_subsystem`).

So `CRATONVM_ENFORCE_NATIVE_SHADOW=javax/management/` under `--jdk-only` makes
every `ObjectName` bridge yield to the real constructor and the real accessors,
which fills all five fields correctly for free. **This also means the P0 row's
premise needs one more correction: under a plain `--jdk-only` run today the 23
bridges still RUN.** They are *observed* as `NativeShadowsBytecode` and not
enforced away. The row reads as though strict mode already prefers the bytecode;
it does not, by default, on the step-1 path that answers nearly every dispatch.

### 2.5 The blocker that must be cleared BEFORE that dial is armed — and it is not in `javax/management/`

`native_platform_managed_object_name` calls `object_name_new`, which fabricates.
It is registered on **`java/lang/management/*` and `jdk/management/*`
interfaces** (`register_platform_managed_object_names`), so:

1. the `javax/management/` prefix does not cover it; and
2. `step1_dispatch_has_code` is documented to answer `false` for an abstract
   resolution, and these bind abstract interface methods — so **even
   `CRATONVM_ENFORCE_NATIVE_SHADOW=all` leaves this native in charge.**

With the dial armed, real `javax.management` bytecode would then receive an
`ObjectName` whose `_kp_array` is null and NPE on `_kp_array.length`. **Arming
the dial without first making `object_name_new` construct through real
`<init>(Ljava/lang/String;)V` bytecode makes strict mode worse, not better.**

`native-api/src/registry.rs` already has the exact primitive for that:
`invoke_special_bytecode_only` — its doc comment is written for "a native that
IS ITSELF the native registered for `(class_name, method_name, descriptor)` and
must run that class's own real bytecode body directly", skipping the native
re-find that would otherwise re-enter.

### 2.6 What H6 landed for H6-A, and what it deliberately did not

**Not landed:** the `object_name_new` → real-`<init>` conversion, and the 23
retirements. Reason, stated rather than hedged: converting `object_name_new`
alone flips `_canonicalName` from source to canonical order for `getInstance`
and the eight platform beans, which regresses `getKeyPropertyListString()` and
`toString()` on `java.lang:type=GarbageCollector,name=…` against the oracle
output in §2.2. Fixing that requires converting the order-sensitive accessors in
the same change — a four-to-twenty-three-site object-model migration that this
lane is **forbidden to compile**. `G88-1` §5 measured the half-migrated object
model twice; shipping an unbuilt one to avoid a paragraph of prose is the trade
this record declines.

**Landed instead**, all oracle- or `javap`-backed:

**(i) A real divergence, fixed.** `native_object_name_get_serialized_name_string`
returned `canonical_object_name_text(..)`, and its doc comment asserted that the
serialized name "is simply the canonical name text (domain + sorted key
properties)". `ObjectName.java:1652` is `public String toString() { return
getSerializedNameString(); }`, and §2.2 measures `toString()` as **source
order**. So the serialized name is the source-order text and the native was
wrong — on every name whose keys were not already sorted, which is the jmxmp
wire path `RKC-ObjectName-03` exists for. Now returns the text model verbatim.

**(ii) The premise pinned in code** at `object_name_set_text`, quoting the
`Property` layout and the oracle output, so the next lane cannot reach for (b).

**(iii) Two in-tree comments that were wrong about the tree**, corrected in
place with what replaces them — see §5.

---

## 3. H6-B — the per-site table

**The distinction is the finding, so it is stated per site rather than
assumed:** an index into a carrier CratonVM itself allocated is legitimate; an
index into a real JDK layout is not. The discriminator is whether the stamped
class has a real layout behind it, and for the MXBean family the answer is
uniform and checkable — **they are all interfaces**:

```bash
$ javap --module java.management java.lang.management.RuntimeMXBean … | grep -E '^public'
public interface java.lang.management.RuntimeMXBean extends …PlatformManagedObject {
public interface java.lang.management.ThreadMXBean extends …
public interface java.lang.management.ClassLoadingMXBean extends …
public interface java.lang.management.OperatingSystemMXBean extends …
public interface java.lang.management.CompilationMXBean extends …
public interface java.lang.management.GarbageCollectorMXBean extends …MemoryManagerMXBean {
public interface java.lang.management.MemoryMXBean extends …
public interface java.lang.management.PlatformLoggingMXBean extends …
public interface javax.management.MBeanServer extends javax.management.MBeanServerConnection {
public interface jdk.management.VirtualThreadSchedulerMXBean extends …
```

An interface has **zero instance fields**, so every one of those carriers is
CratonVM's own invention and its indices cannot collide with anything.

| site (line, post-H6) | receiver class | real or fabricated | verdict |
|---|---|---|---|
| `object_name_text` / `object_name_set_text` (~1035, ~1052) | `javax/management/ObjectName` | **REAL** (5 fields) | converted by `H0-1` §3; unchanged here |
| `object_name_table_pairs` 1117–1136 | `java/util/Hashtable`, `java/util/HashMap` + their nodes | **REAL** | **left, with reason — see §4** |
| `getMemoryUsage0` 2264–2276 | `java/lang/management/MemoryUsage` | **REAL** (4 fields) | **converted** (`memory_usage_slots`) |
| `undefined_memory_usage` ~3009 | `MemoryUsage` | **REAL** | **converted** |
| `alloc_memory_usage` 4591–4594 | `MemoryUsage` | **REAL** | **converted** |
| `MemoryUsage.<init>()V`, `<init>(JJJJ)V`, 4 `()J` getters, `toString` shim | `MemoryUsage` | **REAL** | **converted** |
| `platform_mxbean_object_name_text` 3935 (slot 0 = collector name) | `java/lang/management/GarbageCollectorMXBean` | interface → fabricated | left; index legitimate |
| `init_runtime_mxbean_fields` 4032–4046, 4091 (slots 0..9) | `java/lang/management/RuntimeMXBean` | interface → fabricated | left; index legitimate, comment added |
| `init_runtime_mxbean_fields`, the `inputArguments` list | **`java/util/ArrayList`** | **REAL** | **CONVERTED — this was live heap corruption, see §3.1** |
| `RuntimeMXBean` getters 4112–4170 | `RuntimeMXBean` | interface → fabricated | left; index legitimate |
| `alloc_memory_mxbean` 4536 | `sun/management/MemoryImpl` | **REAL** | already guarded by `is_class_synthetic_stub(..)`; left, correct |
| `register_memory_mxbean` 4620–4665 (slots 0..4) | `java/lang/management/MemoryMXBean` | interface → fabricated | left; index legitimate |
| `init_thread_mxbean_fields` 5428–5433, getters 5485–5500 | `java/lang/management/ThreadMXBean` | interface → fabricated | left; index legitimate |
| `ClassLoadingMXBean` 6036–6093 (incl. `CLM_VERBOSE`) | `java/lang/management/ClassLoadingMXBean` | interface → fabricated | left; index legitimate |
| `OperatingSystemMXBean` 6139–6194 | interface | fabricated | left; index legitimate |
| `CompilationMXBean` ~6263–6302 | interface | fabricated | left; index legitimate |
| `VirtualThreadSchedulerMXBean` ~6349–6378 | interface | fabricated | left; index legitimate |
| `GarbageCollectorMXBean` ~6423–6474 | interface | fabricated | left; index legitimate |
| `MBS_*` family (~6544–8060) | `javax/management/MBeanServer` | interface → fabricated | left; index legitimate, already named constants |
| `build_synthetic_hash_set` ~6748 | **`java/util/HashSet`** | **REAL** | **left, with reason — see §3.2** |
| `ThreadInfo` / `LockInfo` / `MonitorInfo` family | real classes | **REAL** | **already fully by NAME — see §3.3** |
| `MemoryType` enum fallback (~3226) | `java/lang/management/MemoryType` | **REAL** enum | already by name; latent trap, NOMINATION N4 |
| `AbstractOwnableSynchronizer` owner 2795/2809 (pre-H6 numbering) | real | **REAL** | already name-resolved with a bounded memo; exemplary |

### 3.1 The one live heap-corruption site found, and fixed

`javap -p java.util.ArrayList` + `java.util.AbstractList`, JDK 25.0.3+9:

```text
  0  protected transient int      modCount      (AbstractList)
  1  transient java.lang.Object[] elementData   (ArrayList)
  2  private int                  size          (ArrayList)
```

`init_runtime_mxbean_fields` wrote:

```rust
ctx.set_field(args_list, 0, Value::Object(Some(empty_arr)));  // -> modCount   (an int)
ctx.set_field(args_list, 1, Value::Int(0));                   // -> elementData (a REFERENCE)
```

Against a real `ArrayList` — which is what `try_alloc_concurrent_synthetic`'s
`max(real, requested)` widening hands back in real-JDK mode — that is an oop in
an int field and an `Int` in a reference field the GC scans as an oop. It is
`docs/architecture/natives-over-real-jdk-classes.md` §5's species verbatim, in a
class §5 does not name: *"`Int(0x5F)` in either is a bogus pointer for the
collector to mark and move."*

**The shape worth carrying:** the file's other two `java/util/ArrayList`
allocations — the component-list registration and
`init_notification_emitter_support` — **already wrote by name.** This was the
one call site that did not. `[1 of 10 callsites]` inverted: the correct pattern
was the majority and the exception was invisible because a whole-file grep for
`set_field_by_name` looks healthy.

Converted with the `H0-1` §3 pattern — resolve the index by name,
`unwrap_or(<synthetic index>)` — and **not** with `set_field_by_name`, which is
a documented no-op on an absent field and would have silently dropped both
writes on the synthetic carrier that mints fields with no names.

### 3.2 `build_synthetic_hash_set` — left, and why

`javap -p java.util.HashSet`: the real instance layout is **one** field,
`transient HashMap map` (`AbstractSet`/`AbstractCollection` declare none;
`PRESENT` is static). The site writes slot 0 (`map`) with an `Object[]` and
slot 1, which does not exist on the real class.

Left, because its own doc comment scopes it: *"Only used if a real
`java.util.HashSet` cannot be constructed in this context (e.g. a unit-test mock
with no JDK classes)"* — `build_real_hash_set` is the live path. This is
`[plat=reach]`: the site is wrong-shaped but unreachable when the real class
exists, and converting it would change the behaviour of a fallback whose only
job is to work where names do not resolve. **Recorded rather than "fixed", and
nominated (N3) for a `is_class_synthetic_stub` guard of the kind
`alloc_memory_mxbean` already carries — that is the in-tree exemplar for this
exact situation and it is four lines away in the same file.**

### 3.3 A correction to `H0-1` N2 and to this lane's own brief

Both said the remaining sites include "`ThreadInfo` and the lock family".
**They do not.** `alloc_basic_thread_info`, `alloc_named_thread_info`,
`alloc_jmx_lock_info` and the `MonitorInfo` builder write **every** field
through `set_field_by_name` already. Verified:

```bash
$ grep -n "ThreadInfo\|LockInfo\|MonitorInfo" native-builtins/src/jmx.rs | grep "set_field\|get_field"
(no output)
```

Quoted anyway so the next lane does not re-`javap` it —
`javap -p java.lang.management.ThreadInfo`, 18 instance fields:

```text
 0 threadName String | 1 threadId long | 2 blockedTime long | 3 blockedCount long
 4 waitedTime long   | 5 waitedCount long | 6 lock LockInfo | 7 lockName String
 8 lockOwnerId long  | 9 lockOwnerName String | 10 daemon boolean | 11 inNative boolean
12 suspended boolean | 13 threadState Thread$State | 14 priority int
15 stackTrace StackTraceElement[] | 16 lockedMonitors MonitorInfo[] | 17 lockedSynchronizers LockInfo[]
```

`LockInfo` = `0 className String | 1 identityHashCode int`; `MonitorInfo extends
LockInfo` = `0 className | 1 identityHashCode | 2 stackDepth int | 3 stackFrame
StackTraceElement`.

**The estimate "~40 index sites" was a count of `get_field`/`set_field` calls,
not a count of sites addressing a real layout.** Of 141 such calls in the file,
**four clusters** touch a real JDK layout: `ObjectName` (done by H0-1),
`MemoryUsage` (done here), one `ArrayList` write (done here), and
`object_name_table_pairs` (§4). Everything else addresses an interface-stamped
carrier. A raw grep over-counts this backlog by roughly an order of magnitude,
and the over-count is what makes the row look unfinishable.

---

## 4. CROSS-LANE HAZARD — `object_name_table_pairs` and lane H4

`object_name_table_pairs` walks a `Hashtable`/`HashMap`'s **private internal
node layout by index**. Its in-file comment claims *"Both store their buckets in
field 0 and chain entries through field 3; the entry's key/value slots differ
only because a real `Hashtable$Entry` has its hash in slot 0."* **That claim is
accurate** — `javap -p`, JDK 25.0.3+9:

```text
java.util.Hashtable        0 table Entry[] | 1 count int | 2 threshold int
                           3 loadFactor float | 4 modCount int | 5 keySet | 6 entrySet | 7 values
java.util.HashMap          0 table Node[] | 1 entrySet | 2 size int | 3 modCount int | 4 loadFactor float
java.util.Hashtable$Entry  0 hash int | 1 key | 2 value | 3 next
java.util.HashMap$Node     0 hash int | 1 key | 2 value | 3 next
```

Buckets at 0 and `next` at 3 hold for both node types, and the
`matches!(get_field(node, 0), Value::Int(_))` real-vs-synthetic discriminator is
sound because `hash` is an `int` in both.

**It is correct by coincidence of two other classes' private layouts, and lane
H4 is concurrently making the collection containers real.** Per the brief I did
not work around it and did not touch H4's files. What the orchestrator needs:

* If H4 changes CratonVM's own node shape so that slot 0 of a synthetic node can
  hold an `Int`, the discriminator inverts and this walk silently reads
  `(hash, key)` as `(key, value)` — every `ObjectName(String, Hashtable)`
  constructed from a synthetic table then gets wrong properties, with no
  exception anywhere.
* If H4 makes `Hashtable`/`HashMap` real *and* this walk is still reached, it
  reads a real private layout by index. It happens to be right today. Nothing
  pins it.
* The durable fix is not in `jmx.rs`: it is to stop walking node internals and
  drive `entrySet().iterator()` instead. That is a behavioural change across a
  GC-sensitive loop and belongs to whichever lane owns the container semantics.

**Neither lane can verify the pair alone.** `[fix+fix≠]` — the combination is
what is untested.

---

## 5. Where existing records, rows and in-tree comments were wrong about the tree

Each backed by a command, per the brief.

| claim | where | correction | evidence |
|---|---|---|---|
| "`_ca_array` … an OUT-OF-BOUNDS read of it yields `null`" | `jmx.rs`, RKC-ObjectName-01 block | mechanism gone. `try_alloc_concurrent_synthetic` widens to `max(real, requested)`, so a real `ObjectName` gets 5 slots; the read is in bounds against a genuinely null slot | `util_concurrent_ext.rs`, `let n = num_fields.max(real);` — **corrected in place, commit `90e4305fe`** |
| "any write past slot 0 is silently discarded" | P0 row, `jdk-only-runtime-services.md:91` | stale, same reason. `H0-1` §3 already said so for the allocation half; independently re-verified | same line — **row not edited (not my file), see §8 N1** |
| "`getSerializedNameString` … is simply the canonical name text (domain + sorted key properties)" | `jmx.rs`, that native's doc | **false.** `ObjectName.java:1652`: `toString() { return getSerializedNameString(); }`, and HotSpot's `toString()` is source order | oracle run, §2.2 — **fixed, commit `90e4305fe`** |
| "`type=GarbageCollector,name=<n>` … is the order `getCanonicalName()` sorts to, so the text is already canonical" | `jmx.rs`, `platform_mxbean_object_name_text` | **false.** Canonical sorts by key; `name` < `type`. It is the only multi-property text that function builds, so it was the one case the claim had to get right | oracle: `getCanonicalName() = java.lang:name=…,type=GarbageCollector` — **corrected in place, commit `90e4305fe`** |
| "`ThreadInfo` and the lock family" still index-based | `H0-1` N2, and this lane's brief | already 100% by name | `grep` in §3.3 |
| "~40 index-based sites remain" | `H0-1` §4 / N2 | 141 calls, but only **four clusters** address a real layout; the rest are interface-stamped carriers | §3 table |
| the row reads as though strict mode prefers bytecode over a JMX bridge today | P0 row remedy step 3 | it does not, by default: `CRATONVM_ENFORCE_NATIVE_SHADOW` is `Off` and step 1 only *observes* the shadow | `env_cache.rs::parse_enforce_shadow_scope`, §2.4 |
| `register_as` is the API for step 1 | P0 row remedy step 1 | does not exist; re-confirmed today | `grep -rn "fn register_as" native-api/src` → no match. `H0-1` §2 found this first |

One record claim **checked and found RIGHT**, recorded because Rule 1 cuts both
ways: `H0-1` §3's statement that `try_alloc_concurrent_synthetic` widens is
accurate, and its refusal to pad the synthetic carrier to 5 is the correct
direction under "convert, verify, unpad, then drop".

---

## 6. Commits

| SHA | what |
|---|---|
| `90e4305fe8a1e894184741e0fafddb1074811168` | H6-A: `getSerializedNameString` returned the canonical name where HotSpot returns source order; two in-tree comments corrected; the `Property`-offsets premise pinned at `object_name_set_text` |
| `98e4811158aa0109fd275f29a77e89cf3bb8cb30` | H6-B: `RuntimeMXBean.inputArguments` wrote an oop into `ArrayList.modCount` and an `Int` into `elementData` |
| `c33ba71fc67d19c7ed6891aed0bb51bcd2bb3cb2` | H6-B: `MemoryUsage`'s four slots by name (`memory_usage_slots`), 8 sites |

One bean/decision per commit, per `H0-1` N2's rule.

---

> **VERIFIED AGAINST A BINARY 2026-09-02.** §7.1's oracle probe — "Run this
> against CratonVM too; it is the differential that decides §7.3" — has been run.
> It is transcribed verbatim into `probes/ONProbe.java`. **The differential is
> ZERO**, on both arms:
>
> ```text
> A getCanonicalName         = java.lang:name=G1 Young Generation,type=GarbageCollector
> A getKeyPropertyListString = type=GarbageCollector,name=G1 Young Generation
> A toString                 = java.lang:type=GarbageCollector,name=G1 Young Generation
> A getCanonicalKeyPropList  = name=G1 Young Generation,type=GarbageCollector
> B getCanonicalName         = d:a=1,b=2,c=3
> B getKeyPropertyListString = b=2,a=1,c=3
> B toString                 = d:b=2,a=1,c=3
>
> HotSpot vs CratonVM compatible   IDENTICAL
> HotSpot vs CratonVM --jdk-only   IDENTICAL
> ```
>
> The three-way distinction this record is about is exactly what the B rows pin,
> and all three are right: `getCanonicalName` SORTS the keys (`a=1,b=2,c=3`),
> `getKeyPropertyListString` preserves INSERTION order (`b=2,a=1,c=3`), and
> `toString` preserves the ORIGINAL spelling. A slot holding the wrong one of
> those three would show here, and none does.
>
> **§7.2's prediction also holds, by a route it did not anticipate.** It predicts
> `RJdkJmx` "stays green and moves nothing". `RJdkJmx` passes when run alone in
> both modes. It DOES appear in the failure list of a full `SUITE=all` run — but
> that is one of six vectors shown to fail only inside a full-suite run and to
> pass alone in either mode, which is the harness rather than the mode or the
> vector. See
> [`the-suite-ab-that-was-the-harness-20260902.md`](the-suite-ab-that-was-the-harness-20260902.md).
> Taken from the full-suite list alone, `RJdkJmx` would have read as this
> record's prediction failing.

## 7. VERIFICATION PLAN

### 7.1 The oracle probe, so it can be re-run

```java
import javax.management.ObjectName;
public class ONProbe {
    public static void main(String[] a) throws Exception {
        ObjectName n = new ObjectName("java.lang:type=GarbageCollector,name=G1 Young Generation");
        System.out.println("A getCanonicalName         = " + n.getCanonicalName());
        System.out.println("A getKeyPropertyListString = " + n.getKeyPropertyListString());
        System.out.println("A toString                 = " + n.toString());
        System.out.println("A getCanonicalKeyPropList  = " + n.getCanonicalKeyPropertyListString());
        ObjectName m = new ObjectName("d:b=2,a=1,c=3");
        System.out.println("B getCanonicalName         = " + m.getCanonicalName());
        System.out.println("B getKeyPropertyListString = " + m.getKeyPropertyListString());
        System.out.println("B toString                 = " + m.toString());
    }
}
```

`"$JDK/bin/java" ONProbe.java`. Output is quoted in §2.2. **Run this against
CratonVM too** — it is the differential that decides §7.3.

### 7.2 Which vectors should move — and `RJdkJmx` alone is NOT sufficient

**PREDICTED: `RJdkJmx` stays green and moves nothing.** It is the obvious vector
and it cannot see any of this lane's three changes. Read from
`regression-suite/src/RJdkJmx.java`:

* line 106 asserts `getCanonicalName()` sorts keys — already correct, and
  unaffected;
* line 238 is `check(rt.getInputArguments() != null, …)` — a **null check**. The
  `ArrayList` corruption of §3.1 produces a non-null list, so this passes both
  before and after;
* `getKeyPropertyListString`, `toString()` and `getSerializedNameString` are
  **not asserted anywhere in the file**, so the §2.6(i) divergence is invisible
  to it;
* `MemoryUsage` is exercised at 243–246 by an `init <= committed` invariant,
  which is order-preserving under a slot permutation of equal-typed longs.

*Falsified if* `RJdkJmx` changes verdict at all: that would mean one of these
three changes altered behaviour on a path this record claims it does not touch,
and the change should be reverted and re-derived rather than re-baselined.

**What would actually verify it**, in order of value:

1. **`ONProbe` differentially, CratonVM vs HotSpot, stdout only** (`[stdout only]`
   — `2>&1` puts VM tracing in the diff). Before commit `90e4305fe`, line `B
   toString` should already agree and `getSerializedNameString` is unreachable
   from the probe — so **add a fourth line** driving serialization
   (`new ObjectOutputStream(...).writeObject(m)` then read back) or call
   `toString()` on a name round-tripped through jmxmp. Without that line the
   §2.6(i) fix has **no vector at all**, which is itself the finding: a
   divergence with no test is why it survived.
2. **`--dump-native-registry`**: the 25 `javax/management/ObjectName` +
   `ObjectInstance` rows must still read `bridge`, and `registered_by` must
   still name `register_object_name` / `register_object_instance` (H0-1 §2's
   falsifier — this lane changed no registration, so any movement there is a
   merge artefact, not this lane).
3. **The whole-suite `--jdk-only` arm**, as the verdict-neutrality control.
   **PREDICTED: unchanged, and the shadow count unchanged at whatever H0-1's
   control measured.** No registration was added, removed or retagged, so per
   `HANDOFF-20260819.md` §1 strict mode cannot move. *Falsified by* any change in
   the `--jdk-only` shadow census.
4. **A GC stress run of `RuntimeMXBean.getInputArguments()`** is the only thing
   that can positively confirm §3.1, and it is the hardest: the pre-fix bug is a
   bogus oop the collector marks and moves, so it manifests as a crash
   *somewhere else*, later. If the orchestrator wants a witness rather than an
   argument, the cheap version is `--jdk-only` + a forced young GC after
   `ManagementFactory.getRuntimeMXBean().getInputArguments()`, on the **pristine
   parent commit** — `[ctrl@orig commit]`, credit the fix only after the
   original commit reproduces.

### 7.2b Why the in-file unit tests cannot move — checked, not assumed

This lane could not build, so the mock's behaviour under the new
`resolve_field_index_by_class_id` calls was established by reading rather than
running. `[mock=slot table]` — `MockNativeContext` answers that method from a
name-to-slot table, so a conversion from index to name can silently change what
a unit test measures.

```bash
$ grep -n '"init"\|"used"\|"committed"\|"max"\|"elementData"\|"size"' \
      native-builtins/src/test_utils.rs
(no output)
```

None of the six names appears in any of the mock's tables. `java/util/ArrayList`
is `instance_fields(4)` in `synthetic_stub_field_model`
(`classloading/src/class_manager.rs:12497`) — **unnamed** `_f0.._f3` — so
`mock_stub_model_field_slot` cannot match `elementData`/`size`;
`java/lang/management/MemoryUsage` is not modelled at all; and
`mock_jdk_field_slot`, the only class-agnostic arm, carries none of the six.

**So every new lookup in this diff resolves to `None` under the mock and takes
the `unwrap_or(<old fixed index>)` fallback.** PREDICTED: the `jmx.rs` test
module is bit-identical before and after. *Falsified by* any test in that module
changing verdict — which would mean a table arm was missed and the mock is now
measuring a different slot than the VM.

Note this cuts the other way too, and it is the reason N5 matters: a unit test
under the mock **cannot** exercise the real-layout branch these conversions
exist for. The mock proves the fallback is unchanged; only a real-JDK run
exercises the fix.

### 7.3 The (a) experiment, which needs no code

Once §2.5's blocker is cleared, the whole of (a) is one A/B in one binary:

```bash
CRATONVM_ENFORCE_NATIVE_SHADOW=javax/management/ CRATONVM_ARGS="--jdk-only" …
```

**Do not run it before clearing §2.5** — the platform-bean `getObjectName()`
natives are outside the prefix *and* bind abstract methods, so they keep
fabricating while everything around them goes real, and the run will NPE for a
reason that has nothing to do with whether (a) works. A red result there would
be read as "(a) is wrong", which it would not be. `[setup lies]`.

---

## 8. Does this close the P0 row? **No.**

`H0-1` §4 said the row does not close on step 1. It does not close on this
either. What is left, in one list:

1. **Step 2's layout half is still open.** `_kp_array`, `_ca_array` and
   `_propertyList` are still null on every `ObjectName` CratonVM fabricates.
   §2.3 establishes they cannot be filled in place; the remedy is step 3.
2. **Step 3 is untouched.** Real `java.management` bytecode is not running. §2.4
   names the switch and §2.5 names the one blocker in front of it.
3. **`object_name_new` must construct through real `<init>` bytecode** before
   the dial is armed. This is the single highest-value remaining code change in
   the row and it is bounded to one function.
4. **The row's own closure rule 1 is unmet and independent of all of the above.**
   It requires a *reviewed* bridge, and **no
   [`jdk-only-native-review.md`](../../jdk-only-native-review.md) §4 completeness
   review has been run on the 203 JMX bridges.** The row itself notes the census
   fields that review needs do not exist yet. **Nothing in wave H can close this
   row; it is gated on a census, not on code.** Anyone reading a green JMX
   vector as closure is hitting README Rule 4.
5. **The row's text needs three corrections** before it can be read at face
   value — `register_as`, "write past slot 0", and the implicit claim that
   strict mode already prefers bytecode (§5).

---

## 9. OUT-OF-FILE EDITS REQUIRED

**O1 — `docs/jdk-only-runtime-services.md`, line 91, the `JMX real path` row.**
Three corrections, all evidenced in §5. Not applied: not this lane's file.

* current: `…so `_ca_array` is permanently null and any write past slot 0 is silently discarded.`
* replace with: `…so `_ca_array` is permanently null. (CORRECTION, H0-1 §3 and H6-1 §5: "any write past slot 0 is silently discarded" is STALE — `try_alloc_concurrent_synthetic` widens every allocation to `max(real_instance_field_count, requested)`, so a real `ObjectName` gets all five slots. The fields are null, not absent.)`

* current: `**1.** Pin `register_object_name` and `register_object_instance` with `register_as``
* replace with: `**1.** DONE 2026-08-20 (H0-1 §2). Pinned with `set_category`/`current_category`; `register_as` has never existed in this tree.`

* current: `**3.** Then run real `java.management` / `javax.management` bytecode, keeping only the VM-native leaves plus reflection and class-definition services.`
* replace with: `**3.** Then run real `java.management` / `javax.management` bytecode. `javax.management.ObjectName` declares ZERO native methods (`javap -p … | grep -c native` → `0`), so for that class "keep the VM-native leaves" means keep none. The switch already exists — `CRATONVM_ENFORCE_NATIVE_SHADOW=javax/management/`, `Off` by default, so today's `--jdk-only` run still RUNS all 23 bridges and only observes them. **Precondition (H6-1 §2.5): `object_name_new` must construct through real `<init>(Ljava/lang/String;)V` first** — `native_platform_managed_object_name` is registered on `java/lang/management/*` interfaces, outside that prefix and on abstract methods, so it keeps fabricating even under `=all`.`

**O2 — `classloading/src/class_manager.rs:13585`.** `"javax/management/ObjectName" => instance_fields(1)`. Not an error today (the widening covers real-JDK mode), and it is the *synthetic* carrier's width, which the object-layout audit's "unpad" step wants shrunk rather than grown. **Flagged, not requested:** do not raise it to 5 — that pads the synthetic carrier in the wrong direction, exactly as `H0-1` §3 argued for the allocation request. It becomes deletable when step 3 lands.

**O3 — none for H4's files.** §4 is a hazard report, not an edit request.

---

## 10. NOMINATIONS

* **N1 — apply O1.** A remedy whose step 1 names a function that never existed,
  and whose evidence column carries a premise the tree disproved, reads as "not
  done yet" forever. `H0-1` N1 raised half of this and it is still unapplied,
  which is the second time this exact row has outlived a correction.
* **N2 — `object_name_new` → real `<init>` bytecode**, via
  `invoke_special_bytecode_only`, gated on
  `resolve_field_index_by_class_id(cid, "_kp_array").is_some()` (ask the CLASS,
  not the value — `natives-over-real-jdk-classes.md` §4). **It must land in the
  same change as the order-sensitive accessors**, which are exactly three and
  are enumerated: `toString`, `getKeyPropertyListString`,
  `getSerializedNameString`. Every other accessor canonicalises on read or is
  order-insensitive, so the set is closed — this is the migration's full extent,
  measured, not estimated. A lane that can BUILD should take it.
* **N3 — `build_synthetic_hash_set`** should carry the
  `is_class_synthetic_stub("java/util/HashSet")` guard that
  `alloc_memory_mxbean` already carries four hundred lines away in the same
  file, for the identical reason. §3.2.
* **N4 — the `MemoryType` enum fallback** (`jmx.rs`, `getMemoryType0`'s `else`
  arm) writes `set_field_by_name(e, "name", …)` onto a freshly fabricated
  synthetic enum carrier. Synthetic carriers mint fields with **no names**, and
  `set_field_by_name` is a documented no-op on an absent field — so on the only
  carrier that arm ever builds, the write is silently dropped and the comment
  above it ("`toString()` … reads the `name` field at slot 0") describes a value
  that was never stored. Same family as the nameless-enum-constant defect. Not
  fixed here: it is the fallback of a fallback and deserves its own reachability
  argument first.
* **N5 — `RJdkJmx` has no assertion for `toString()`,
  `getKeyPropertyListString()` or a serialization round-trip**, and its
  `getInputArguments` check is `!= null`. Both defects this lane fixed were
  invisible to the named P0 vector. **A vector that cannot fail on the row it is
  named for is not evidence about that row** — §7.2 lists the four lines that
  would change that.
