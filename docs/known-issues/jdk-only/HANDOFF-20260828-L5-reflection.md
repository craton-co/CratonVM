# L5 — reflection and class metadata: 207 rows — **TAKEN, IN PROGRESS**

**Read `HANDOFF-20260828-SCOPE.md` first.**

> **OWNER: this session.**
> worktree `C:\craton\cratonvm\.claude\worktrees\h2-known-issues-206dee`
> branch `claude/jdk-only-mode-handoff-09b48c`
>
> **Do not take this lane, and do not edit reflection natives in
> `native-builtins/src/lang_class.rs` while it is open.** That is the one file
> where a collision between lanes is likely rather than theoretical.

## Families

```text
java/lang/Class              67 bridge-with-code rows
java/lang/reflect/Field      32
java/lang/ClassLoader        29
java/lang/System$1           29   (the JavaLangAccess impl — reached INDIRECTLY)
java/lang/reflect/Method     26
java/lang/Module             24
                            ---
                            207   (9%)
```

Registrars: `native-builtins/src/lang_class.rs` (28 231 lines) and
`native-builtins/src/lib.rs`, plus `phases_late/reflect_invoke.rs` for `Module`.

## Done so far in this lane

`probes/ClassShadowSweep.java` — 261 rows over the 20 `Class` and 5 `Module`
native-won triples. **9 defects, 8 fixed**, landed in `eeffb12d6`:

* `getPackageName()` returned `""` for all six primitives and `void` where the
  JDK returns `"java.lang"` — six rows, one bug. The check had to go BEFORE the
  memo, since `PACKAGE_NAME_CACHE` is keyed on `ClassId` and a primitive mirror
  has one.
* `Class.getResourceAsStream(null)` answered null instead of throwing NPE.
* `Module.canRead(null)` answered `false` instead of throwing NPE.

Also landed earlier in this lane's area: `int.class.getClassLoader()` returned
the app loader instead of null (`primitive-class-had-a-loader-and-two-deeper-gaps`).

**What passed** — and this is the map of where the work is *not*: every
`getName` / `getSimpleName` / `descriptorString` special case across primitives,
arrays, nested, enum and anonymous classes; all of `forName` (primitives
correctly NOT findable by name, `[I` and `[[I` findable, the slash form
rejected, a null loader scoping to bootstrap); `cast` including its null and
primitive rules; and the whole public-vs-declared split for fields, methods and
constructors.

## Open in this lane

* **`Module.canUse` over-approximates.** `java.base.canUse(Runnable.class)` is
  `true` here and `false` on HotSpot. Documented in the registrar with the
  measurement and the cost. Not fixed because a faithful implementation must
  read the descriptor's `uses` set — and that native exists *precisely because* a
  named `Module` mirror can carry a null `descriptor` field. Fixing it means
  calling back into Java from the native written to avoid touching that field,
  on `ServiceLoader.checkCaller`'s path during `Console.<clinit>`, for one row.
* **`Module.getPackages()` for `java.base` is short** (`< 100`) — recorded in
  `primitive-class-had-a-loader-and-two-deeper-gaps-20260827.md` §3. The
  predicates are all right; the package SET is incomplete.
* **`MethodHandle.invokeExact` does not enforce its exact signature** — recorded
  §4 of the same page. Deliberately not fixed: it needs the call-site descriptor
  to reach the handle's dispatch, which is a change to how `invokeExact` is
  dispatched rather than to a native body.

## Remaining in this lane

`reflect/Field` (32), `reflect/Method` (26), `ClassLoader` (29) and
`java/lang/System$1` (29) are **not yet probed** — 116 of the 207 rows.

`System$1` is the `JavaLangAccess` implementation. It is not callable from Java
and must be reached INDIRECTLY, through the `Module` and `ClassLoader` calls that
route into it. `probes/LoaderModuleSweep.java` already exercises some of that
path and is the place to start.

Edges to aim at: `Field.get`/`set` on a static, a final, a primitive and a
mismatched type (`IllegalArgumentException` vs `IllegalAccessException` — the
two are easy to swap); `setAccessible` on a JDK-internal member under the module
system; `Method.invoke` with a null receiver on an instance method, a wrong
argument count, and an exception thrown by the callee wrapped in
`InvocationTargetException`; `ClassLoader.loadClass` delegation order and
`getResource` relative-vs-absolute naming.
