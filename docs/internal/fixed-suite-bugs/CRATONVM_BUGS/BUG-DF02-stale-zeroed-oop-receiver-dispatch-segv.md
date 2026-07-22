# Bug DF02 — Stale / zeroed OOP used as a call receiver → bogus method dispatch (linkage error) or hard SIGSEGV

**Severity:** CRITICAL (memory-safety / GC use-after-free; includes the one hard
process crash in the suite).
**Status on CratonVM:** CRASH / process-death. **HotSpot:** PASS.
**Run date:** 2026-06-17
**Binary:** dev `77620f55` (worktree `C:/craton/CratonVM-tcfull`).
**Affected classes (7):**
`org.apache.coyote.http2.TestLargeUpload` (**SIGSEGV**),
`org.apache.catalina.valves.TestSSLValve`,
`org.apache.tomcat.websocket.TestWebSocketFrameClientSSL`,
`org.apache.catalina.realm.TestJNDIRealm`,
`org.apache.catalina.core.TestApplicationContextGetRequestDispatcher`,
`org.apache.tomcat.util.net.TestSSLHostConfigProtocol`,
`org.apache.tomcat.util.buf.TestCharsetCachePerformance`.

## Symptom — one root cause, two manifestations

A heap object whose header has decayed to **all-zero** (`ClassId(0)`,
`num_slots=0`, class name reported as `java/lang/Object`) is used as the
receiver of an `invokevirtual`. Depending on whether the call site is
interpreted or JIT-compiled, this surfaces as either:

**(a) Interpreter — caught as a bogus "linkage error".** The VM resolves the
method against the corrupted/garbage receiver class and reports a method that
makes no sense for the named class:

```
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped
     ... class_name=java/lang/Object num_slots=0 class_id=ClassId(0)
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/String.setFilter(Ljava/util/logging/Filter;)V"
Error in thread "main" linkage error: no such method: java/lang/String.setFilter(Ljava/util/logging/Filter;)V    # TestSSLValve

WARN cratonvm_vm::runtime::interpreter: Stale pointer detected in invokevirtual receiver
     (ptr=0x1eb7bd78, all-zero header) — falling back to CP class java/util/Iterator
Error in thread "main" linkage error: no such method: java/lang/Object.hasNext()Z                              # TestWebSocketFrameClientSSL

Error in thread "main" linkage error: no such method: cratonvm/synthetic/AnonymousObject$4.clone()Ljava/lang/Object;   # TestJNDIRealm
```

The method names (`String.setFilter`, `Object.hasNext`, `AnonymousObject$4.clone`)
are **red herrings** — they are whatever the resolver finds after dispatching on
a zeroed receiver. The real fault is the stale receiver.

**(b) JIT — hard SIGSEGV.** When the same zeroed/near-null receiver reaches
JIT-compiled code, the field read is not guarded and the process dies:

```
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF7D48077FC
#  Faulting access: read at address 0x00000000000001CA      <-- null + 0x1CA field offset
#  thread: "main-vm"
Native frames (most recent call first) [raw]:
   0: (exe+0x9F3A77)
   1: (external/jit)        <-- fault is inside JIT-compiled code
   2: (external/jit)
   3: (external/jit)
   4: (exe+0x1E77FC)
```

(`TestLargeUpload`, HTTP/2 large upload — the only CRASH in the whole suite.)

## Root cause — CONFIRMED 2026-06-17 (register-resident missed JIT root)

Investigated against the existing GC/JIT-root machinery. **DF02 is the
"register-resident / above-band missed JIT root" bug** already tracked as
known-issues **Family A (A2/A3)** and `project_precise_jit_stack_maps`. Confirmed
by reproducing the canonical repro `wildfly-suite/repro/ReflRepro` on this build:
`CRATONVM_DBG_GC_STRESS=65536 ... ReflRepro 8000` → **rc=132 crash**, identical
class to DF02 (all-zero-header receiver → bogus dispatch / SIGSEGV).

