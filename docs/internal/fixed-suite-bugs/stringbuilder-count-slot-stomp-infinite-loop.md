# StringBuilder count field-slot stomp caused universal length()==0 and infinite length-keyed loops

Status: FIXED (verified 2026-07-06).

Found while investigating an infinite loop reported when loading the real
WildFly 32.0.1.Final distribution's `org.wildfly.extension.core-management`
module (its `module.xml` depends on `java.desktop`, "for java.beans") and
triggering its class graph's static initialization in real-JDK mode. A 45s
timeout produced 4.3 million repeated lines of:

```text
WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped
(caller used slot index past receiver's layout — class layout is correct; the
bug is in the caller's slot computation) obj=0x... index=2 num_slots=2
class_id=ClassId(45) class_name=java/lang/StringBuilder real_field_count=Some(2)
value=Int(61)
```

alternating with the same warning with `value=Int(1)`. `org.jboss.as.jmx`
(no `java.desktop` dependency) did not trigger this.

## Root cause

CratonVM's synthetic `StringBuilder`/`StringBuffer` layout is 2 slots —
`value: char[]` @0, `count: int` @1 (`instance_fields(2)` in
`classloading/src/class_manager.rs`) — the layout every StringBuilder is
actually allocated with unless real `AbstractStringBuilder` bytecode itself
constructs one (e.g. during Byte Buddy retransformation), which uses the
real JDK 9+ 3-slot layout instead: `value: byte[]` @0, `coder: byte` @1,
`count: int` @2.

Commit `cc820053` ("Fix Spring bean Mockito parameter annotations",
2026-07-04) refactored every append/insert/setLength call site's direct
`ctx.set_field(this, 1, Value::Int(<new count>))` into a shared
`sb_set_count` helper, intending to *also* defensively mirror the count
into slot 2 for the narrow real-3-slot scenario:

```rust
fn sb_set_count(ctx: &mut dyn NativeContext, this: ObjectRef, count: i32) {
    ctx.set_field(this, 1, Value::Int(0));   // was: Value::Int(count)
    ctx.set_field(this, 2, Value::Int(count));
}
```

For the universal 2-slot case this unconditionally **stomped the object's
only count-bearing slot to 0** on every single append/insert/setLength
call, and the slot-2 write was silently dropped by the `gen_heap`
out-of-bounds-write guard (`num_slots=2`, index 2 is out of bounds). The
net effect: `StringBuilder.length()` always read back 0 (or briefly close
to it) immediately after the very call that was supposed to grow it — a
universal regression, reproducible with nothing more exotic than:

```java
StringBuilder sb = new StringBuilder();
sb.append('a'); sb.append('b'); sb.append('c');
sb.length();     // returned 0, not 3
sb.toString();   // returned "", not "abc"
```

Java code with a growth loop keyed on `sb.length()` — exactly the shape
real `java.beans`/AWT clinit code uses, e.g.
`while (sb.length() < n) sb.append(c);` — never observed the length
increase and spun forever, hammering the `set_field` OOB guard (which has
no rate limit, unlike the `get_field` OOB-read guard's 512-occurrence cap)
on every iteration. That is the actual source of the multi-million-line
log volume.

## Fix

`sb_set_count` (`native-builtins/src/lang_string.rs`) now branches on
`ctx.object_num_fields(this)` — the object's *actual* allocated slot count
(the same idiom already used elsewhere to avoid hard-coding a layout, e.g.
the `Timestamp` nanos-field fix) — instead of assuming one shape
universally:

```rust
fn sb_set_count(ctx: &mut dyn NativeContext, this: ObjectRef, count: i32) {
    if ctx.object_num_fields(this) >= 3 {
        ctx.set_field(this, 1, Value::Int(0));       // real layout: coder
        ctx.set_field(this, 2, Value::Int(count));   // real layout: count
    } else {
        ctx.set_field(this, 1, Value::Int(count));   // synthetic layout: count
    }
}
```

## Verification

- Manual before/after repro (debug build, no special flags):
  `new StringBuilder().append('a').append('b').append('c')` — pre-fix
  `length()==0`/`toString()==""`; post-fix `length()==3`/`toString()=="abc"`.
  A 40-character append (forcing growth past the default 16-char capacity)
  and a `while (sb.length() < 20) sb.append('x')` loop (the actual hung
  shape) both now complete correctly and instantly.
- New regression test `vm/tests/sb_count_slot_guard_regression.rs` /
  `apps/sb_count_probe/SbCountProbe.java` — spawns the real `cratonvm`
  binary as a subprocess under a 15s hard timeout (so a regression is a
  fast test *failure*, not a hung test *run*) and asserts all three shapes
  above.

## Not fully resolved by this fix

The *original* repro (loading `org.wildfly.extension.core-management`'s
real `java.desktop`-dependent class graph via
`Module.loadService(Extension.class)`) still hangs after this fix — the
`set_field` OOB-write spam for `java/lang/StringBuilder` is gone (confirmed
via a 30-45s timeout run), but the process still spins at ~100% CPU with no
further externally-visible progress. This is tracked as a separate, as yet
unidentified issue — see
`docs/known-issues/java-desktop-clinit-infinite-loop-under-cratonvm.md`.
This StringBuilder bug was real, severe (any StringBuilder use was broken
this way, everywhere, not just under `java.desktop`), and worth fixing on
its own regardless of whether it's the sole contributor to that hang.
