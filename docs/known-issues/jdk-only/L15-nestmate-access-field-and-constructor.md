# The field reflection path has the same caller-step gap `Method.invoke` had

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Headline: CLOSED in source.** `check_field_access` consults
  `lang_reflect::caller_may_access_member` — commit `8429fcbbc`,
  `native-builtins/src/lang_class.rs:669` (fn), `:696` (the call), `:633`
  (`UNRESOLVED_DECLARING_CLASS_ID`). All four field call sites funnel through
  `field_access_phase` (`lang_class.rs:6683`), and the five new unit tests are
  at `lang_class.rs:23068`, `:23092`, `:23116`, `:23349`, `:23371`.
* **Residual: CLOSED — the `protected` widening. This record still lists it as
  open; it is not.** Commit `dcfe77cb8` gave `check_field_access` a
  `receiver_class_id: Option<ClassId>` parameter and added
  `reflective_target_class_id` (`native-builtins/src/lang_class.rs:732`), which
  is HotSpot's `targetClass` — `Modifier.isStatic ? null : obj.getClass()` —
  shared by all four field entry points, with the JLS §6.6.2.1 refinement in
  `caller_may_access_member`.
* **Residual: CLOSED 2026-08-12 — constructor access is unchecked.** The
  member-modifier gate is written, in `native_constructor_new_instance`
  (`native-builtins/src/lang_class.rs`), immediately **after** the two JPMS arms
  and before the descriptor is composed. It mirrors HotSpot's
  `Constructor.newInstanceWithCaller` -> `checkAccess(caller, clazz, clazz,
  modifiers)` -> `Reflection.verifyMemberAccess`, i.e. the same funnel
  `native_method_invoke` already uses, and routes the same
  `lang_reflect::caller_may_access_member` predicate so the constructor, field
  and method paths cannot drift. The `receiver` argument is `Some(declaring)`
  rather than `None` because a constructor has no receiver and HotSpot passes
  `clazz` as `targetClass`, which reduces the `protected` sub-rule to
  `isSubclassOf(clazz, caller)`. **This is a NARROWING** — see *Blast radius* at
  the end of this record for exactly which callers begin to throw and which
  provably do not. Not built, not run.
* **Residual: CLOSED 2026-08-12 — hidden classes are never nestmates, and this
  record's reason for calling it unfixable was a grep for the wrong name.**
  There is no `NativeContext::is_hidden_class`; there **is**
  `NativeContext::is_class_hidden` (`native-api/src/registry.rs:1601`), and it
  is not a defaulted `false` — `vm/src/vm/vm_exec.rs:8497` reads the real
  `Class::is_hidden()` and `native-builtins/src/test_utils.rs:1771` reads the
  mock's `hidden_classes` set. `confirmed_nest_host_name`
  (`native-builtins/src/lang_reflect.rs`) now carries the hidden-class arm in
  the same position `classloading/src/access_control.rs::confirmed_nest_host`
  puts it (self-host, then hidden, then the `NestMembers` round-trip). Pure
  widening: the arm only ever returns an allow, and it cannot admit anything
  `access_control.rs` does not already admit for the same class at the bytecode
  level. Covered by two paired unit tests in `lang_class.rs`
  (`field_access_allows_private_field_of_a_hidden_nestmate` and its
  non-hidden falsifier) whose fixture deliberately gives the host **no**
  `NestMembers` entry, so the round-trip cannot be what admits the positive.
* ~~**Residual: STILL OPEN — the missing vector.**~~ **CLOSED 2026-08-12
  (W7-78-inherited-residual-closeout.md) — the vector is written; it has NOT yet
  been run against CratonVM.** Four checks added to
  `regression-suite/src/RJdkReflect.java::accessAndInvoke`, immediately before
  the first `setAccessible` on a field, exactly where this record asked for
  them:

  | check | HotSpot 25.0.3.9, measured 2026-08-12 |
  |---|---|
  | nestmate private instance field **get**, no `setAccessible` | succeeds |
  | nestmate private instance field **set**, no `setAccessible` (value restored) | succeeds |
  | nestmate private **static final** field read, no `setAccessible` | succeeds |
  | **non**-nestmate private field read, no `setAccessible` | `IllegalAccessException` |

  The fourth is the falsifier for the other three, and it is why this is not a
  vacuous green: under a gate that admits too much — or no gate at all, which is
  what `Constructor.newInstance` has today — the three positives still pass and
  only that one goes red. Its subject is `RJdkReflectOutsider`, a new
  package-private top-level class in the same compilation unit: **same package,
  different nest**, which separates the `private` rule from the package rule. It
  is deliberately not a separate `src/*.java` file, because `run.sh`'s list
  hygiene requires every one of those to be a listed vector or named in
  `UNREGISTERED_CLASSES`.

  The vector goes **60 → 64 checks** and passes on HotSpot. **It has never been
  run on CratonVM** — the lane that wrote it could not build. If `RJdkReflect`
  goes red at the next suite run, the reading is that the landed
  `check_field_access` narrowing is inert, which is precisely the question
  nothing had ever asked.
