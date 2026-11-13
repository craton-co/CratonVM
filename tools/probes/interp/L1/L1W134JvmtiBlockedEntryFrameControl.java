// Interpreter round i6 wave 134, lane L1: the C JVMTI table's frame control
// (PopFrame, ForceEarlyReturn*) of a thread that is blocked entering a monitor,
// and from the MethodEntry callback of a contended synchronized entry, against
// HotSpot:
//   docs/feature-designs/jvmti-delivery-threading.md
//   docs/feature-designs/jvmti-delivery-threading.md
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
// The block below is MEASURED (interpreter round i7 wave 135, lane L1): HotSpot
// 25.0.3 (Temurin 25.0.3+9), Windows, the shim built with MSVC 2022 `/O2`,
// default flags and `-Xint`, identical. Wave 134 had written it from reading,
// and the reading was wrong for every `blocked` row: HotSpot answers
// OPAQUE_FRAME (32) to both PopFrame and ForceEarlyReturn* of a thread blocked
// on a contended monitor entry. UpdateForPopTopFrameClosure::doit and
// SetForceEarlyReturn::doit refuse an OSThread in MONITOR_WAIT (the bug-4812902
// workaround), and ObjectMonitor::enter holds that state (OSThreadContendState)
// for the whole blocked span, suspended or not. The `entry contended int` and
// `entry uncontended int` rows return 146: the forced 46 plus syncCaller's 100
// (the wave-134 reading said 46).
//
// CratonVM before wave 134, and with CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL=0:
// every `blocked` action answers 32 and the calls run on (`calls=1 inside=1
// returned 107`, `calls=1 returned 109`), which IS HotSpot's block. With the
// switch at its wave-134 default (ON) the `blocked` actions answer 0 instead
// (the i133-L1 page: the switch must go default OFF). With
// CRATONVM_JVMTI_CONTENDED_ENTRY_FRAME_CONTROL=0: `entry contended int` is
// `int:46=32 hits=1 calls=1 returned 109`, `entry contended pop` is
// `pop=32 hits=1 calls=1 returned 109`. Shape 2 (the `entry jni` rows) is
// served since interpreter round i7 wave 135, lane L1
// (CRATONVM_JVMTI_NESTED_ENTRY_FRAME_CONTROL, default ON): `entry jni int` is
// the line below, `entry jni pop` stays 32 (its caller is the native). With
// CRATONVM_JVMTI_NESTED_ENTRY_FRAME_CONTROL=0 (negative control):
// `entry jni int: int:47=32 hits=1 calls=1 returned 9`, `entry jni pop:
// pop=32 hits=1 calls=1 returned 9`. Positive control: CRATONVM_FRAME_TRACE=1
// prints `[POP_FRAMES] jvmti deferred` / `[JVMTI_EARLY_RETURN] deferred` per
// recorded request and `[BLOCKED_FRAME_CONTROL] adopted tid=<n>` at the wake.
//
// `entry lambda *` rows (interpreter round i7 wave 136, lane L1; MEASURED on
// HotSpot 25.0.3, default flags and -Xint identical): the MethodEntry callback
// of lambdaTarget reached through an IntSupplier method reference. HotSpot
// serves both: the forced return, and the pop (the caller is the spun proxy's
// getAsInt, a Java frame), whose re-invoke posts a second MethodEntry. On
// CratonVM the lambda door runs the target in a loop of its own
// (`install_and_run_cached_frame`), entered since wave 136 at the entry-frame
// callback point (CRATONVM_JVMTI_NESTED_ENTRY_FRAME_CONTROL): `entry lambda
// int` matches. `entry lambda pop` matches since interpreter round i7 wave
// 137, lane L1 (CRATONVM_JVMTI_LAMBDA_ENTRY_POP, default ON: the lambda door
// pops the unrun target and calls it again,
// `interpreter::install_and_run_cached_frame_reinvokable`); with
// CRATONVM_JVMTI_LAMBDA_ENTRY_POP=0 (negative control) it is
// `pop=32 hits=1 calls=1 returned 11`
// (docs/feature-designs/real-jdk-mode.md).
// Positive control: CRATONVM_FRAME_TRACE=1 prints `[POP_FRAMES] jvmti
// re-invoking door entry popped`.
// Before wave 136, and with CRATONVM_JVMTI_NESTED_ENTRY_FRAME_CONTROL=0:
// `entry lambda int: int:48=32 hits=1 calls=1 returned 11`.
//
// `entry compiled *` rows (interpreter round i7 wave 138, lane L1; MEASURED on
// HotSpot 25.0.3 with the same shim, default flags and -Xint identical): the
// MethodEntry callback of compiledTarget, called from a hot loop that arms
// the agent itself at iteration 400,000 (compiled by then on a VM with a
// JIT). HotSpot serves both (it deoptimizes the caller for the pop). On
// CratonVM with the JIT the call may go through the JIT helpers'
// interpreted-callee door (`install_and_run_cached_frame_monitored`), whose
// pop is served since wave 138 (CRATONVM_JVMTI_COMPILED_CALLER_ENTRY_POP,
// default ON: the door pops the unrun callee and calls it again,
// `interpreter::install_and_run_cached_frame_from_compiled`); with
// CRATONVM_JVMTI_COMPILED_CALLER_ENTRY_POP=0 (negative control, only when
// the door served it) `entry compiled pop: pop=32 hits=1 calls=1 returned
// 200000`. Under `--nojit` the caller is interpreted and the ordinary pop
// serves the row. Positive control: CRATONVM_FRAME_TRACE=1 prints
// `[FRAME_PUSH/jit-static-bc] ... compiledTarget(I)I` for the armed call and
// `[POP_FRAMES] jvmti re-invoking door entry popped`.
//
// HOTSPOT_EXPECTED_BEGIN
// blocked int: suspend=0 int:42=32 int:43=32 resume=0 calls=1 inside=1 returned 107 free=true
// blocked pop: suspend=0 pop=32 pop=32 resume=0 calls=1 inside=1 returned 107 free=true
// blocked long: suspend=0 long=32 resume=0 calls=1 inside=1 returned 107 free=true
// blocked pop then int: suspend=0 pop=32 int:9=32 resume=0 calls=1 inside=1 returned 107 free=true
// blocked sync int: suspend=0 int:45=32 resume=0 calls=1 inside=0 returned 109 free=true
// blocked sync pop: suspend=0 pop=32 resume=0 calls=1 inside=0 returned 109 free=true
// waiting: suspend=0 pop=32 int:1=32 resume=0 free=true
// entry contended int: int:46=0 hits=1 calls=0 returned 146
// entry contended pop: pop=0 hits=2 calls=1 returned 109
// entry uncontended int: int:46=0 hits=1 calls=0 returned 146
// entry jni int: int:47=0 hits=1 calls=0 returned 47
// entry jni pop: pop=32 hits=1 calls=1 returned 9
// entry lambda int: int:48=0 hits=1 calls=0 returned 48
// entry lambda pop: pop=0 hits=2 calls=1 returned 11
// entry compiled int: int:49=0 hits=1 calls=0 returned 200049
// entry compiled pop: pop=0 hits=2 calls=1 returned 200000
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

    /** Interpreter round i7 wave 136, lane L1: the `entry lambda *` rows' target. */
    static int lambdaTarget() {
        calls++;
        return 11;
    }

    /** Interpreter round i7 wave 138, lane L1: the `entry compiled *` rows' target. */
    static int compiledTarget(int i) {
        calls++;
        return i & 1;
    }

    static volatile int armAt = -1;
    static volatile String armActions = "";
    static volatile int armResult;

    /**
     * The `entry compiled *` rows' caller: a hot loop (compiled by the time
     * the row arms, on a VM with a JIT) that arms the agent at iteration
     * `armAt` and returns after that iteration's call.
     */
    static int compiledCaller(int n) {
        int sum = 0;
        for (int i = 0; i < n; i++) {
            if (i == armAt) {
                calls = 0;
                armResult = armEntry(Thread.currentThread(), "compiledTarget", armActions);
                sum += compiledTarget(i);
                return sum;
            }
            sum += compiledTarget(i);
        }
        return sum;
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

    /**
     * Interpreter round i7 wave 136, lane L1: lambdaTarget reached through a
     * method reference (an IntSupplier proxy's getAsInt calls it). Warmed
     * first, so CratonVM's lambda door, which runs the target in a loop of its
     * own (`install_and_run_cached_frame`), serves the measured call.
     */
    static void entryLambda(String label, String actions) {
        reset();
        java.util.function.IntSupplier s = L1W134JvmtiBlockedEntryFrameControl::lambdaTarget;
        int warm = 0;
        for (int i = 0; i < 2_000; i++) {
            warm += s.getAsInt();
        }
        calls = 0;
        Thread self = Thread.currentThread();
        int arm = armEntry(self, "lambdaTarget", actions);
        int r = s.getAsInt();
        String rec = disarmEntry(self);
        System.out.println(label + ": " + (arm == 0 ? "" : "arm=" + arm + " ") + rec + " calls=" + calls
                + " returned " + r + (warm == 22_000 ? "" : " warm=" + warm));
    }

    /**
     * Interpreter round i7 wave 138, lane L1: compiledTarget called from a
     * caller that runs compiled when the agent arms (warmed first; on
     * CratonVM the JIT helpers' interpreted-callee door,
     * `install_and_run_cached_frame_monitored`, serves the call). HotSpot
     * deoptimizes the caller for the pop and re-runs its invoke.
     */
    static void entryCompiled(String label, String actions) {
        reset();
        armAt = -1;
        int warm = 0;
        for (int k = 0; k < 20; k++) {
            warm += compiledCaller(50_000);
        }
        armActions = actions;
        armAt = 400_000;
        int r = compiledCaller(500_000);
        armAt = -1;
        String rec = disarmEntry(Thread.currentThread());
        int arm = armResult;
        System.out.println(label + ": " + (arm == 0 ? "" : "arm=" + arm + " ") + rec + " calls=" + calls
                + " returned " + r + (warm == 500_000 ? "" : " warm=" + warm));
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
        entryLambda("entry lambda int", "int:48");
        entryLambda("entry lambda pop", "pop");
        entryCompiled("entry compiled int", "int:49");
        entryCompiled("entry compiled pop", "pop");
        System.out.println("done");
    }
}
