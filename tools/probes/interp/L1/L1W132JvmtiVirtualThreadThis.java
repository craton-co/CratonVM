// Interpreter round i6 wave 132, lane L1: the C JVMTI table's GetLocalInstance
// of every frame of two SUSPENDED, UNMOUNTED virtual threads, down to the
// probe's own method (docs/internal/fixed-bugs/interpreter-i119-L1-compatible-standin-java-levels-answer-no-this-CLOSED-20261224.md,
// the wave-130 residual): `vsleeper` in Thread.sleep(60 s) under sleepIn(),
// `vparker` in LockSupport.park() under parkIn(). Each row prints the frame's
// method, whether it is native, and the answer: `self` (the virtual thread's
// own Thread object), `null`, `other`, or `error N` (35 INVALID_SLOT for a
// static method, 32 OPAQUE_FRAME).
//
// HotSpot answers `self` for the java.lang.VirtualThread instance rows
// (yieldContinuation, parkNanos / park, sleepNanos). On CratonVM's default
// shape (CRATONVM_VT_CONTINUATIONS on) those are real frames. On the bound
// shape (CRATONVM_VT_CONTINUATIONS=0) they are synthesised continuation rows
// (`continuation_head_of_unmounted`): before wave 132 the C table answered
// `error 32` there even with CRATONVM_DEBUG_STANDIN_LEVEL_LOCALS=1, which JDWP's
// ThisObject already answered (wave 129). Wave 132: with the switch `=1`, the
// C table answers `self` too. Known residual on the bound shape: when the park
// was served by a `LockSupport.park*` stand-in, `System$1.parkVirtualThread` is a
// synthesised row too and answers `error 32` where HotSpot answers `other` (its
// receiver, the JavaLangAccess instance, has no tag); when the bytecode ran it
// is a real frame and answers `other`.
//
// Needs the native shim tools/probes/interp/L1/L1W132JvmtiVirtualThreadThis.c,
// loaded as an agent and with System.load. Windows (MSVC, from a vcvars64 shell):
//
//   cl /nologo /O2 /LD /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" tools\probes\interp\L1\L1W132JvmtiVirtualThreadThis.c /Fe:%TEMP%\L1W132JvmtiVirtualThreadThis.dll
//   javac -d %TEMP%\l1w132vt tools\probes\interp\L1\L1W132JvmtiVirtualThreadThis.java
//   java     -agentpath:%TEMP%\L1W132JvmtiVirtualThreadThis.dll -cp %TEMP%\l1w132vt L1W132JvmtiVirtualThreadThis %TEMP%\L1W132JvmtiVirtualThreadThis.dll
//   cratonvm [--compatible] -agentpath:%TEMP%\L1W132JvmtiVirtualThreadThis.dll -cp %TEMP%\l1w132vt L1W132JvmtiVirtualThreadThis %TEMP%\L1W132JvmtiVirtualThreadThis.dll
//
// HotSpot's stderr carries the System.load native-access warning; compare stdout.
//
// HOTSPOT_EXPECTED_BEGIN
// == vsleeper SuspendThread: err=0
//   frame 0: VirtualThread.yieldContinuation()Z native=false this=self
//   frame 1: VirtualThread.parkNanos(J)V native=false this=self
//   frame 2: VirtualThread.sleepNanos(J)V native=false this=self
//   frame 3: Thread.sleepNanos(J)V native=false this=error 35
//   frame 4: Thread.sleep(J)V native=false this=error 35
//   frame 5: L1W132JvmtiVirtualThreadThis.sleepIn()V native=false this=error 35
//   ResumeThread: err=0
// == vparker SuspendThread: err=0
//   frame 0: VirtualThread.yieldContinuation()Z native=false this=self
//   frame 1: VirtualThread.park()V native=false this=self
//   frame 2: System$1.parkVirtualThread()V native=false this=other
//   frame 3: LockSupport.park()V native=false this=error 35
//   frame 4: L1W132JvmtiVirtualThreadThis.parkIn()V native=false this=error 35
//   ResumeThread: err=0
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.locks.LockSupport;

public class L1W132JvmtiVirtualThreadThis {
    static native String rows(Thread sleeper, Thread parker);

    static volatile boolean release;
    static volatile int ended;

    static void sleepIn() {
        try {
            Thread.sleep(60_000);
        } catch (InterruptedException stop) {
            // Ends the wait.
        }
        ended++;
    }

    static void parkIn() {
        while (!release) {
            LockSupport.park();
        }
        ended++;
    }

    static void awaitState(Thread t, Thread.State s) throws InterruptedException {
        long until = System.currentTimeMillis() + 60_000;
        while (t.getState() != s && System.currentTimeMillis() < until) {
            Thread.sleep(5);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W132JvmtiVirtualThreadThis <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Thread sleeper = Thread.ofVirtual().name("vsleeper").start(L1W132JvmtiVirtualThreadThis::sleepIn);
        Thread parker = Thread.ofVirtual().name("vparker").start(L1W132JvmtiVirtualThreadThis::parkIn);
        awaitState(sleeper, Thread.State.TIMED_WAITING);
        awaitState(parker, Thread.State.WAITING);
        Thread.sleep(200);
        System.out.print(rows(sleeper, parker));
        release = true;
        LockSupport.unpark(parker);
        sleeper.interrupt();
        sleeper.join(10_000);
        parker.join(10_000);
        System.out.println("done");
    }
}
