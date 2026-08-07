# `Method.invoke` has no caller step, so nestmate private invocation is refused

Status: fix written, unbuilt (lane L1 cannot run cargo or the VM).
Applies to: **both** `--real-jdk` (Compatible) and `--jdk-only` (JdkOnly).
HotSpot 25 passes the same class with exit 0, so this is an ordinary
Compatible-mode defect, not a strict-mode policy question.

## The failure

`regression-suite/src/RJdkReflect.java`, both arms, identical output:

```
CK RJdkReflect methods=[compute/1, secret/0, statics/2, thrower/0] fields=[CONST, generic, hidden, pub]
Exception in thread "main" java/lang/IllegalAccessException: cannot access member: modifiers 0x0002, Method.invoke: RJdkReflect$Subject.secret
    at RJdkReflect.main(RJdkReflect.java:425)
    at RJdkReflect.accessAndInvoke(RJdkReflect.java:160)
```

Line 160 is the call **before** any `setAccessible`:

```java
// RJdkReflect.java:157-162
// Subject is a NESTMATE of RJdkReflect, so its private members are
// reflectively accessible from here without setAccessible.
Method secret = k.getDeclaredMethod("secret");
check("s3".equals(secret.invoke(s)), "nestmate private invoke without setAccessible");
secret.setAccessible(true);
check("s3".equals(secret.invoke(s)), "private invoke after setAccessible");
```

`RJdkReflect$Subject` is a `static` nested class of `RJdkReflect`
(`RJdkReflect.java:74`) and `secret()` is `private` (`RJdkReflect.java:93`) —
`modifiers 0x0002` in the message is `ACC_PRIVATE`, which corroborates that the
member reached is the right one and that enumeration already worked (the `CK`
line printed).

**So the hypothesis in the lane brief was wrong in its second half.** The
`setAccessible(true)` override flag is *not* the thing being missed — the test
had not called `setAccessible` yet at the failing line. The missing question is
the caller-class one.

## Root cause

`native_method_invoke` (`native-builtins/src/lang_class.rs:7368-7374`):

```rust
    if !accessible && !is_public {
        check_access(
            modifiers,
            false,
            &format!("Method.invoke: {}.{}", class_name, method_name),
        )?;
    }
```

and `check_access` (`native-builtins/src/lang_class.rs:524-541`):

```rust
fn check_access(modifiers: i32, accessible: bool, member_desc: &str) -> ... {
    if accessible || (modifiers & ACC_PUBLIC) != 0 {
        return Ok(());
    }
    Err(... IllegalAccessException { "cannot access member: modifiers 0x{:04x}, {}" } ...)
}
```

`check_access` takes no `ctx` and therefore **cannot** consult the caller. Its
whole rule is "public, or `setAccessible(true)`, else deny". The real JDK routes
`Method.invoke` through `Reflection.verifyMemberAccess(caller, declaringClass,
obj, modifiers)`, i.e. the ordinary JLS §6.6.1 rules against the caller class,
of which the private arm is `Reflection.areNestMates` (JEP 181).

