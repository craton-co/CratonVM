// Interpreter round i6 wave 134, lane L1: the C JVMTI table's frame control
// (PopFrame, ForceEarlyReturn*) of a thread that is blocked entering a monitor,
// and from the MethodEntry callback of a contended synchronized entry, against
// HotSpot:
//   docs/known-issues/interpreter/i133-L1-c-jvmti-frame-control-of-a-thread-blocked-on-monitor-entry-20261231.md
//   docs/known-issues/interpreter/i133-L8-c-jvmti-method-entry-frame-control-residuals-20261231.md
//
// `blocked *` rows: the main thread holds a monitor, a worker blocks entering it
// (a `synchronized` block in blockTarget(), or the static synchronized
// syncTarget(), whose class monitor the main thread holds), the main thread
// suspends the worker, runs the actions on it, resumes it, and only then
// releases the monitor. The row prints each function's jvmtiError, how many
// times the target's body began (`calls`) and got inside the block (`inside`),
// what the worker's call returned, and whether the monitor is free afterwards.
// `waiting` is the control: a worker in Object.wait() (a native top frame,
// OPAQUE_FRAME on HotSpot, measured in L1W61JvmtiFrameControl).
//
// `entry *` rows: the agent acts in the worker's MethodEntry callback for
// syncTarget (`hits` callbacks matched). `entry contended *`: the main thread
// holds the class monitor until the worker blocks, so the event is posted once
// the monitor is held. `entry jni *`: syncTarget called through
// CallStaticIntMethod from a native (its caller is native: i133-L8 shape 2).
//
// Needs the native shim tools/probes/interp/L1/L1W134JvmtiBlockedEntryFrameControl.c,
// loaded as an agent and with System.load. Windows (MSVC, from a vcvars64 shell):
//
//   cl /nologo /O2 /LD /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" tools\probes\interp\L1\L1W134JvmtiBlockedEntryFrameControl.c /Fe:%TEMP%\L1W134JvmtiBlockedEntryFrameControl.dll
//   javac -d %TEMP%\l1w134 tools\probes\interp\L1\L1W134JvmtiBlockedEntryFrameControl.java
//   java     -agentpath:%TEMP%\L1W134JvmtiBlockedEntryFrameControl.dll -cp %TEMP%\l1w134 L1W134JvmtiBlockedEntryFrameControl %TEMP%\L1W134JvmtiBlockedEntryFrameControl.dll
//   java -Xint -agentpath:%TEMP%\L1W134JvmtiBlockedEntryFrameControl.dll -cp %TEMP%\l1w134 L1W134JvmtiBlockedEntryFrameControl %TEMP%\L1W134JvmtiBlockedEntryFrameControl.dll
//   cratonvm [--compatible] [--nojit] -agentpath:%TEMP%\L1W134JvmtiBlockedEntryFrameControl.dll -cp %TEMP%\l1w134 L1W134JvmtiBlockedEntryFrameControl %TEMP%\L1W134JvmtiBlockedEntryFrameControl.dll
//
// The cratonvm needs `experimental-debug` (the C frame-control slots). HotSpot's
// stderr carries the System.load native-access warning; compare stdout.
//
// The block below is NOT MEASURED: wave 134 had no built shim. It is written
// from the JVMTI specification and from reading HotSpot 25's
// UpdateForPopTopFrameClosure::doit, SetForceEarlyReturn::doit and the template
// interpreter (a pending pop or early return is taken in the call epilogue of
// InterpreterRuntime::monitorenter, check_and_handle_popframe /
// check_and_handle_earlyret, once the monitor is held, and the pop or the
// return releases it; for a synchronized method that is lock_method, before
// notify_method_entry). The orchestrator records the measured block over it.
// Rows most likely to differ from the reading: `blocked pop then int`, and the
// two `entry jni` rows (by reading: a forced return is served, the pop is
// OPAQUE_FRAME because the caller is native).
//
// CratonVM before wave 134, and with CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL=0
// (negative control): every `blocked` action answers 32 and the calls run on
// (`calls=1 inside=1 returned 107`, `calls=1 returned 109`); with
// CRATONVM_JVMTI_CONTENDED_ENTRY_FRAME_CONTROL=0: `entry contended int` is
// `int:46=32 hits=1 calls=1 returned 109`, `entry contended pop` is
// `pop=32 hits=1 calls=1 returned 109`. Shape 2 is not served on CratonVM yet:
// `entry jni int: int:47=32 hits=1 calls=1 returned 9`, `entry jni pop:
// pop=32 hits=1 calls=1 returned 9`. Positive control: CRATONVM_FRAME_TRACE=1
// prints `[POP_FRAMES] jvmti deferred` / `[JVMTI_EARLY_RETURN] deferred` per
// recorded request and `[BLOCKED_FRAME_CONTROL] adopted tid=<n>` at the wake.
//
// HOTSPOT_EXPECTED_BEGIN
// blocked int: suspend=0 int:42=0 int:43=113 resume=0 calls=1 inside=0 returned 42 free=true
// blocked pop: suspend=0 pop=0 pop=113 resume=0 calls=2 inside=1 returned 107 free=true
// blocked long: suspend=0 long=34 resume=0 calls=1 inside=1 returned 107 free=true
// blocked pop then int: suspend=0 pop=0 int:9=0 resume=0 calls=1 inside=0 returned 9 free=true
// blocked sync int: suspend=0 int:45=0 resume=0 calls=0 inside=0 returned 45 free=true
// blocked sync pop: suspend=0 pop=0 resume=0 calls=1 inside=0 returned 109 free=true
// waiting: suspend=0 pop=32 int:1=32 resume=0 free=true
// entry contended int: int:46=0 hits=1 calls=0 returned 46
// entry contended pop: pop=0 hits=2 calls=1 returned 109
// entry uncontended int: int:46=0 hits=1 calls=0 returned 46
// entry jni int: int:47=0 hits=1 calls=0 returned 47
// entry jni pop: pop=32 hits=1 calls=1 returned 9
// done
// HOTSPOT_EXPECTED_END
public class L1W134JvmtiBlockedEntryFrameControl {
    static native String suspended(Thread thread, String actions);