* **The CONSTRUCTOR vector, added 2026-08-12 with the narrowing.** Three more
  checks in the same method, `64 → 67`, and — the point — placed **above** the
  existing `priv.setAccessible(true)` block, because that flag short-circuits
  the gate and is exactly what kept the field narrowing unexercised for weeks:

  | check | HotSpot 25 |
  |---|---|
  | nestmate (`Subject`) private ctor `newInstance`, no `setAccessible` | succeeds |
  | same-package package-private ctor (`RJdkReflectOutsider()`), no `setAccessible` | succeeds |
  | **non**-nestmate private ctor (`RJdkReflectOutsider(int)`), no `setAccessible` | `IllegalAccessException` |

  `RJdkReflectOutsider` gains an explicit package-private no-arg constructor and
  a `private RJdkReflectOutsider(int)`; the private one is never called from
  Java and exists only to be reached reflectively. The third row is the
  falsifier: with **no** gate — today's behaviour — the two positives pass and
  only it goes red, so a green run before this change was not evidence of
  anything.
* **Cannot adjudicate without a run:** `cargo test -p cratonvm-native-builtins
  field_access_`; then, with the seven new checks in place,
  `cratonvm --java-home "<jdk-25>" --real-jdk -cp regression-suite/build
  RJdkReflect` and the same with `--jdk-only`. Expect `PASS RJdkReflect
  (67 checks)` on both arms; HotSpot 25 is the control and gives it.

Applies to: **both** `--real-jdk` (Compatible) and `--jdk-only` (JdkOnly).
Follow-up to `L1-reflect-setaccessible-invoke.md`, which fixed the method half
and named this as its known residual.

## What L1 left, and what was actually there

L1's residual note said two paths still had the gap: `check_field_access` and
`Constructor.newInstance`. Reading both:

* **`check_field_access` — confirmed.** It admitted only the *same class*
  (`resolve_caller_class_id(ctx) == Some(declaring_class_id)`), never a
  nestmate, a same-package caller, or a subclass. Fixed here.
* **`Constructor.newInstance` — the brief is wrong.** `native_constructor_new_instance`
  (`native-builtins/src/lang_class.rs:9614`) contains **no member-modifier
  access check at all**. It reads `read_constructor_accessible` (`:9761`) and
  `modifiers` (`:9762`) solely to decide whether to run the JPMS deep check
  (`check_reflection_module_access_with_target_id`, `:9768`); `check_access` is
  never called on that path and neither is `check_field_access`. There is
  therefore nothing to route: a `private` constructor is *already* reachable
  without `setAccessible(true)`.

  That means CratonVM was **more** permissive than HotSpot for constructors, not
  less. Adding the missing check is a *narrowing*, which that lane's mandate
  forbade; it was recorded as an open divergence instead. **Closed 2026-08-12 by
  a lane whose mandate was to write it** — see "CLOSED 2026-08-12: constructor
  access is unchecked" below, and read its *Blast radius* section rather than
  the diff.

## The change

`check_field_access` (`native-builtins/src/lang_class.rs:580-614`) now consults
the same predicate L1 introduced for methods,
`lang_reflect::caller_may_access_member`:

