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
* **Residual: STILL OPEN — constructor access is unchecked.** This is the item
  re-homed here from L1. `native_constructor_new_instance`
  (`native-builtins/src/lang_class.rs:11017`) has **no** member-modifier gate:
  lines 11017-11400 contain no `check_access`, `check_field_access` or
  `caller_may_access_member` call. The only gates are
  `check_reflection_module_access_with_target_id` (`:11165`) and
  `check_reflection_export_access_with_target_id` (`:11184`), both JPMS. A
  private constructor is still reachable without `setAccessible(true)`.
  Re-grepped 2026-08-12. **Note the shape of the work:** L1's residual named a
  check to *route*; there is no check to route, so this is a **narrowing**, and
  the blast radius is every reflective instantiation in the corpus.
* **Residual: STILL OPEN — hidden classes are never nestmates by this helper.**
  `NativeContext::is_hidden_class` does not exist (`grep -rn "fn is_hidden_class"`
  over `native-api/src` and `native-builtins/src` returns nothing), and
  `confirmed_nest_host_name` (`native-builtins/src/lang_reflect.rs:1439`) has no
  hidden-class exemption — contrast `classloading/src/access_control.rs`. This
  fails **closed**, so it over-denies rather than under-denies.
* **Residual: STILL OPEN — the missing vector.** The targeted probe this record
  asks for (a nestmate field read with **no** `setAccessible`, inserted before
  `regression-suite/src/RJdkReflect.java:182`) was never added; `:181-183` still
  calls `setAccessible(true)` first. The only nestmate-without-`setAccessible`
  assertion in the corpus is the **method** one at `:160`, so the field
  narrowing this record landed is **unexercised**.
* **Cannot adjudicate without a run:** `cargo test -p cratonvm-native-builtins
  field_access_`; `target/release/cratonvm --real-jdk -cp
  regression-suite/classes RJdkReflect`; the same with `--jdk-only`.

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

  That means CratonVM is currently **more** permissive than HotSpot for
  constructors, not less. Adding the missing check would be a *narrowing*, which
  this lane's mandate forbids and which would risk regressing every framework
  that reflectively constructs a package-private type. It is recorded here as an
  open divergence, not fixed. See "Open: constructor access is unchecked" below.

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

### Open: constructor access is unchecked

`Constructor.newInstance` never asks the member-modifier question, so a
`private` or package-private constructor is reflectively reachable from anywhere
without `setAccessible(true)` (subject only to the JPMS check, which for two
classpath classes in the unnamed module always passes). HotSpot refuses this.
Closing it is a **narrowing** and needs its own lane with a corpus A/B — the
likely blast radius is every framework that reflectively instantiates a
package-private implementation type.

### `protected` instance members are widened slightly past HotSpot

`caller_may_access_member`'s `protected` arm asks only
"is the caller a subclass of the declaring class?". The real JDK's
`AccessibleObject.slowVerifyAccess` additionally applies JLS §6.6.2.1 to the
*target object's* class (for a non-static `protected` member the target must be
a subclass of the caller). `check_field_access` never sees the receiver, so that
half cannot be applied here. It only ever admits more, so it is safe under the
widening rule, but it is a real divergence from HotSpot.

### Hidden classes are never nestmates by this helper

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
