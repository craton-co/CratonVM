# java.desktop-dependent module clinit hangs under CratonVM (root cause not yet identified)

Status: open

Found while investigating a reported infinite loop when loading the real
WildFly 32.0.1.Final distribution's `org.wildfly.extension.core-management`
module and triggering its class graph's static initialization. That
module's `module.xml` depends on `java.desktop` ("for java.beans"), unlike
`org.jboss.as.jmx` (no `java.desktop` dependency), which loads and returns
cleanly in under a second.

## Reproduction

```bash
CTRL_JAR=/data/data/wildfly-dist/wildfly-32.0.1.Final/modules/system/layers/base/org/jboss/as/controller/main/wildfly-controller-24.0.1.Final.jar
CP=<dir with a compiled probe class>:$CTRL_JAR
timeout 45 env CRATONVM_JBOSS_MP_ROOT=/data/data/wildfly-dist/wildfly-32.0.1.Final/modules \
  ./cratonvm -cp "$CP" <probe class doing> \
  'new LocalModuleLoader().loadModule("org.wildfly.extension.core-management").loadService(Class.forName("org.jboss.as.controller.Extension"))'
```

Never returns. `ps` shows the VM's `main-vm` thread pegged at ~100% CPU for
as long as observed (up to 180s in one run); `gdb -p <pid> -batch -ex
'thread apply all bt'` sampled twice a few seconds apart shows the same
repeating call shape both times:

```
jit_invoke_dispatch -> invoke_virtual -> invoke_or_native ->
safe_native_call -> update_root_snapshot -> scan_active_jit_frames ->
scan_one_frame_precise -> scan_one_frame -> VmHeap::is_object_address
```

i.e. a JIT-compiled method is repeatedly invoking some native, and each
call's (normal, expected) JIT-frame root-scan is a large enough fraction
of each iteration's wall time that repeated sampling lands there — this is
consistent with a tight **Java-level** loop, not a Rust-side spin.

## What this is *not*

A related, more severe StringBuilder bug was found and fixed alongside this
investigation — see
`docs/internal/fixed-suite-bugs/stringbuilder-count-slot-stomp-infinite-loop.md`.
`sb_set_count`'s unconditional slot-1-zero regressed `StringBuilder.length()`
to always read back 0, which would make *any* `while (sb.length() < n) ...`
loop spin forever. After fixing that:

- A 30-45s timeout run of this exact repro no longer produces the
  `gen_heap::set_field` out-of-bounds-write spam for
  `java/lang/StringBuilder` (millions of lines pre-fix).
- The process **still hangs** — confirmed with a 180s timeout, CPU pinned
  at ~100% the whole time, log output unchanged after the `get_field`
  OOB-read diagnostic cap (512 occurrences, all for
  `java/lang/StringBuilder` index 2 on a 2-slot object — this is `sb_state`'s
  normal, correct "try slot 2, fall back to slot 1" probe, not itself a bug).

So the StringBuilder count-slot bug was real and worth fixing on its own,
but it is **not** the (or not the only) cause of this specific hang.

## Suggested next steps

- `java.desktop`'s real clinit chain is enormous (font enumeration, image
  codec registration, `java.beans.Introspector`, platform toolkit probing,
  etc.) — the loop is somewhere in there, not in code this investigation
  reached. Narrowing further needs either:
  - A way to sample/dump the **Java-level** call stack (not just the Rust
    native stack) while hung — if CratonVM has or could grow a
    `jstack`-equivalent (e.g. a SIGQUIT handler walking JIT/interpreter
    frames to method names), that would identify the looping method
    directly instead of via `gdb` + guesswork.
  - Bisecting which specific class(es) in `java.desktop`'s clinit chain is
    responsible, e.g. by trying to load classes from that dependency chain
    individually via `Class.forName(..., true, mcl)` rather than the whole
    module in one shot, to localize which one hangs.
  - Checking for other hard-coded-slot-assuming natives in the same family
    as the fixed `sb_set_count` bug (search for `object_num_fields`/
    `class_num_total_fields` usage vs. bare `set_field`/`get_field` with a
    literal index, in any native touched by AWT/beans/font code) — this
    was the second occurrence of that exact bug shape (`Timestamp` nanos
    field was the first, see `docs/internal/fixed-suite-bugs/` for that
    fix), so a systematic sweep for the same anti-pattern may turn up more.

## Caution for anyone reproducing this

Any repro of this MUST run under a hard timeout (`timeout 30 ...` in shell,
or a bounded subprocess wait in a test) — a naive rerun without one will
hang indefinitely at ~100% CPU. Do not redirect output through a
non-timeout-guarded shell loop or pipe: an earlier attempt without a
timeout produced ~4.3GB of log output before being killed.