    static native int armEntry(Thread thread, String method, String actions);

    static native String disarmEntry(Thread thread);

    static native int callStaticInt(String name);

    static final Object LOCK = new Object();
    static final Object WAIT = new Object();

    static volatile int calls;
    static volatile int inside;
    static volatile int result;
    static volatile boolean ready;
    static volatile boolean go;
    static volatile boolean notified;

    static int blockTarget() {
        calls++;
        synchronized (LOCK) {
            inside++;
            return 7;
        }
    }

    static int blockCaller() {
        return blockTarget() + 100;
    }

    static synchronized int syncTarget() {
        calls++;
        return 9;
    }

    static int syncCaller() {
        return syncTarget() + 100;
    }

    static void waitIn() {
        synchronized (WAIT) {
            while (!notified) {
                try {
                    WAIT.wait();
                } catch (InterruptedException stop) {
                    return;
                }
            }
        }
    }

    static void awaitState(Thread t, Thread.State s) throws InterruptedException {
        long until = System.currentTimeMillis() + 60_000;
        while (t.getState() != s && System.currentTimeMillis() < until) {
            Thread.sleep(5);
        }
        // Past the spin, into the blocking park.
        Thread.sleep(200);
    }

    /** Can a fresh thread take `lock` within 10 s? */
    static boolean free(Object lock) throws InterruptedException {
        Thread t = new Thread(() -> {
            synchronized (lock) {
                lock.hashCode();
            }
        });
        t.setDaemon(true);
        t.start();
        t.join(10_000);
        return !t.isAlive();
    }

    static void reset() {
        calls = 0;
        inside = 0;
        result = -1;
        ready = false;
        go = false;
    }

    static void blocked(String label, Object lock, boolean sync, String actions) throws InterruptedException {
        reset();
        Thread worker = new Thread(() -> result = sync ? syncCaller() : blockCaller(), "worker");
        String rec;
        synchronized (lock) {
            worker.start();
            awaitState(worker, Thread.State.BLOCKED);
            rec = suspended(worker, actions);
        }
        worker.join(10_000);
        System.out.println(label + ": " + rec + " calls=" + calls + " inside=" + inside
                + " returned " + result + " free=" + free(lock));
    }

    static void waiting() throws InterruptedException {
        notified = false;
        Thread worker = new Thread(L1W134JvmtiBlockedEntryFrameControl::waitIn, "waiter");
        worker.start();
        awaitState(worker, Thread.State.WAITING);
        String rec = suspended(worker, "pop int:1");
        synchronized (WAIT) {
            notified = true;
            WAIT.notifyAll();
        }
        worker.join(10_000);
        System.out.println("waiting: " + rec + " free=" + free(WAIT));
    }

    static void entry(String label, boolean contended, String actions) throws InterruptedException {
        reset();
        Thread worker = new Thread(() -> {
            ready = true;
            while (!go) {
                Thread.onSpinWait();
            }
            result = syncCaller();
        }, "entry-worker");
        int arm;
        if (contended) {
            synchronized (L1W134JvmtiBlockedEntryFrameControl.class) {
                worker.start();
                while (!ready) {
                    Thread.onSpinWait();
                }
                arm = armEntry(worker, "syncTarget", actions);
                go = true;
                awaitState(worker, Thread.State.BLOCKED);
            }
        } else {
            worker.start();
            while (!ready) {
                Thread.onSpinWait();
            }
            arm = armEntry(worker, "syncTarget", actions);
            go = true;
        }
        worker.join(10_000);
        String rec = disarmEntry(worker);
        System.out.println(label + ": " + (arm == 0 ? "" : "arm=" + arm + " ") + rec + " calls=" + calls
                + " returned " + result);
    }

    static void entryJni(String label, String actions) {
        reset();
        Thread self = Thread.currentThread();
        int arm = armEntry(self, "syncTarget", actions);
        int r = callStaticInt("syncTarget");
        String rec = disarmEntry(self);
        System.out.println(label + ": " + (arm == 0 ? "" : "arm=" + arm + " ") + rec + " calls=" + calls
                + " returned " + r);
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W134JvmtiBlockedEntryFrameControl <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Class<?> self = L1W134JvmtiBlockedEntryFrameControl.class;
        blocked("blocked int", LOCK, false, "int:42 int:43");
        blocked("blocked pop", LOCK, false, "pop pop");
        blocked("blocked long", LOCK, false, "long");
        blocked("blocked pop then int", LOCK, false, "pop int:9");
        blocked("blocked sync int", self, true, "int:45");
        blocked("blocked sync pop", self, true, "pop");
        waiting();
        entry("entry contended int", true, "int:46");
        entry("entry contended pop", true, "pop");
        entry("entry uncontended int", false, "int:46");
        entryJni("entry jni int", "int:47");
        entryJni("entry jni pop", "pop");
        System.out.println("done");
    }
}
