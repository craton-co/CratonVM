# java.desktop-dependent module clinit hangs under CratonVM

Status: FIXED (verified 2026-07-06).

Found while investigating a reported infinite loop when loading the real
WildFly 32.0.1.Final distribution's `org.wildfly.extension.core-management`
module and triggering its class graph's static initialization. That
module's `module.xml` depends on `java.desktop` ("for java.beans"), unlike
`org.jboss.as.jmx` (no `java.desktop` dependency), which loaded and
returned cleanly in under a second even before this fix.

## Reproduction (original)

```bash
CTRL_JAR=/data/data/wildfly-dist/wildfly-32.0.1.Final/modules/system/layers/base/org/jboss/as/controller/main/wildfly-controller-24.0.1.Final.jar
CP=<dir with a compiled probe class>:$CTRL_JAR
timeout 45 env CRATONVM_JBOSS_MP_ROOT=/data/data/wildfly-dist/wildfly-32.0.1.Final/modules \
  ./cratonvm -cp "$CP" <probe class doing> \
  'new LocalModuleLoader().loadModule("org.wildfly.extension.core-management").loadService(Class.forName("org.jboss.as.controller.Extension"))'
```

Never returned; `ps` showed the VM's `main-vm` thread pegged at ~100% CPU
for as long as observed (up to 180s in one run).

An [earlier StringBuilder count field-slot bug](stringbuilder-count-slot-stomp-infinite-loop.md)
was found and fixed alongside this investigation and eliminated the
associated multi-million-line `gen_heap::set_field` OOB-write log spam,
but did **not** fix the hang itself — the process still spun at ~100% CPU
with the exact same shape after that fix landed. This doc covers the
actual root cause.

## Root cause

Interpreter-mode (`CRATONVM_DISABLE_JIT=1`) `gdb -p <pid> -batch -ex
'thread apply all bt'` on the hung process showed the `main-vm` thread
repeatedly inside `native_sb_append_string` → ... → `execute_frame`'s
bytecode dispatch loop — i.e. a genuine **Java-level** loop, not a Rust-side
spin. Bisecting with a bounded (`i < 8`) probe loop confirmed: draining the
`ServiceLoader<Extension>`'s `iterator()` for `CoreManagementExtension`
correctly found and instantiated exactly one provider (no cross-module
leak — see the sibling
[service-provider-leak fix](wildfly-jboss-modules-service-provider-leak.md)),
but the returned `Iterator`'s `hasNext()` returned `true` forever and
`next()` returned the *same* element (identical `identityHashCode`) on
every call — an ordinary `while (it.hasNext()) { ...; sb.append(...); }`
loop over a **one-element `ArrayList`** never terminated, re-appending the
same ~64-character class name to the same `StringBuilder` indefinitely.
That StringBuilder append is what generated the log spam the sibling fix
addressed — a real but secondary symptom, not the cause.

`vm/src/vm/vm_exec.rs` forces `java/util/ArrayList$Itr.hasNext/next/remove`
through the natives in `native-collections::register_iterator_natives`
whenever the real JDK class isn't available (i.e. CratonVM's own synthetic
`ArrayList$Itr`, the common case for any collection assembled by native
code rather than user bytecode — exactly what `ServiceLoader.iterator()`'s
implementation does). Those natives resolve field slots via
`al_itr_slots`/`al_itr_last_ret_slot`, which look up the *real* JDK 9+
`ArrayList$Itr` field names (`cursor`, `this$0`, `lastRet`) and fall back to
fixed slot indices only when that real class can't be found:

```rust
const AL_ITR_FIELD_LIST: usize = 0;
const AL_ITR_FIELD_CURSOR: usize = 1;
const AL_ITR_NUM_FIELDS: usize = 2;
...
fn al_itr_last_ret_slot(ctx: &dyn NativeContext) -> usize {
    ctx.resolve_field_index("java/util/ArrayList$Itr", "lastRet")
        .unwrap_or(1)   // <-- collides with AL_ITR_FIELD_CURSOR
}
```

`al_itr_last_ret_slot`'s fallback (slot 1) was written against the *real*
JDK layout convention (`cursor` at 0, `lastRet` at 1) but is used against
CratonVM's *own* synthetic fallback layout from `al_itr_slots`
(`list` at 0, `cursor` at 1) — the two fallback conventions disagree, and
slot 1 means something different in each. `native_al_itr_next`'s last two
statements are:

