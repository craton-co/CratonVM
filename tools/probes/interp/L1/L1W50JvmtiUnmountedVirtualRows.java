// Interpreter round i1 wave 50, lane L1: the C JVMTI table's `GetStackTrace`
// / `GetFrameCount` of an UNMOUNTED virtual thread (a continuation that
// yielded in `Thread.sleep` or `LockSupport.park`), against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lists-no-frames-for-an-unmounted-virtual-thread-FIXED-20261014.md).
//
// Two virtual threads block: `sleeper` in `Thread.sleep(60 s)` under
// `sleepIn()`, `parker` in `LockSupport.park()` under `parkIn()`. Neither is
// suspended (`GetStackTrace` needs no suspension). The native `rows` prints,
// for each, `GetFrameCount`'s error, and `GetStackTrace`'s frames from the
// top down to the probe's own `sleepIn` / `parkIn` frame, as
// `Class.method` (no locations: they move between JDK updates; the JDI twin
// `L1W49JdiVirtualContinuationFrames` pins them).
//
// Needs the native shim tools/probes/interp/L1/L1W50JvmtiUnmountedVirtualRows.c,
// loaded as an agent (it adds no capability) and with `System.load`:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w50vt.so tools/probes/interp/L1/L1W50JvmtiUnmountedVirtualRows.c
//   javac -d /tmp/l1w50vt tools/probes/interp/L1/L1W50JvmtiUnmountedVirtualRows.java
//   java     -agentpath:/tmp/libl1w50vt.so -cp /tmp/l1w50vt L1W50JvmtiUnmountedVirtualRows /tmp/libl1w50vt.so
//   cratonvm -agentpath:/tmp/libl1w50vt.so -cp /tmp/l1w50vt L1W50JvmtiUnmountedVirtualRows /tmp/libl1w50vt.so
//
// Expected stdout (HotSpot 25), PREDICTED (the lane's box has no C compiler)
// from the rows `L1W49JdiVirtualContinuationFrames` measured through JDWP;
// MEASURED by the wave-50 orchestrator on the Linux host (HotSpot 25.0.4,
// gcc, two runs alike): identical to the prediction below; the wave-50
// debug build prints the same in the default mode, `--nojit` and
// `--compatible`:
//
//   GetFrameCount sleeper: err=0
//   GetStackTrace sleeper: VirtualThread.yieldContinuation VirtualThread.parkNanos VirtualThread.sleepNanos Thread.sleepNanos Thread.sleep L1W50JvmtiUnmountedVirtualRows.sleepIn
//   GetFrameCount parker: err=0
//   GetStackTrace parker: VirtualThread.yieldContinuation VirtualThread.park System$1.parkVirtualThread LockSupport.park L1W50JvmtiUnmountedVirtualRows.parkIn
//   done
//
// CratonVM before wave 50: the table found no inspection window (the
// thread is in no blocking region), started a handshake no carrier answers,
// and after the full wait answered `err=98` (JVMTI_ERROR_NOT_AVAILABLE) for
// both calls of both threads. Positive control: `CRATONVM_FRAME_TRACE=1`
// prints `[UNMOUNTED_VT_ROWS] tid=<n> rows=<k>` and `[JVMTI_OTHER_STACK]
// tid=<n> rows=<k> via=unmounted`.
import java.util.concurrent.locks.LockSupport;

public class L1W50JvmtiUnmountedVirtualRows {
    static native String rows(Thread sleeper, Thread parker);

    static void sleepIn() {
        try {
            Thread.sleep(60_000);
        } catch (InterruptedException stop) {
            // Ends the thread.
        }
    }

    static volatile boolean release;

    static void parkIn() {
        while (!release) {
            LockSupport.park();
        }
    }

    static void awaitState(Thread t, Thread.State s) throws InterruptedException {
        long until = System.currentTimeMillis() + 60_000;
        while (t.getState() != s && System.currentTimeMillis() < until) {
            Thread.sleep(5);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W50JvmtiUnmountedVirtualRows <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Thread sleeper = Thread.ofVirtual().name("sleeper").start(L1W50JvmtiUnmountedVirtualRows::sleepIn);
        Thread parker = Thread.ofVirtual().name("parker").start(L1W50JvmtiUnmountedVirtualRows::parkIn);
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