```rust
    if accessible || (modifiers & ACC_PUBLIC) != 0 {
        return Ok(());
    }
    let caller_cid = resolve_caller_class_id(ctx);
    if caller_cid == Some(declaring_class_id) {
        return Ok(());
    }
    if declaring_class_id.as_u32() != UNRESOLVED_DECLARING_CLASS_ID {
        if let Some(caller) = caller_cid {
            if crate::lang_reflect::caller_may_access_member(
                ctx, caller, declaring_class_id, modifiers,
            ) {
                return Ok(());
            }
        }
    }
    check_access(modifiers, false, member_desc)
```

All four field call sites inherit it, so `Field.get`, `Field.set`, and the typed
getter/setter families cannot drift from each other or from `Method.invoke`.

### The `ClassId::new(0)` guard

`read_field_meta` (`lang_class.rs:5222`, `:5225`) collapses an unreadable
`clazz` mirror onto `ClassId::new(0)`, and class ids are handed out from
`classes.len()` (`classloading/src/class.rs:1237`), so `0` is *also* a valid id
— normally `java/lang/Object`. Reading the sentinel as a real declaring class
would make `caller_is_subclass_of(caller, java/lang/Object)` true for every
caller, silently widening `protected` access on a Field object we could not even
decode. `UNRESOLVED_DECLARING_CLASS_ID` skips the entitlement step on that value
so it stays byte-identical to today's behaviour. (The mock context reserves `0`
as its "synthetic / unknown class" id for the same reason —
`native-builtins/src/test_utils.rs:114-117`.)

## Complete enumeration of access-check call sites

`check_access` and `check_field_access` are the only member-modifier access
gates in the crate (grep over `native-builtins/src`; `logmanager.rs:2153`
`native_jboss_log_context_check_access` and `phases_late/nio_file.rs:11645`
`fs_check_access` are unrelated name collisions — a JBoss logging SPI stub and a
filesystem permission probe).

| site | what it gates | disposition |
| --- | --- | --- |
| `lang_class.rs:524` | `fn check_access` — definition, caller-blind | left as the fallback; it is the "deny" leaf both routed paths fall through to |
| `lang_class.rs:613` | `check_access` inside `check_field_access` | the fallback itself — reached only when the caller step declined |
| `lang_class.rs:5505` | `Field.get(Object)` | routed (via `check_field_access`) |
| `lang_class.rs:5612` | `Field.set(Object,Object)` | routed (via `check_field_access`) |
| `lang_class.rs:5725` | `field_get_raw` — `getInt`/`getLong`/… typed getters | routed (via `check_field_access`) |
| `lang_class.rs:5943` | `field_set_raw` — `setInt`/`setLong`/… typed setters | routed (via `check_field_access`) |
| `lang_class.rs:7431` | `native_method_invoke` | already routed by L1; unchanged |
| `lang_class.rs:21231`, `:21252` | existing unit tests | unchanged, still pass (see below) |

Deliberately **not** routed:

* **`Constructor.newInstance`** — no such check exists (see above). Routing
  would mean *adding* a gate.
* **`check_final_for_set`** (`lang_class.rs:452`) — a different question
  (JLS §15.26.1 final-field writability), not caller-sensitive. `Field.set` on a
  `static final` is refused even with `setAccessible(true)`; unchanged.
* **`check_reflection_module_access` / `_with_target_id`** (`:709`, `:724`) and
  `enforce_module_check_on_field` / `_from_mirror` (`:785`, `:813`) — gate 3
  (JPMS `opens`/`exports`), already caller-keyed, a different axis. Untouched:
  widening gate 2 does not bypass gate 3, exactly as L1 argued.
* **`enforce_set_accessible_gate`** (the `AccessibleObject.setAccessible` entry
  point) — gate 3 again, and the thing `RJdkReflect.java:179` asserts. Untouched.
* **`reflect_annotations.rs`** — contains no access check of any kind (no
  `check_access`, no `accessible`, no `IllegalAccessException`). Record accessors
  and `getAnnotation` go through the ordinary `Field`/`Method` natives above.

## Pure-widening argument

1. The entitlement step is unreachable unless `accessible == false` **and**
   `(modifiers & ACC_PUBLIC) == 0` — i.e. only on inputs the old rule had
   already rejected.
2. Its only effect is `return Ok(())`. It cannot turn an accept into a reject.
3. The pre-existing same-class arm is evaluated **first** and left byte-identical,
   so the HikariConfig case cannot regress even if the helper were wrong.