The field path already grew half of this: `check_field_access`
(`lang_class.rs:557-571`) calls `resolve_caller_class_id(ctx)` and allows the
same-class case (added for HikariConfig's private-final `AtomicReference`). The
method path never got the equivalent, and neither path ever got the nestmate
arm.

### Which of the three gates is actually rejecting

Per `memory/setaccessible-gate-is-three-questions-not-one`, deep reflection here
is three questions. Measured against this failure:

1. **override flag** — `accessible == false` at line 160 by design. Not the bug;
   the test deliberately exercises the no-override path.
2. **caller class (JLS §6.6.1)** — **this is the rejecting gate.** No caller step
   exists on the method path at all.
3. **JPMS `opens`/`exports`** — runs immediately after, at
   `lang_class.rs:7382-7391`, via `check_reflection_module_access(ctx,
   &class_name, accessible=false)`. It **passes** for this case and the fix does
   not touch it: `resolve_caller_class_id` → `RJdkReflect` (application loader,
   so not `caller_is_jdk_internal`), target `RJdkReflect$Subject` resolves, and
   `NativeContextImpl::check_deep_reflection_access`
   (`vm/src/vm/vm_exec.rs:7759-7788`) maps both classes to `module_name == None`
   → `UNNAMED_MODULE`, which `ModuleRegistry::check_deep_reflection_access`
   (`classloading/src/module.rs:736-739`) allows under rule 1 (same module).
   So fixing gate 2 does not merely move the failure to gate 3.

The later assertion at `RJdkReflect.java:167-179` — `setAccessible(true)` on a
`java.base` internal must be refused — is gate 3 on the `setAccessible` entry
point (`enforce_set_accessible_gate`), untouched here. `jdk.internal.misc.Unsafe.getUnsafe`
is `public static`, so `check_access` was never what refused it.

## The change

### In `native-builtins/src/lang_reflect.rs` (new, owned by this lane)

`pub(crate) fn caller_may_access_member(ctx, caller: ClassId, declaring: ClassId, modifiers: i32) -> bool`
plus four private helpers:

* `runtime_package_of` — `(package name, defining loader id)`. JLS §6.6.1
  package access is *runtime* package access, so the loader id is part of the
  key; two `com.foo.Bar` from different loaders are not in one package.
* `confirmed_nest_host_name` — mirrors
  `classloading::access_control::confirmed_nest_host`. A `NestHost` attribute is
  a *claim*; JVMS §5.4.4 requires the claimed host to list the claimant in its
  `NestMembers` before the claim grants anything, otherwise any class could name
  a victim as its host. Unconfirmed → the class is its own host, so the spoof
  fails to match. The host is resolved with `class_id_by_name_near(.., class_id)`
  rather than the ambient `class_id_by_name` so a duplicate binary name from
  another loader cannot be substituted (same trap as
  `check_reflection_module_access_with_target_id`).
* `classes_are_nestmates` — same confirmed host ⇒ nestmates (JEP 181).
* `caller_is_subclass_of` — bounded superclass walk (`MAX_SUPERCLASS_WALK = 128`)
  for the JLS §6.6.2 `protected` arm; a cyclic chain degrades to "deny", not a
  hang.

Decision table, evaluated only after the caller has already been told "not
public and no override":

| member flags | allowed callers |
| --- | --- |
| `ACC_PRIVATE` | declaring class, or a confirmed nestmate |
| package-private | same runtime package (name + loader) |
| `ACC_PROTECTED` | same runtime package, or a subclass of the declaring class |

This is **pure widening**: every input the old blanket rule accepted is still
accepted, because the helper is only reached on inputs the old rule rejected and
every answer it can give is an `allow`.

### In `native-builtins/src/lang_class.rs` (out-of-file for this lane — patch reported to the orchestrator)

One call site, `native_method_invoke`, gains the caller step in front of the
existing `check_access` fallback. The module check that follows is untouched.

## Verify

Once a binary exists:

```
target/release/cratonvm --real-jdk -cp regression-suite/classes RJdkReflect
target/release/cratonvm --jdk-only -cp regression-suite/classes RJdkReflect
```

Both must reach `CK RJdkReflect invoke ok` and exit 0, matching
`java -cp regression-suite/classes RJdkReflect` on Temurin 25.

The narrow signal that the fix landed: the run gets past
`RJdkReflect.java:160` instead of throwing `IllegalAccessException ... modifiers
0x0002 ... RJdkReflect$Subject.secret`.

Negative control — the widening must not open anything: the
`setAccessible`-refusal assertion at `RJdkReflect.java:179` must still pass, and
`probes/SetAccessibleModuleProbe.java` (the 24 paired ALLOW/DENY questions named
in `memory/setaccessible-gate-is-three-questions-not-one`) must still agree with
Temurin 25 in both the bare and `--add-opens java.base/java.lang=ALL-UNNAMED`
arms.

## Known residual

`Constructor.newInstance` and the `Field.get*`/`set*` path have the same
caller-step gap: `check_field_access` allows only the *same class*, not a
nestmate or a same-package caller. `RJdkReflect` does not expose it (it calls
`setAccessible(true)` before every field and constructor use, lines 182/189/193),
so it is left unfixed here rather than changed blind. Routing both through
`caller_may_access_member` is the obvious follow-up.
