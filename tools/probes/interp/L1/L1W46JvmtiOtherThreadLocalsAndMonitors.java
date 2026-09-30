// Interpreter round i1 wave 46, lane L1: the C JVMTI table's local-variable
// functions and GetOwnedMonitorStackDepthInfo asked about ANOTHER thread,
// against HotSpot (what remained of
// docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-reads-no-other-threads-stack-FIXED-20261010.md).
//
// `main` starts two daemon threads and calls the native `rows` once both
// run:
//
// * `holder` enters the static synchronized `Holder.outer`, which calls
//   `hold(3, 4L, "obj")`; `hold` takes `M1` and spins on its local `mark`
//   (7) until the agent writes 8 into it.
// * `waiter` takes `M2`, then `W`, and waits on `W` with no timeout; its
//   `run` keeps a local `token` (5), which it prints once notified.
// * `upcaller` calls the shim's JNI native `through`, which calls
//   `Up.run()` back through JNI; `Up.spinUp` spins. Its stack has a native
//   method in the middle, which HotSpot lists at location -1.
//
// `rows` prints one line per call, `<row>: <answer>`, with `err=<jvmtiError
// number>` for an error. A monitor is printed by name (`M1`, `M2`, `W`, `HC`
// for `Holder.class`, `?` for another object) at its stack depth. The
// holder is read running (monitors), then suspended (locals, monitors,
// writes); the waiter is read waiting (monitors), then suspended while it
// stays in `Object.wait` (locals, a write, monitors). After `rows` the
// holder has left its loop and `main` notifies the waiter; both print what
// they saw.
//
// Needs the native shim tools/probes/interp/L1/L1W46JvmtiOtherThreadLocalsAndMonitors.c,
// loaded both as an agent (its `Agent_OnLoad` adds `can_suspend`,
// `can_access_local_variables` and `can_get_owned_monitor_stack_depth_info`)
// and with `System.load` (for `rows`):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w46other.so tools/probes/interp/L1/L1W46JvmtiOtherThreadLocalsAndMonitors.c
//   javac -g -d /tmp/l1w46other tools/probes/interp/L1/L1W46JvmtiOtherThreadLocalsAndMonitors.java
//   java     -agentpath:/tmp/libl1w46other.so -cp /tmp/l1w46other L1W46JvmtiOtherThreadLocalsAndMonitors /tmp/libl1w46other.so
//   cratonvm -agentpath:/tmp/libl1w46other.so -cp /tmp/l1w46other L1W46JvmtiOtherThreadLocalsAndMonitors /tmp/libl1w46other.so
//
// (`-g` matters: the slot checks read the LocalVariableTable.) Without the
// argument it prints only the usage line. HotSpot also prints its
// restricted-method WARNING lines for `System.load` on stderr.
//
// Expected stdout (HotSpot 25.0.3): measured on the Windows box with a Rust
// port of the C shim (the box has no C compiler; same calls, same order,
// same output format), three runs, the same each time. The orchestrator
// should run the HotSpot line above once with the C shim and correct this
// block if anything differs.
//
//   Agent_OnLoad AddCapabilities: err=0
//   GetOwnedMonitorStackDepthInfo holder (running): err=0 count=2 M1@0 HC@1
//   GetLocalInt holder a (running): err=13
//   GetOwnedMonitorStackDepthInfo waiter (waiting): err=0 count=1 M2@3
//   GetLocalInt waiter token (waiting, not suspended): err=13
//   SuspendThread holder: err=0
//   GetFrameLocation holder 0: err=0 hold
//   GetFrameLocation holder 1: err=0 outer
//   GetLocalInt holder a: err=0 3
//   GetLocalLong holder b: err=0 4
//   GetLocalObject holder o: err=0 obj
//   GetLocalInt holder mark: err=0 7
//   GetLocalInt holder b (a long): err=34
//   GetLocalInt holder slot 99: err=35
//   GetLocalInt holder depth 1 slot 0 (outer has no locals): err=35
//   GetLocalInt holder depth 9: err=31
//   GetLocalInstance holder depth 0 (static): err=35
//   GetOwnedMonitorStackDepthInfo holder (suspended): err=0 count=2 M1@0 HC@1
//   SetLocalInt holder mark=8: err=0
//   GetLocalInt holder mark after the write: err=0 8
//   SetLocalObject holder o=written: err=0
//   GetLocalObject holder o after the write: err=0 written
//   SetLocalLong holder a (an int): err=34
//   ResumeThread holder: err=0
//   SuspendThread waiter: err=0
//   GetFrameLocation waiter 0: err=0 wait0
//   GetFrameLocation waiter 3: err=0 run
//   GetLocalInt waiter token: err=0 5
//   GetLocalObject waiter this: err=0 same=1
//   GetLocalInstance waiter depth 3: err=0
//   GetLocalInstance waiter depth 3 is the waiter: 1
//   GetLocalInt waiter depth 0 (the native): err=32
//   GetLocalInt waiter slot 0 (this): err=34
//   SetLocalInt waiter token=6: err=0
//   GetLocalInt waiter token after the write: err=0 6
//   GetOwnedMonitorStackDepthInfo waiter (suspended): err=0 count=1 M2@3
//   ResumeThread waiter: err=0
//   GetLocalInt waiter token (resumed): err=13
//   SuspendThread upcaller: err=0
//   GetStackTrace upcaller (suspended) 0 4: n=4 spinUp run through run
//   GetFrameLocation upcaller 2: err=0 through@-1
//   GetFrameLocation upcaller 3: err=0 run@7
//   ResumeThread upcaller: err=0
//   GetStackTrace upcaller (running) 0 4: n=4 spinUp run through run
//   holder saw mark=8 a=3 b=4 o=written
//   waiter saw token=6
//
// CratonVM before wave 46 (from reading `jvmti::native_env`): every local
// function asked about another thread answers `THREAD_NOT_SUSPENDED` (13),
// suspended or not, and `GetOwnedMonitorStackDepthInfo` of another thread
// `NOT_AVAILABLE` (98). So both `(running)` / `(waiting)` monitor rows and
// both `(suspended)` ones print `err=98`, every holder and waiter local row
// after its `SuspendThread` (the `GetLocalInstance` and `SetLocal*` rows
// included) prints `err=13`, the holder never leaves its loop (`holder saw
// nothing`, after `join`'s 10 s) and the waiter prints `waiter saw token=5`.
// The `SuspendThread`, `ResumeThread` and holder / waiter `GetFrameLocation`
// rows already matched (wave 45). The upcaller's rows list no `through`: a
// parked or handshaken thread was listed from the listing its park
// published for JDWP, which names no JNI native in the middle of a stack
// (`n=3 spinUp run run` twice, `GetFrameLocation upcaller 2: err=0 run@7`,
// `GetFrameLocation upcaller 3: err=31`).
//
// CratonVM since wave 46, predicted from the code: every row matches. The
// holder is read on its own thread, parked at a suspend point (after a
// handshake for the `(running)` row); the waiter through its blocking
// region's inspection window (`interpreter::with_blocked_frames`), the
// `token` write stored in place; the upcaller lists itself on its own thread
// (`native_env::listed_rows`, which splices its JNI native's row,
// `JvmThread::jni_native_frames`). Positive control: `CRATONVM_FRAME_TRACE=1`
// prints `[JVMTI_OTHER_LOCAL] tid=<n> depth=<d> op=read|write|instance
// via=parked|window err=<code>` per local function of another suspended
// thread (`via=parked` for the holder, `via=window` for the waiter) and
// `[JVMTI_OTHER_MONITORS] tid=<n> count=<k> via=parked|window
// handshake=true|false` per monitor read (`handshake=true` for the holder's
// `(running)` row); neither line exists on the base. The upcaller's two
// `GetStackTrace` rows print `[JVMTI_OTHER_STACK] tid=<n> rows=4
// via=suspended` and `via=handshake` (`rows=3` on the base). `--compatible` may
// differ in the waiter's frames, where a registered native stands in for
// `Object.wait`. Not served yet (HotSpot: `err=0`; CratonVM:
// `JVMTI_ERROR_NOT_AVAILABLE`, so not a row here): `SetLocalObject` of a
// thread blocked in a native region
// (docs/known-issues/interpreter/i46-L1-a-thread-blocked-in-a-native-lacks-two-c-jvmti-answers-20261010.md).
public class L1W46JvmtiOtherThreadLocalsAndMonitors {
    static final Object M1 = new Object();
    static final Object M2 = new Object();
    static final Object W = new Object();
    static final Object STARTED = new Object();
    static volatile int started;
    static volatile long sink;
    static volatile String holderSaw = "holder saw nothing";
    static volatile String waiterSaw = "waiter saw nothing";