```rust
ctx.set_field(this, cursor_slot, Value::Int(cursor + 1));   // slot 1 = cursor+1
let last_ret_slot = al_itr_last_ret_slot(ctx);               // slot 1 (fallback)
ctx.set_field(this, last_ret_slot, Value::Int(cursor));      // slot 1 = cursor (PRE-increment!)
```

Both fallback slot indices are **1**, so the second write unconditionally
overwrote the first with the stale pre-increment `cursor` value on every
single call — `cursor` was reset to 0 immediately after every increment,
so it never advanced. `hasNext()`'s `cursor < size` check therefore stayed
`true` forever, and `next()` kept returning `elementData[0]`.

Because this collision only exists in the *fallback* (no-real-class)
branch, and only 2 fields are allocated there, it reproduces with nothing
more exotic than a plain, freshly-constructed `ArrayList` iterated in a
loop — no WildFly, no `java.desktop`, needed at all. It just happens that
almost everywhere else in the suite either doesn't call `Iterator.remove()`
(so the corrupted `lastRet` write is harmless — `hasNext()`/`next()` alone
don't touch it) or the collection in question resolves the real JDK class
(giving `al_itr_slots` at least 4 non-colliding slots). This exact
combination — a native-assembled `ArrayList` (not `new ArrayList()` via
user bytecode) drained by a plain `while (it.hasNext())` loop with no
`remove()` call — is common enough (any `ServiceLoader.iterator()`
consumer) that it was silently broken well beyond this one WildFly path.

## Fix

Added a dedicated, non-colliding fallback slot for `lastRet`:

```rust
const AL_ITR_FIELD_LIST: usize = 0;
const AL_ITR_FIELD_CURSOR: usize = 1;
const AL_ITR_FIELD_LAST_RET: usize = 2;
const AL_ITR_NUM_FIELDS: usize = 3;
...
fn al_itr_last_ret_slot(ctx: &dyn NativeContext) -> usize {
    ctx.resolve_field_index("java/util/ArrayList$Itr", "lastRet")
        .unwrap_or(AL_ITR_FIELD_LAST_RET)
}
```

(`native-collections/src/lib.rs`). The real-JDK-class-available branch of
`al_itr_slots` already allocates at least 4 fields, so this only changes
the fallback (synthetic) case, from 2 fields to 3.

## Verification

- Manual before/after repro of the original WildFly command: pre-fix,
  timed out (100% CPU, no return) at up to 180s; post-fix, returns
  correctly and near-instantly with `RESULT:org.wildfly.extension.core.management.CoreManagementExtension`.
- A bounded (`i < 50`) probe confirmed a plain one-element `ArrayList`'s
  iterator now terminates after exactly one element instead of forever.
- `Iterator.remove()` (which depends on `lastRet` at its *new* slot)
  verified still correct: removing every "b"/"d" while iterating
  `[a, b, c, d]` visits all four elements and leaves `[a, c]`.
- New regression test `vm/tests/al_itr_lastret_slot_regression.rs` /
  `vm/tests/al_itr_lastret_probe_fixtures/AlItrLastRetProbe.java` — spawns
  the real `cratonvm` binary as a subprocess under a 15s hard timeout (so a
  regression is a fast test *failure*, not a hung test *run*) and asserts
  both the termination and the `remove()` shapes above.
- `cargo test -p cratonvm-native-collections` — 126 passed, including the
  updated `iterator_field_layout_valid` test (now also asserts the three
  slots are pairwise distinct — the exact invariant this bug violated).
- `cargo test -p cratonvm-native-builtins --lib` — same 14 pre-existing,
  unrelated failures as an unmodified `dev` checkout (`lang_class`,
  `security_manager`, `tls`, `unsafe_jdk25` — nothing touching
  collections); no new failures.

## Caution for anyone reproducing hangs like this

Any repro of an infinite-loop bug MUST run under a hard timeout
(`timeout 30 ...` in shell, or a bounded subprocess wait in a test) — a
naive rerun without one will hang indefinitely and can produce multi-GB
logs in seconds if the loop body also does logging (as this one did, via
the sibling StringBuilder bug's uncapped `set_field` OOB-write guard).
