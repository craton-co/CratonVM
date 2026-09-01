# ✅ FIXED — `SimpleClientHttpResponseTests` hung on a native `transferTo` shadow, not on the GC

## Status

**RESOLVED 2026-08-30** on `fix/zgc-low-end-frag-and-spring-hang-20260830`.

Retires `simpleclienthttpresponse-hang-is-not-arena-fragmentation-20260829.md`,
which had correctly excluded two causes and left the third open. This is the
third.

| | before | after |
|---|---|---|
| `org.springframework.http.client.SimpleClientHttpResponseTests` | **rc=124**, killed at the 500 s cap, all four GC arms | **`found=5 succ=5 fail=0 status=OK`, 12.8 s** |
| real HotSpot, same runner | `found=5 succ=5 fail=0`, 1.5 s | unchanged |
| a plain `InputStream` subclass's `transferTo` buffer | 16 777 216 bytes | **16 384**, the JDK's own |

The previous page's two eliminations were both right and are kept: the
fragmentation gauge fires **zero** times in arms that hang identically, and the
descriptor-coercion storm is designed behaviour that a twenty-line `HashMap`
program reproduces at the same rate while finishing instantly.

## The defect

`register_p59_bulk_stream_transfer` registered a Rust native for
`transferTo(Ljava/io/OutputStream;)J` on **`java/io/InputStream`** — the root of
every input stream in the JVM. It exists for one Spring Boot fixture that copies
six 1 GiB STORED ZIP entries, and its own header calls the method it is
overriding "ordinary bytecode".

An override on the root class reaches every subclass that does not declare its
own `transferTo`, and it cost two things:

* **a 16 MiB Java array per call**, where the JDK's `transferTo` uses 16 384
  bytes — so an ordinary two-byte copy allocated 16 MiB;
* **a method CratonVM serves from Rust cannot carry an agent's woven advice.**

The second is the hang. Mockito's inline mock maker instruments
`java.io.InputStream`, and the test stubs `read(byte[],int,int)` to throw.
`SimpleClientHttpResponse.close()` drains the body through `transferTo`, so the
chain is:

1. the outer `transferTo` on the mock **is** intercepted, and its stub is
   `willCallRealMethod`;
2. Mockito sets its `SelfCallInfo` self-call grant and reflects into the real
   method — which lands on **CratonVM's native**, not on the woven body, so the
   advice never re-enters and the grant is never consumed;
3. the grant is therefore still outstanding at the next intercepted call. That
   call is `read(byte[],int,int)` — the one the test stubs to throw — and
   `checkSelfCall` swallows it as a self-call, so the **real JDK**
   `InputStream.read(byte[],int,int)` runs;
4. that body loops on `read()`, which the mock answers with an unstubbed
   default `0`. It fills the buffer, reports progress, and never terminates.
   The stubbed `NullPointerException` the test asserts on is never thrown.

## What settled it, and what could have settled it sooner

**Which body ran is observable from Java, with no debugger.** The native copies
through 16 MiB and the JDK body through 16 384, so the `len` argument the stub
sees names the executor:

```text
                     CratonVM        HotSpot
plain subclass       16777216        16384
```

**The control is in the same class.** `readNBytes`, `readAllBytes` and `skip`
all call `this.read(byte[],int,int)` from inside `java.io.InputStream` through
the identical `willCallRealMethod` path, and all three behave exactly like
HotSpot in the same run. The one thing they do not have is a registered native.
That is what isolates the registration rather than the dispatch machinery, and
it is one probe, not a bisect.

**A backtrace from inside the native named the door in one build**, after three
plausible doors had been guarded and none of them was on the path:

```text
native_input_stream_transfer_to
safe_native_call_impl
invoke_or_native
invoke_virtual
native_method_invoke      <- Method.invoke, i.e. willCallRealMethod
```

Guessing which of ~6 dispatch doors serves a call cost two builds. Printing a
`Backtrace::force_capture()` from the callee cost one.

## The fix

`java/io/InputStream` is dropped from the registration. Both fast paths the
native exists for are receiver-specific and keep their receivers:

* `has_byte_array_stream_layout` — the `ByteArrayInputStream` "just advance
  `pos`" drain, now registered on `java/io/ByteArrayInputStream`;
* `file_stream_fd` — the `FileInputStream` to STORED-zip raw copy, unchanged.

Spring Boot's `ZipInflaterInputStream` already carries its own `read([BII)`
native in the same registrar, which is where its bulk path belongs. Everything
else runs the JDK's `transferTo`.

`is_input_stream_transfer_to_native_override` is kept in step — it is consulted
by two other gates and would otherwise have drifted from the registration it
describes.

## Four dispatch gates were repaired on the way, and they are worth keeping

None of them turned out to be on this call's path, and all four are real:

1. **`invoke_on_class_shared_inner`'s `check_override` chain** had no JVMTI
   redefine guard at all, where every other native-shadow gate has one.
2. **`execute_invokevirtual_cached`'s hit-time eviction** asks whether the
   RECEIVER class was redefined. For a Mockito mock of an ABSTRACT class the
   receiver is a generated subclass whose generation is and stays 0, while the
   woven class is its superclass. `populate_virtual_invoke_cache`'s own comment
   names this and concludes "the fix has to be here, where the entry is
   created" — necessary, and not sufficient, because an entry created or
   promoted before the retransform is never revisited.
   `hierarchy_was_redefined` was the right question and already existed in
   `redefine_state` with zero callers.
3. **`invoke_or_native`'s "always check native registry first"** had no guard
   either.
4. **A native may only yield to a body that EXISTS.** The first cut of (3)
   dropped `java/lang/Object.hashCode()I` — `ACC_NATIVE` in the real JDK, no
   `Code` — the moment anything anywhere was mocked, which would have taken
   CratonVM's identity hash with it for every object in the process. The guard
   is now `method.is_abstract()`'s question widened by one word: a registered
   native yields only to a concrete, non-native body carrying `Code`.

`NATIVE_SHADOW_DROPPED_BY_REDEFINE` counts (4)'s engagement and
`CRATONVM_DBG_NATIVE_SHADOW` names each dropped triple, because a guard nobody
can see fire is indistinguishable from one that does not.

## Residual, small and separate

With the native gone the test passes and the exception propagates, but on a
synthetic probe whose stub RETURNS rather than throws, CratonVM records
`[transferTo/1, read/0]` where HotSpot records `[transferTo/1, read/3]`: the
reflective re-entry still does not consume Mockito's self-call grant, so the
inner `read(byte[],int,int)` is swallowed one step out of phase. The visible
answer is the same — `transferTo` returns 0 either way — which is why the class
passes.

It is **not** reflective virtual dispatch: a plain-Java probe
(`Method.invoke(Base.who, subInstance)`) answers `SUB` on both VMs. Whatever it
is, it is not this defect and it does not need this page.

## Repro

```bash
cd /data/<worktree>/apps/spring-suite-runner
CRATONVM_BIN=<binary> JDK25=/data/toolchain/jdk-25 \
  timeout 500 bash one.sh org.springframework.http.client.SimpleClientHttpResponseTests
```

Ten lines, no GC, no suite:

```java
InputStream is = mock(InputStream.class);
given(is.transferTo(any())).willCallRealMethod();
given(is.read(any(), anyInt(), anyInt())).willThrow(new NullPointerException("stub"));
is.transferTo(OutputStream.nullOutputStream());   // must throw; used to spin forever
```

Add `willAnswer(i -> { lastLen = i.getArgument(2); return -1; })` to the `read`
stub and the same program reports which body ran: 16 384 is the JDK's,
16 777 216 was CratonVM's native.
