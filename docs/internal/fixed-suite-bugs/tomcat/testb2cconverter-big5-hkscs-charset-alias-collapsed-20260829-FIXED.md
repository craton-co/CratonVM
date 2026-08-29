# `TestB2CConverter.testLeftoverSize` — `Big5-HKSCS` static-init `ArrayStoreException` (FIXED)

**Status: CLOSED — 2026-08-29.**

## What the original doc got wrong

The original finding rooted this in `native-api/src/charset.rs:133`, the
`"BIG5HKSCS" => "Big5"` alias-name fold in CratonVM's *own* transcoding
engine. That engine is never reached here: `TestB2CConverter.testLeftoverSize`
calls only `Charset.availableCharsets()` and `Charset.newEncoder()`, both of
which resolve to the **real JDK's own bytecode** for `sun.nio.cs.Big5_HKSCS`
(a `java.base` class, not `sun.nio.cs.ext` as the original symptom's stack
trace suggested — that detail didn't survive transcription).

`Charset.availableCharsets()` has a `check_override` "force native" entry
(`native_override.rs`), but the native it names,
`native_charset_available_charsets`, was deliberately **unregistered** in
commit `9e104704e` ("fix(tomcat): address Group 16 runtime regressions") —
its `registry.register(...)` call was removed while the (now dead) function
body was left in place. So `Charset.availableCharsets()` actually runs real
JDK bytecode, walking the genuine `CharsetProvider` SPI and returning all 173
charsets HotSpot reports — which is *correct*, and is how the loop ever
reaches `Big5-HKSCS` at all.

## Root cause

`sun.nio.cs.Big5_HKSCS.Encoder`'s static initializer calls
`HKSCS.Encoder.initc2b`, whose first line is:

```java
Arrays.fill(c2b, C2B_UNMAPPABLE);   // c2b: char[][], C2B_UNMAPPABLE: char[]
```

CratonVM's `Arrays.fill(Object[], Object)` native
(`native_arrays_fill_object`, `native-collections/src/lib.rs:22083`) store-checked
this as:

```rust
let comp = ctx.class_id_of_object(arr);      // arr = the char[][] itself
let actual = ctx.class_id_of_object(v);      // v   = the char[] being stored
if actual != comp && !ctx.is_subclass(actual, comp) && ... { throw ArrayStoreException }
```

For an ordinary reference array (`String[]`), `class_id_of_object(arr)`
reports the array's **component** class id by construction (arrays store
their element class id, not a distinct `[L…;` id — see
`array_component_class_id`'s doc comment), so comparing it against the
value's own class id is the right check. But when the component is *itself*
an array — `char[][]`'s component is `char[]`, a primitive array with no
ordinary registered class id — that convention breaks down: `arr`'s reported
"component" id and `v`'s own reported class id are populated by two different
code paths and disagree, so the exact check always refuses and every
`Arrays.fill` of a 2-D (or deeper) array throws `ArrayStoreException` under
CratonVM, identically for `Big5_HKSCS`, `MS950_HKSCS`, and every other
extended charset whose static init does the same `char[][]` fill.

## Fix

`java.lang.reflect.Array.set` already had this exact problem and an existing
fix for it: `reflect_array_element_assignable`
(`native-builtins/src/lib.rs:42818`) falls back to the shared, hardened
`NativeContext::aastore_element_assignable` — the same JVMS §aastore
covariance predicate the interpreter's `aastore` opcode and the JIT's
`jit_aastore` helper use, deliberately *additive* so it never produces a
false `ArrayStoreException` — whenever its own exact class-id check can't
prove the store is legal. `native_arrays_fill_object` never got that
fallback. Added it, mirroring the existing pattern:

```rust
let exact_admits = actual == comp
    || ctx.is_subclass(actual, comp)
    || ctx.class_name_of_id(comp).as_deref() == Some("java/lang/Object");
if !exact_admits && !ctx.aastore_element_assignable(arr, v).unwrap_or(false) {
    return Err(RuntimeError::ArrayStoreException { .. }.into());
}
```

`Arrays.fill(Object[], Object)`'s own deliberately-correct rejection case
(`String[]` filled with an `Integer`) is unaffected — `aastore_element_assignable`
implements the real rule, so it also refuses that store; it only *admits*
cases the naive check couldn't prove, such as a primitive array into a
primitive-array-of-primitive-arrays.

## Validation

Azure host, worktree `/data/cvm-tomcatfs-20260829`
(branch `fix/tomcat-filestore-and-big5hkscs-20260829`), `livedbg` build.

- Standalone probe (`Big5HkscsProbe.java`, walks all `Charset.availableCharsets()`
  entries and calls `newEncoder()` on each, skipping `x-` aliases exactly as
  the Tomcat test does): **173/173, no failures** — matches HotSpot 25.0.3
  exactly (previously: `ArrayStoreException` on `Big5-HKSCS`, first of the
  loop to hit a `char[][]` static init).
- Targeted `Arrays.fill` scope probe (`ArraysFillProbe.java`): `String[]`
  filled with `String` — OK; `Object[]` filled with `String` — OK; `char[][]`
  filled with `char[]` — OK (previously threw); `String[]` filled with
  `Integer` — still correctly throws `ArrayStoreException`. All four match
  HotSpot.
- Real Tomcat suite, `org.apache.tomcat.util.buf.TestB2CConverter` via
  `run-tomcat-suite.sh`'s exact invocation (`JUnitCore`, real classpath,
  `TC_ROOT=/data/cratonvm/apps/tomcat`): **OK (15 tests)** — the class's
  full 15-test run, including `testLeftoverSize`.
- `cargo test --release -p cratonvm-native-collections --lib`:
  **143 passed, 0 failed**.

No known `Big5-HKSCS` / charset-static-init residual remains.
