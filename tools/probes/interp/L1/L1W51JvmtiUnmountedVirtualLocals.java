// Interpreter round i1 wave 51, lane L1: the C JVMTI table's local-variable
// functions and `GetOwnedMonitorStackDepthInfo` asked about a SUSPENDED, UNMOUNTED
// virtual thread (a continuation that yielded in `Thread.sleep` or
// `LockSupport.park`), against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-reads-no-locals-of-an-unmounted-virtual-thread-FIXED-20261015.md).
//
// Two virtual threads block: `sleeper` in `Thread.sleep(60 s)` under
// `sleepIn(7, "seven")`, `parker` in `LockSupport.park()` under
// `parkIn(11, "eleven")`; both keep their two parameters live past the
// blocking call. The native `rows` suspends each, finds the frame of the
// probe's own method in `GetStackTrace`, and prints, at that depth, one line
// per call (`err=<jvmtiError number>`, then the value): `GetLocalInt` of
// slot 0, `GetLocalObject` of slot 1, `GetLocalInt` of slot 9 (past
// `max_locals`), `GetLocalInstance` (a static method), `SetLocalInt` of
// slot 0 (HotSpot writes no frame of a heap stack chunk: OPAQUE_FRAME), then
// `GetOwnedMonitorStackDepthInfo`'s count, and resumes the thread. Last, the
// calling thread (inside `synchronized (LOCK)`) asks `GetOwnedMonitorInfo`
// of itself (slot 10, `JVMTI_ERROR_NOT_AVAILABLE` on CratonVM before wave
// 51): one monitor, `LOCK`.
//
// Needs the native shim tools/probes/interp/L1/L1W51JvmtiUnmountedVirtualLocals.c,
// loaded as an agent (it adds `can_suspend`, `can_access_local_variables`,
// `can_get_owned_monitor_stack_depth_info` and `can_get_owned_monitor_info`)
// and with `System.load`:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w51vl.so tools/probes/interp/L1/L1W51JvmtiUnmountedVirtualLocals.c
//   javac -d /tmp/l1w51vl tools/probes/interp/L1/L1W51JvmtiUnmountedVirtualLocals.java
//   java     -agentpath:/tmp/libl1w51vl.so -cp /tmp/l1w51vl L1W51JvmtiUnmountedVirtualLocals /tmp/libl1w51vl.so
//   cratonvm -agentpath:/tmp/libl1w51vl.so -cp /tmp/l1w51vl L1W51JvmtiUnmountedVirtualLocals /tmp/libl1w51vl.so
//
// Expected stdout (HotSpot 25), first PREDICTED (the lane's box has no C
// compiler) from HotSpot's `VM_VirtualThreadGetOrSetLocal` /
// `VM_BaseGetOrSetLocal::doit` (reads of a heap frame served, a write
// answered OPAQUE_FRAME), then MEASURED by the wave-52 orchestrator on the
// Linux host (HotSpot 25.0.4, gcc): identical to the block below, and the
// wave-52 debug build prints the same in the default mode, `--nojit` and
// `--compatible` (HotSpot also prints the `System.load` native-access
// warnings on stderr):
//
//   SuspendThread sleeper: err=0
//   frame sleeper: found
//   GetLocalInt sleeper n: err=0 7
//   GetLocalObject sleeper tag: err=0 seven
//   GetLocalInt sleeper slot 9: err=35
//   GetLocalInstance sleeper: err=35
//   SetLocalInt sleeper n: err=32
//   GetOwnedMonitorStackDepthInfo sleeper: err=0 count=0
//   ResumeThread sleeper: err=0
//   SuspendThread parker: err=0
//   frame parker: found
//   GetLocalInt parker n: err=0 11
//   GetLocalObject parker tag: err=0 eleven
//   GetLocalInt parker slot 9: err=35
//   GetLocalInstance parker: err=35
//   SetLocalInt parker n: err=32
//   GetOwnedMonitorStackDepthInfo parker: err=0 count=0
//   ResumeThread parker: err=0
//   GetOwnedMonitorInfo current: err=0 count=1 lock=true
//   sleeper saw 7 seven
//   parker saw 11 eleven
//   done
//
// CratonVM before wave 51: every local and monitor call waited the table's
// full stop wait and answered `err=98` (JVMTI_ERROR_NOT_AVAILABLE): the
// thread has no blocking window and no carrier to run a handshake; and
// `GetOwnedMonitorInfo current: err=98` (slot 10 was not implemented, and
// its capability not potential). Positive control: `CRATONVM_FRAME_TRACE=1` prints `[UNMOUNTED_VT_WORK]
// tid=<n> ran=true` and `[JVMTI_OTHER_LOCAL] ... via=unmounted`.
import java.util.concurrent.locks.LockSupport;

public class L1W51JvmtiUnmountedVirtualLocals {
    static native String rows(Thread sleeper, Thread parker, Object lock);

    static final Object LOCK = new Object();

    static volatile String sleeperSaw;
    static volatile String parkerSaw;

    static void sleepIn(int n, String tag) {
        try {
            Thread.sleep(60_000);
        } catch (InterruptedException stop) {
            // Ends the wait.
        }
        sleeperSaw = n + " " + tag;
    }

    static volatile boolean release;

    static void parkIn(int n, String tag) {
        while (!release) {
            LockSupport.park();
        }
        parkerSaw = n + " " + tag;
    }

    static void awaitState(Thread t, Thread.State s) throws InterruptedException {
        long until = System.currentTimeMillis() + 60_000;
        while (t.getState() != s && System.currentTimeMillis() < until) {
            Thread.sleep(5);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W51JvmtiUnmountedVirtualLocals <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Thread sleeper = Thread.ofVirtual().name("sleeper").start(() -> sleepIn(7, "seven"));
        Thread parker = Thread.ofVirtual().name("parker").start(() -> parkIn(11, "eleven"));
        awaitState(sleeper, Thread.State.TIMED_WAITING);
        awaitState(parker, Thread.State.WAITING);
        Thread.sleep(200);
        String rows;
        synchronized (LOCK) {
            rows = rows(sleeper, parker, LOCK);
        }
        System.out.print(rows);
        release = true;
        LockSupport.unpark(parker);
        sleeper.interrupt();
        sleeper.join(10_000);
        parker.join(10_000);
        System.out.println("sleeper saw " + sleeperSaw);
        System.out.println("parker saw " + parkerSaw);
        System.out.println("done");
    }
}
