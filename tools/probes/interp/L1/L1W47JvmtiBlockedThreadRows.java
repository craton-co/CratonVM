// Interpreter round i1 wave 47, lane L1: the C JVMTI table's answers about
// a thread BLOCKED in a native region (`Object.wait`), against HotSpot:
//
// * `SetLocalObject` of such a thread is served (item 1 of
//   docs/internal/fixed-bugs/interpreter-L1-a-thread-blocked-in-a-native-lacks-two-c-jvmti-answers-FIXED-20261011.md;
//   wave 46 answered `JVMTI_ERROR_NOT_AVAILABLE`, 98), and a read before the
//   thread wakes answers the new value;
// * `GetStackTrace` of such a thread lists the JDK levels of the `wait`
//   stand-in (docs/internal/fixed-bugs/interpreter-L1-a-sleep-or-wait-stand-in-lists-only-its-native-leaf-FIXED-20261011.md),
//   a JNI native in the middle of its stack, and a reflective call's JDK
//   frames (item 2 of the page above; stage 1 of
//   docs/internal/fixed-bugs/interpreter-i46-L1-proposal-one-frame-listing-for-jdwp-and-the-c-jvmti-table-DONE-20261224.md).
//
// Three daemon threads block in `Object.wait()`: `waiter` in its own `run`
// (a local `o` = "obj"), `upcaller` under the shim's JNI native `through`,
// which calls `Up.run()` back through JNI, and `reflector` under
// `Method.invoke` of `nap()`. The native `rows` suspends each, prints its
// stack from the top down to its thread's `run` (`Class.method@location`;
// frames of `java.lang.invoke` classes, of hidden classes and the
// accessors' `invokeImpl` are left out: HotSpot runs them, this VM does not,
// as `L1W46JvmtiReflectiveFrames` leaves them out), reads and writes the
// waiter's `o`, and resumes each. `main` then notifies all three; the waiter
// prints what its `o` held when it woke.
//
// Needs the native shim tools/probes/interp/L1/L1W47JvmtiBlockedThreadRows.c,
// loaded both as an agent (its `Agent_OnLoad` adds `can_suspend` and
// `can_access_local_variables`) and with `System.load` (for `rows` and
// `through`):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w47blocked.so tools/probes/interp/L1/L1W47JvmtiBlockedThreadRows.c
//   javac -g -d /tmp/l1w47blocked tools/probes/interp/L1/L1W47JvmtiBlockedThreadRows.java
//   java     -agentpath:/tmp/libl1w47blocked.so -cp /tmp/l1w47blocked L1W47JvmtiBlockedThreadRows /tmp/libl1w47blocked.so
//   cratonvm -agentpath:/tmp/libl1w47blocked.so -cp /tmp/l1w47blocked L1W47JvmtiBlockedThreadRows /tmp/libl1w47blocked.so
//
// (`-g` matters: the slot of `o` is read from the LocalVariableTable.)
// Without the argument it prints only the usage line.
//
// Expected stdout (HotSpot 25.0.3), PREDICTED from JDK 25.0.3's class files
// and the measured rows of L1W46JvmtiOtherThreadLocalsAndMonitors and
// L1W46JvmtiReflectiveFrames (the lane's box has no C compiler): the
// orchestrator must run the HotSpot line above once and replace this block
// with what it prints. MEASURED by the wave-47 orchestrator on the Linux host
// (HotSpot 25.0.4, gcc, three runs alike): identical to the prediction below
// (stdout; HotSpot's four `System.load` restricted-method WARNING lines go to
// stderr).
//
//   Agent_OnLoad AddCapabilities: err=0
//   SuspendThread waiter: err=0
//   GetStackTrace waiter: Object.wait0@-1 Object.wait@55 Object.wait@2 L1W47JvmtiBlockedThreadRows$Waiter.run@12
//   GetLocalObject waiter o: err=0 obj
//   SetLocalObject waiter o=written: err=0
//   GetLocalObject waiter o after the write: err=0 written
//   ResumeThread waiter: err=0
//   SuspendThread upcaller: err=0
//   GetStackTrace upcaller: Object.wait0@-1 Object.wait@55 Object.wait@2 L1W47JvmtiBlockedThreadRows$Up.run@9 L1W47JvmtiBlockedThreadRows.through@-1 L1W47JvmtiBlockedThreadRows$Upcaller.run@7
//   ResumeThread upcaller: err=0
//   SuspendThread reflector: err=0
//   GetStackTrace reflector: Object.wait0@-1 Object.wait@55 Object.wait@2 L1W47JvmtiBlockedThreadRows.nap@9 DirectMethodHandleAccessor.invoke@23 Method.invoke@102 L1W47JvmtiBlockedThreadRows$Reflector.run@16
//   ResumeThread reflector: err=0
//   waiter saw o=written
//
// (Locations read with `javap -c` from JDK 25.0.3 and this file compiled
// by its `javac`: `Object.wait@55` is `wait(long)`'s platform `wait0` call,
// `@2` `wait()`'s `wait(0L)`; the accessor rows are L1W46JvmtiReflectiveFrames'.)
//
// CratonVM before wave 47 (from the code): `SetLocalObject waiter` answered
// `err=98` (and the reads after it `obj`, the waiter `saw o=obj`); under a
// registered `Object.wait` stand-in (every thread of this probe, whose C
// agent does not make `wait` run its bytecode) each stack listed `wait0`
// directly above its caller; the upcaller's listed no `through` and the
// reflector's no `DirectMethodHandleAccessor.invoke` / `Method.invoke`.
// Positive controls: `CRATONVM_FRAME_TRACE=1` prints `[DEFERRED_SETVALUES]
// recorded tid=<n> slots=1 jvmti=true`, then `[DEFERRED_SETVALUES] applied
// tid=<n> writes=1 dropped=0` when the waiter wakes; `[STANDIN_HEAD]
// java/lang/Object.wait()V levels=2` per blocked listing (`wait(J)V
// levels=1` where `wait()` runs its own bytecode); `[DEBUG_LISTING]
// tid=<n> splices=1 rows=1` (the upcaller) and `rows=2` (the reflector).
// The splices need `CRATONVM_DEBUG_ONE_FRAME_LISTING` (default on).
public class L1W47JvmtiBlockedThreadRows {
    static final Object W = new Object();
    static final Object W2 = new Object();
    static final Object W3 = new Object();
    static final String OBJ = "obj";
    static volatile String waiterSaw = "waiter saw nothing";