4. `resolve_caller_class_id(ctx) == None` (VM bootstrap, or an all-reflection
   frame stack) falls straight through to `check_access` — fail **closed**,
   unchanged.
5. `declaring_class_id == 0` falls through unchanged (see the guard above).
6. Everything the helper does is a `&self` metadata lookup (`class_name_of_id`,
   `loader_id_of_class`, `nest_host_name`, `nest_member_names`,
   `class_id_by_name_near`, `superclass_of`). None of them re-enters Java, so no
   `ObjectRef` can go stale across the call and no `pin_native_root` is needed.
   `class_id_by_name_near` defaults to the *lookup* `class_id_by_name`
   (`native-api/src/registry.rs:691`), **not** the loading variant.

The two existing unit tests still hold under the change:
`field_access_allows_private_field_from_its_declaring_class` passes on the
untouched same-class arm; `field_access_rejects_private_field_from_another_class_without_override`
uses two classes in the same package `cratonvm/test`, and `private` is
nest-scoped only — the package arm is never reached for `ACC_PRIVATE` — so the
mock (whose `nest_host_name` defaults to `None`, making each class its own nest
host) still denies.

Five new tests pin the new behaviour (`lang_class.rs:21266+`): same-package
package-private allowed; foreign-package package-private denied; foreign-package
`protected` allowed to a subclass; no resolvable caller frame denied;
same-package non-nestmate `private` denied.

## Verify

```
target/release/cratonvm --real-jdk  -cp regression-suite/classes RJdkReflect
target/release/cratonvm --jdk-only  -cp regression-suite/classes RJdkReflect
cargo test -p cratonvm-native-builtins field_access_
```

Both VM arms must reach `CK RJdkReflect invoke ok` and exit 0, matching
`java -cp regression-suite/classes RJdkReflect` on Temurin 25.

**This fix has no regression vector in `RJdkReflect`.** The class calls
`setAccessible(true)` before every field use (`:182`, `:189`) and before the
constructor (`:193`), so the `accessible` short-circuit fires first and the new
code is never reached. Nothing in the corpus exercises it today; the tests above
and `probes/SetAccessibleModuleProbe.java` are the coverage.

A targeted probe would be the missing vector — a nestmate field read with no
`setAccessible`:

```java
Field hidden = Subject.class.getDeclaredField("hidden");
check(hidden.getInt(new Subject()) == 3, "nestmate private field get without setAccessible");
```

inserted **before** `RJdkReflect.java:182`. That is the one line that turns this
from an unexercised widening into a tested one; adding it is out of this lane's
file scope.

Negative control: the `setAccessible`-refusal assertion at
`RJdkReflect.java:179` must still pass, and the paired ALLOW/DENY questions in
`probes/SetAccessibleModuleProbe.java` must still agree with Temurin 25 in both
the bare and `--add-opens java.base/java.lang=ALL-UNNAMED` arms.

## Known residuals

### CLOSED 2026-08-12: constructor access is unchecked

`Constructor.newInstance` never asked the member-modifier question, so a
`private` or package-private constructor was reflectively reachable from
anywhere without `setAccessible(true)` (subject only to the JPMS check, which
for two classpath classes in the unnamed module always passes). HotSpot refuses
this. The gate is now written; what follows is the narrowing's blast radius,
which is the part a reviewer should read rather than the diff.

#### Blast radius — what starts throwing

**The rule.** `IllegalAccessException` is raised when ALL of: the `accessible`
override is unset; the constructor is not `ACC_PUBLIC`; its `modifiers` word was
readable; the declaring class resolved to a `ClassId`; a caller frame resolved;
and `caller_may_access_member` says no. Any one of those failing is an allow.

**What begins to throw.** A `private`, `protected` or package-private
constructor reached by `getDeclaredConstructor(..).newInstance(..)` from a
caller that is not the declaring class, not a confirmed nestmate, not in the
same runtime package, and (for `protected`) not a superclass of the declaring
class. That is precisely the set HotSpot already refuses, so anything in the
blast radius is code that does not run on HotSpot either.

**What provably does NOT move, checked site by site rather than argued:**