Mechanism: whenever a **JIT frame is live**, the young collection is forced to the
**non-moving sweep** (`gc_quiescence`; the moving collector can't relocate
conservatively-discovered roots). That sweep is only correct if the root set is
complete, but CratonVM's JIT-frame root scan
(`vm/src/jit/conservative_roots.rs::scan_active_jit_frames`) is **conservative —
it walks only the stack band `[scanner_sp, entry_sp)`**. A live oop that at GC
time sits **only in a CPU register** (e.g. an allocating native's result in `rax`
before it is spilled), or **above `entry_sp`** in the calling frame, is not in
that band → not marked → swept-zeroed → its stale reference later reads an
all-zero header → bogus `invokevirtual` dispatch (interpreter) or SIGSEGV (JIT).

Decisive evidence (from the handoff, re-confirmed here): `--nojit` → clean;
`-Xmx8g` (no young GC) → clean; `CRATONVM_DBG_FULLSTACK_SCAN` removes the crash
but leaves a `bad=1` residual = the truly register-resident case no stack scan
can see; `CRATONVM_SHADOW_STACK=1` → currently **rc=127 hang** (the shadow-stack
precise-roots mechanism is itself broken on `dev`).

**This is not a clean one-shot fix like DF01.** It is the precise-JIT-stack-maps
project: the real fix is to populate precise oop maps in JIT codegen (deferred
Stage B/C — the infrastructure exists in `conservative_roots.rs` but the compiler
does not yet emit maps) or to complete + un-break the shadow stack. One
**unexplored concrete lead** (per the handoff): the MIC/PIC inline-cache fast
call path (`call r11` in `jit/src/x64.rs`) may not spill the caller's live oops
before the call the way the helper path does — a localized source of the
register-resident window worth investigating first.

Verification bar for any fix: **`bad=0`** on `ReflRepro 8000` under
`CRATONVM_DBG_GC_STRESS=65536`, plus no bintrees regression (`bt18=68332206`) and
no WildFly/Spring/Tomcat regression. See
`docs/known-issues/reflrepro-register-resident-jit-root-handoff.md` and
`docs/known-issues/README.md` (Family A).

### Same family as the previously-documented stale-OOP bugs
- BUG-U (stale locale GC root → SIGSEGV),
- BUG-W (stale OOP across `Object.monitor.wait`),
- BUG-Z (filestore concurrency GC SEGV).

All 7 affected classes are **threaded + blocking + GC-heavy** workloads (TLS
handshakes, websocket SSL, JNDI realm lookups, HTTP/2 large upload, request
dispatch). The receiver is most plausibly dropped/zeroed across a blocking
operation (socket read/TLS handshake/monitor wait) where a root is not kept live
or not remapped after a collection — consistent with the recent dev threading
work (`77620f55` timed `Object.wait`, `4033a2bf` `Thread.interrupted`,
`a748087c`).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$CP = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS="1"; $env:CRATONVM_REAL_AQS="1"; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG="1"
$exe = "C:\craton\CratonVM-tcfull\target\release\cratonvm.exe"
& $exe -Xmx2g -cp $CP org.junit.runner.JUnitCore org.apache.coyote.http2.TestLargeUpload          # SIGSEGV
& $exe -Xmx2g -cp $CP org.junit.runner.JUnitCore org.apache.catalina.valves.TestSSLValve          # bogus linkage error
```

For the SIGSEGV, pin the faulting Rust/JIT frame with a `strip="none"` /
`debug="line-tables-only"` release-with-debug build + `CRATONVM_SYMBOLIZE` on RVA
`0x1E77FC` (see `reference_crash_debug_tooling`). Toggle `CRATONVM_DISABLE_JIT=1`
to confirm the JIT path is what turns the (a)-class corruption into a (b) crash.

## Recommendation

**HANDOFF to GC/runtime memory-safety** (or fix if confirmed as a regression from
the recent threading commits — worth a `git bisect` of `4033a2bf..77620f55`).
High severity: a live receiver decaying to a zeroed header is a soundness bug,
and the JIT manifestation is an unguarded SIGSEGV. The interpreter "linkage
error" cases are the same bug failing safe.