    static native String rows(Thread waiter, Thread upcaller, Thread reflector, String obj, String written);

    /** Calls `r.run()` through JNI: a native in the middle of the stack. */
    static native void through(Runnable r);

    static String name(Object o) {
        return o == null ? "null" : o.toString();
    }

    static final class Waiter extends Thread {
        Waiter() {
            super("waiter");
            setDaemon(true);
        }

        @Override
        public void run() {
            Object o = OBJ;
            try {
                synchronized (W) {
                    W.wait();
                }
            } catch (InterruptedException stop) {
                // Ends the thread.
            }
            waiterSaw = "waiter saw o=" + name(o);
        }
    }

    static final class Up implements Runnable {
        @Override
        public void run() {
            try {
                synchronized (W2) {
                    W2.wait();
                }
            } catch (InterruptedException stop) {
                // Ends the thread.
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

    static void nap() throws InterruptedException {
        synchronized (W3) {
            W3.wait();
        }
    }

    static final class Reflector extends Thread {
        Reflector() {
            super("reflector");
            setDaemon(true);
        }

        @Override
        public void run() {
            try {
                L1W47JvmtiBlockedThreadRows.class.getDeclaredMethod("nap").invoke(null);
            } catch (ReflectiveOperationException stop) {
                // Ends the thread.
            }
        }
    }

    static void awaitWaiting(Thread t) throws InterruptedException {
        long until = System.currentTimeMillis() + 60_000;
        while (t.getState() != Thread.State.WAITING && System.currentTimeMillis() < until) {
            Thread.sleep(5);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W47JvmtiBlockedThreadRows <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Thread waiter = new Waiter();
        Thread upcaller = new Upcaller();
        Thread reflector = new Reflector();
        waiter.start();
        upcaller.start();
        reflector.start();
        awaitWaiting(waiter);
        awaitWaiting(upcaller);
        awaitWaiting(reflector);
        Thread.sleep(200);
        System.out.print(rows(waiter, upcaller, reflector, OBJ, "written"));
        for (Object w : new Object[] {W, W2, W3}) {
            synchronized (w) {
                w.notifyAll();
            }
        }
        waiter.join(10_000);
        upcaller.join(10_000);
        reflector.join(10_000);
        System.out.println(waiterSaw);
    }
}