    static native String rows(Thread holder, Thread waiter, Thread upcaller, Object m1, Object m2,
            Object w, Object holderClass, String obj, String written);

    /** Calls `r.run()` through JNI: a native in the middle of the stack. */
    static native void through(Runnable r);

    static final class Holder extends Thread {
        Holder() {
            super("holder");
            setDaemon(true);
        }

        @Override
        public void run() {
            outer();
        }

        static synchronized void outer() {
            hold(3, 4L, "obj");
        }

        static void hold(int a, long b, Object o) {
            int mark = 7;
            synchronized (M1) {
                synchronized (STARTED) {
                    started++;
                }
                while (mark == 7) {
                    sink++;
                }
                holderSaw = "holder saw mark=" + mark + " a=" + a + " b=" + b + " o=" + o;
            }
        }
    }

    static final class Up implements Runnable {
        @Override
        public void run() {
            spinUp();
        }

        static void spinUp() {
            synchronized (STARTED) {
                started++;
            }
            while (true) {
                sink++;
            }
        }
    }

    static final class Upcaller extends Thread {
        Upcaller() {
            super("upcaller");
            setDaemon(true);
        }

        @Override
        public void run() {
            through(new Up());
        }
    }

    static final class Waiter extends Thread {
        Waiter() {
            super("waiter");
            setDaemon(true);
        }

        @Override
        public void run() {
            int token = 5;
            try {
                synchronized (M2) {
                    synchronized (W) {
                        synchronized (STARTED) {
                            started++;
                        }
                        W.wait();
                    }
                }
            } catch (InterruptedException stop) {
                // Ends the thread.
            }
            // `this` is read after the wait, so it stays live across it
            // (a blocked thread's dead reference local reads as null here).
            waiterSaw = getName() + " saw token=" + token;
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W46JvmtiOtherThreadLocalsAndMonitors <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Thread holder = new Holder();
        Thread waiter = new Waiter();
        Thread upcaller = new Upcaller();
        holder.start();
        waiter.start();
        upcaller.start();
        while (started < 3) {
            Thread.sleep(5);
        }
        // The waiter has released W inside wait() once main can take it.
        synchronized (W) {
            // Nothing: only to know the waiter waits.
        }
        Thread.sleep(200);
        System.out.print(rows(holder, waiter, upcaller, M1, M2, W, Holder.class, "obj", "written"));
        holder.join(10_000);
        System.out.println(holderSaw);
        synchronized (W) {
            W.notifyAll();
        }
        waiter.join(10_000);
        System.out.println(waiterSaw);
    }
}