* **Every reflective construction in the regression suite.**
  `RFieldSiteCache.java:460,462` (`public Slots(int)`), `RClassUnloadSweep:98`
  and `RJdkFailure:120` (implicit ctors of `public static` classes, hence
  public), `RJdkRecords:118` (canonical record ctor), `RReflect:42`
  (`public Annotated()`) — all public, so the gate returns before asking
  anything. `RJdkReflect:225` and `:281` call `setAccessible(true)` first.
  `RJdkModule:178` already asserts a refusal. `RJdkFieldModule:343`
  (`java.text.CalendarBuilder`, package-private ctor) **already** throws
  `IllegalAccessException` today from the JPMS arm above — java.text is exported
  but not opened — and its `expect(...)` asserts that type, so the row is
  unchanged in outcome as well as in type.
* **`Class.newInstance()`** (deprecated) — the JDK's own body calls
  `setAccessible(true)` on its copied `Constructor` before invoking it.
* **The serialization path.**
  `ReflectionFactory.newConstructorForSerialization` returns from
  `native_constructor_new_instance` far above the gate, matching HotSpot, whose
  serialization constructor is not access-checked either.
* **`Proxy.newProxyInstance`** — the JDK sets accessible when the proxy class is
  not public.
* **Spring / Hibernate / Jackson / ByteBuddy instantiation of non-public types.**
  All of them call `setAccessible(true)`; they have to, because HotSpot refuses
  otherwise.

**Where a NEW refusal could be wrong rather than faithful**, and the valve for
each: an unreadable `modifiers` word (0 would read as package-private — kept as
`Option`, `None` allows); an unresolvable caller frame or declaring `ClassId`
(allows — a **deliberate divergence** from `native_method_invoke`'s `_ => false`,
because there the fail-closed leg was already shipping and here every refusal is
new); a misattributed caller frame from `resolve_caller_class_id` (shared with
`Method.invoke`, which ships, so a defect there is already observable on the
method path).

**Exception type and message.** Type is `IllegalAccessException`, HotSpot's.
The message is CratonVM's house format — `check_access`'s
`"cannot access member: modifiers 0x…, Constructor.newInstance: <class>"` — not
HotSpot's `class A cannot access a member of class B with modifiers "private"`.
That is deliberate: it is the identical helper and identical wording the
already-shipping `Method.invoke` refusal uses, and making the constructor arm
differ from the method arm to chase HotSpot's text would reintroduce exactly the
drift the shared helper exists to prevent. No vector asserts either message; the
suite catches on type.

**Deliberately NOT implemented:** `verifyMemberAccess`'s class-accessibility
half — a member of a non-public class is reachable only from that class's own
runtime package, however public the member.
`enforce_module_check_on_field`'s `public_member_class_is_reachable` is that
rule for fields. Adding it here would refuse the reflective instantiation of
every package-private implementation type from a foreign package, a far larger
blast radius than this record was filed with, and it wants its own measurement.

### `protected` instance members are widened slightly past HotSpot

`caller_may_access_member`'s `protected` arm asks only
"is the caller a subclass of the declaring class?". The real JDK's
`AccessibleObject.slowVerifyAccess` additionally applies JLS §6.6.2.1 to the
*target object's* class (for a non-static `protected` member the target must be
a subclass of the caller). `check_field_access` never sees the receiver, so that
half cannot be applied here. It only ever admits more, so it is safe under the
widening rule, but it is a real divergence from HotSpot.

### CLOSED 2026-08-12: hidden classes are never nestmates by this helper

> **The reason this was recorded as unfixable is wrong, and it is worth naming
> the shape.** The claim was *"`NativeContext::is_hidden_class` does not exist
> (`grep -rn "fn is_hidden_class"` returns nothing)"*. Both halves are true and
> the conclusion is false: the capability exists under a different name,
> `NativeContext::is_class_hidden` (`native-api/src/registry.rs:1601`), with a
> real `ClassManager`-backed impl at `vm/src/vm/vm_exec.rs:8497` and a real mock
> impl at `native-builtins/src/test_utils.rs:1771` — so the arm is exercised,
> not inert against a defaulted `false`. **A grep for one spelling of a
> capability is not a search for the capability.** The verb order in this
> codebase is not stable (`is_class_hidden` / `set_class_hidden` vs
> `class_id_by_name`), so grep the noun.
>
> `confirmed_nest_host_name` now carries the arm, in the same position
> `access_control.rs::confirmed_nest_host` puts it. The paragraph below is the
> original filing, kept because its statement of *why* the hidden claim is
> authoritative is what the new arm's doc comment cites.



`lang_reflect::confirmed_nest_host_name` mirrors
`classloading::access_control::confirmed_nest_host` (`classloading/src/access_control.rs:450`)
**except** for its hidden-class exemption (`:457`): a JEP 371 hidden class's
`nest_host` is authoritative by construction because
`define_class_with_options` overwrites whatever the class file claimed with the
defining `Lookup`'s host, and no `NestMembers` round-trip is possible. L15's
helper has no `is_hidden_class` question to ask through `NativeContext`, so a
hidden class always resolves to itself as host and never matches a nestmate.
This is fail-**closed** — reflection from or into a hidden class stays exactly
as restrictive as today — so it is not a regression, but it means lambda-proxy
and `defineHiddenClass` nestmates do not get the widening. Fixing it needs a
`NativeContext::is_hidden_class` (or a `nest_host_name` that already applies the
exemption VM-side, which would be the better shape).

## Downstream of `RJdkReflect.java:160` — what has never run

L1's failure was at `:160`, so **nothing after it has ever executed** in either
arm. Reading forward, in likely-to-fail order:

* **`:167-179`, the `jdk.internal.misc.Unsafe` refusal.** The highest-risk
  assertion in the file, and it is a *strict* one — the test requires a throw.
  `getUnsafe()` is `public static` in the real JDK, so `check_access` was never
  what refused it; the refusal must come from `enforce_set_accessible_gate` at
  `:171` raising `InaccessibleObjectException`, and the `catch` at `:173` only
  counts it if `getSimpleName()` is exactly `"InaccessibleObjectException"`. Three
  independent ways to fail: the gate does not fire; it fires with a different
  exception type; or in `synthetic-jdk` mode `jdk.internal.misc.Unsafe` is
  fabricated with no module and `check_deep_reflection_access` allows it, in
  which case `m.invoke(null)` returns a non-null Unsafe and `:172` fails first.
* **`:188-190`, `CONST` (`private static final long = 99L`).** Depends on the
  `ConstantValue` attribute being applied at preparation (JVMS §5.4.2) — javac
  emits no `<clinit>` for it. `classloading` does handle `ConstantValue`
  (`class_manager.rs`, `class.rs`), so this is a moderate rather than high risk,
  but it is the only place in the file that reads a compile-time constant
  reflectively and a `0L` here is the signature.
* **`:229-234`, array reflection identity.** `arr.getClass() == int[].class`
  and `getComponentType() == int.class` are pointer-identity assertions on
  primitive-array mirrors — the classic place a VM mints two distinct mirrors
  for the same array type. Related memory: `array-class-and-caller-loader-blindness`,
  `array-receivers-alias-component-class-id-in-inline-caches`.
* **`:242-261`, the inflation loop.** 200 iterations of
  `Constructor.newInstance` + `Method.invoke` + `Field.setInt`/`getInt` past the
  JDK's inflation threshold (15), asserting `acc == 60590`. This is the assertion
  the file was written for: under `--jdk-only` the generated bytecode accessor
  must carry `ClassOrigin::ReflectionAccessor`, neither a compatibility stub nor
  a refusal. A wrong accumulator (rather than an exception) would mean a
  *miscompiled* accessor, which is a much worse finding than a refusal.
* **`:264-296`, annotations.** `:278` demands a `RetentionPolicy.CLASS`
  annotation be invisible at runtime — a filter that is easy to omit; `:283-285`
  wants `getParameterAnnotations()` shaped `[1][1]`; `:274-275` wants proxy
  `equals`/`hashCode` to agree across two separate `getAnnotation` calls.
* **`:298+`, serialization.** `Node` declares `private void writeObject` /
  `readObject`, `transient` skipping, a `serialVersionUID`, and a cyclic `next`
  reference. Reflective invocation of the private `writeObject` from
  `ObjectOutputStream` is itself a nestmate-shaped access, and
  `native_constructor_new_instance`'s `ReflectionFactory.newConstructorForSerialization`
  branch (`lang_class.rs:9650+`) is exercised here for the first time.

None of these are touched by this fix; they are the queue behind it.
