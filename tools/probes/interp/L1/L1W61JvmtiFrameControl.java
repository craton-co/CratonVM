// Interpreter round i2 wave 61, lane L1: the C JVMTI table's frame control
// against HotSpot -- `PopFrame` (80), `ForceEarlyReturn*` (81-86),
// `ClearAllFramePops` (67) and `NotifyFramePop` (20) of a thread suspended
// while blocked in a native
// (docs/internal/fixed-bugs/interpreter-L1-proposal-c-jvmti-suspension-and-frame-control-FIXED-20261025.md).
//
// A worker thread spins in a target method (`while (!release) {}`); `main`
// suspends it there, asks the function, resumes it and prints what the
// worker's call returned. Rows print the `jvmtiError` number (`err=`).
//
// * `caps`: whether `can_pop_frame` / `can_force_early_return` are potential
//   in `Agent_OnLoad` and in the live phase, and what a capability-less env
//   answers;
// * `early <kind>`: each `ForceEarlyReturn*` on a method of that return kind,
//   the wrong kinds first, then the right one twice (a second request
//   before the resume), then the value the call returned;
// * `pop`: `PopFrame` twice, the frame count and the top frame's method
//   before the resume, then how many times the target ran and what it
//   returned; a `NotifyFramePop` of the popped frame posts no `FramePop`;
// * `clear`: `NotifyFramePop` of the target, `ClearAllFramePops`, no
//   `FramePop`; then a control that keeps its request;
// * `running` / `current` / `bottom`: a thread not suspended, the calling
//   thread (its top frame is the native) and a thread whose only frame is
//   `run`;
// * `sleeper`: a thread suspended in `Thread.sleep`: `PopFrame` and
//   `ForceEarlyReturnVoid` (its top frame is a native), then `NotifyFramePop`
//   of the frame that called `sleep`, which posts its `FramePop` once the
//   thread is resumed and interrupted.
//
// The first scenario runs again at the end (the cold-VM first-call rows).
//
// Needs the native shim tools/probes/interp/L1/L1W61JvmtiFrameControl.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w61frames.so tools/probes/interp/L1/L1W61JvmtiFrameControl.c
//   javac -d /tmp/l1w61frames tools/probes/interp/L1/L1W61JvmtiFrameControl.java
//   java     -agentpath:/tmp/libl1w61frames.so -cp /tmp/l1w61frames L1W61JvmtiFrameControl /tmp/libl1w61frames.so
//   cratonvm -agentpath:/tmp/libl1w61frames.so -cp /tmp/l1w61frames L1W61JvmtiFrameControl /tmp/libl1w61frames.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W61JvmtiFrameControl (README in
// tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   caps onload: pop=1 early=1 add=0 enable=0
//   caps live: pop=1 early=1
//   caps none: pop=99 early=99 clear=99
//   caps live add: pop=0 early=0
//   early int: suspend=0
//     long=34 float=34 double=34 object=34 void=34
//     int=0 again=113
//     resume: err=0
//     returned 42
//   early boolean: suspend=0 int=0
//     resume: err=0
//     returned true
//   early long: suspend=0 int=34 long=0
//     resume: err=0
//     returned 1099511627776
//   early float: suspend=0 double=34 float=0
//     resume: err=0
//     returned 2.5
//   early double: suspend=0 float=34 double=0
//     resume: err=0
//     returned -0.5
//   early object: suspend=0 integer=34 int=34 string=0
//     resume: err=0
//     returned forced
//   early null: suspend=0 null=0
//     resume: err=0
//     returned null
//   early void: suspend=0 int=34 void=0
//     resume: err=0
//     reached the end: false
//   pop: suspend=0
//     notify depth 0: err=0
//     pop: err=0 again=113
//     frames after - before: 0
//     top: popTarget next: popCaller
//     resume: err=0
//     calls=2 returned 42
//   clear: suspend=0 notify=0 clear=0 clear again=0
//     resume: err=0
//   kept: suspend=0 notify=0
//     resume: err=0
//     FramePop intTarget popped=0
//   running: pop=13 int=13 clear=13
//   current: pop=32 int=32 void=32 clear=0 pop null=10 void null=32
//   bottom: suspend=0 pop=31
//     resume: err=0
//   sleeper: suspend=0 pop=32 void=32
//     found sleepIn: true notify=0 notify again=40
//     resume: err=0
//     FramePop sleepIn popped=0
//   early int: suspend=0
//     long=34 float=34 double=34 object=34 void=34
//     int=0 again=113
//     resume: err=0
//     returned 42
//   pop: suspend=0
//     notify depth 0: err=0
//     pop: err=0 again=113
//     frames after - before: 0
//     top: popTarget next: popCaller
//     resume: err=0
//     calls=2 returned 42
//
// CratonVM before wave 61: the slots answered 98 and `can_pop_frame` /
// `can_force_early_return` were not potential, so `Agent_OnLoad`'s
// `AddCapabilities` failed whole (98) and every row differs; running with
// `CRATONVM_JVMTI_C_FRAME_CONTROL=0` reproduces it (the negative control).
// Since wave 61, with the switch on (a debug `cratonvm` with
// `experimental-debug`): HotSpot's lines under `--jdk-only`, `--compatible`
// and `--nojit`. Positive control: `CRATONVM_FRAME_TRACE=1` prints
// `[JVMTI_EARLY_RETURN] recorded tid=<n> kind=I depth=<d>` (eight kinds),
// `[POP_FRAMES] jvmti recorded tid=<n> depth=<d>`, then
// `[POP_FRAMES] tid=<n> popped=1 top=L1W61JvmtiFrameControl.popCaller()I at=<bci> args=1`,
// and `[DEFERRED_FRAME_POP] recorded tid=<n> frame=<i>` / `applied ... added=1`.
public class L1W61JvmtiFrameControl {
    static native String caps();

    static native int suspend(Thread t);

    static native int resume(Thread t);

    static native int popFrame(Thread t);

    /** `GetFrameCount`, or `-err`. */
    static native int frameCount(Thread t);

    /** The method name at `depth` of `t`, or `err=<n>`. */
    static native String frameName(Thread t, int depth);

    /** The depth of the frame of the method named `name`, or -1. */
    static native int depthOf(Thread t, String name);

    static native int earlyInt(Thread t, int v);

    static native int earlyLong(Thread t, long v);

    static native int earlyFloat(Thread t, float v);

    static native int earlyDouble(Thread t, double v);

    static native int earlyObject(Thread t, Object v);

    static native int earlyVoid(Thread t);

    static native int notifyFramePop(Thread t, int depth);

    static native int clearAllFramePops(Thread t);

    /** The `FramePop` events recorded since the last call. */
    static native String events();

    static volatile boolean spinning;
    static volatile boolean release;
    static volatile boolean reachedEnd;
    static volatile int calls;
    static volatile Object result;

    static int intTarget(int x) {
        spinning = true;
        while (!release) {
        }
        return x + 1;
    }

    static boolean boolTarget() {
        spinning = true;
        while (!release) {
        }
        return false;
    }

    static long longTarget() {
        spinning = true;
        while (!release) {
        }
        return 1L;
    }

    static float floatTarget() {
        spinning = true;
        while (!release) {
        }
        return 1.0f;
    }

    static double doubleTarget() {
        spinning = true;
        while (!release) {
        }
        return 1.0;
    }

    static String stringTarget() {
        spinning = true;
        while (!release) {
        }
        return "normal";
    }

    static void voidTarget() {
        spinning = true;
        while (!release) {
        }
        reachedEnd = true;
    }

    static int popTarget(int n) {
        calls++;
        spinning = true;
        while (!release) {
        }
        return n * 2;
    }

    static int popCaller() {
        return popTarget(21);
    }

    static Thread start(String name, Runnable body) throws InterruptedException {
        spinning = false;
        release = false;
        reachedEnd = false;
        calls = 0;
        result = null;
        Thread t = new Thread(body, name);
        t.setDaemon(true);
        t.start();
        while (!spinning) {
            Thread.sleep(1);
        }
        return t;
    }

    static void finish(Thread t) throws InterruptedException {
        release = true;
        System.out.println("  resume: err=" + resume(t));
        t.join();
    }

    static void earlyInt() throws InterruptedException {
        Thread t = start("early-int", () -> result = intTarget(1));
        System.out.println("early int: suspend=" + suspend(t));
        System.out.println("  long=" + earlyLong(t, 5L) + " float=" + earlyFloat(t, 5f) + " double="
            + earlyDouble(t, 5.0) + " object=" + earlyObject(t, "x") + " void=" + earlyVoid(t));
        System.out.println("  int=" + earlyInt(t, 42) + " again=" + earlyInt(t, 43));
        finish(t);
        System.out.println("  returned " + result);
    }

    static void earlyOthers() throws InterruptedException {
        Thread t = start("early-boolean", () -> result = boolTarget());
        System.out.println("early boolean: suspend=" + suspend(t) + " int=" + earlyInt(t, 1));
        finish(t);
        System.out.println("  returned " + result);
        t = start("early-long", () -> result = longTarget());
        System.out.println("early long: suspend=" + suspend(t) + " int=" + earlyInt(t, 1) + " long="
            + earlyLong(t, 1L << 40));
        finish(t);
        System.out.println("  returned " + result);
        t = start("early-float", () -> result = floatTarget());
        System.out.println("early float: suspend=" + suspend(t) + " double=" + earlyDouble(t, 2.5)
            + " float=" + earlyFloat(t, 2.5f));
        finish(t);
        System.out.println("  returned " + result);
        t = start("early-double", () -> result = doubleTarget());
        System.out.println("early double: suspend=" + suspend(t) + " float=" + earlyFloat(t, 2.5f)
            + " double=" + earlyDouble(t, -0.5));
        finish(t);
        System.out.println("  returned " + result);
        t = start("early-object", () -> result = stringTarget());
        System.out.println("early object: suspend=" + suspend(t) + " integer=" + earlyObject(t, 7)
            + " int=" + earlyInt(t, 7) + " string=" + earlyObject(t, "forced"));
        finish(t);
        System.out.println("  returned " + result);
        t = start("early-null", () -> result = stringTarget());
        System.out.println("early null: suspend=" + suspend(t) + " null=" + earlyObject(t, null));
        finish(t);
        System.out.println("  returned " + result);
        t = start("early-void", () -> voidTarget());
        System.out.println("early void: suspend=" + suspend(t) + " int=" + earlyInt(t, 1) + " void="
            + earlyVoid(t));
        finish(t);
        System.out.println("  reached the end: " + reachedEnd);
    }

    static void pop() throws InterruptedException {
        Thread t = start("pop", () -> result = popCaller());
        System.out.println("pop: suspend=" + suspend(t));
        int before = frameCount(t);
        System.out.println("  notify depth 0: err=" + notifyFramePop(t, 0));
        System.out.println("  pop: err=" + popFrame(t) + " again=" + popFrame(t));
        int after = frameCount(t);
        System.out.println("  frames after - before: " + (after - before));
        System.out.println("  top: " + frameName(t, 0) + " next: " + frameName(t, 1));
        finish(t);
        System.out.println("  calls=" + calls + " returned " + result);
        System.out.print(events());
    }

    static void clear() throws InterruptedException {
        Thread t = start("clear", () -> result = intTarget(1));
        System.out.println("clear: suspend=" + suspend(t) + " notify=" + notifyFramePop(t, 0) + " clear="
            + clearAllFramePops(t) + " clear again=" + clearAllFramePops(t));
        finish(t);
        System.out.print(events());
        t = start("kept", () -> result = intTarget(1));
        System.out.println("kept: suspend=" + suspend(t) + " notify=" + notifyFramePop(t, 0));
        finish(t);
        System.out.print(events());
    }

    static void edges() throws InterruptedException {
        Thread t = start("running", () -> result = intTarget(1));
        System.out.println("running: pop=" + popFrame(t) + " int=" + earlyInt(t, 1) + " clear="
            + clearAllFramePops(t));
        release = true;
        t.join();
        Thread me = Thread.currentThread();
        System.out.println("current: pop=" + popFrame(me) + " int=" + earlyInt(me, 1) + " void="
            + earlyVoid(me) + " clear=" + clearAllFramePops(me) + " pop null=" + popFrame(null)
            + " void null=" + earlyVoid(null));
        spinning = false;
        release = false;
        Thread bottom = new Thread() {
            @Override
            public void run() {
                spinning = true;
                while (!release) {
                }
            }
        };
        bottom.setDaemon(true);
        bottom.start();
        while (!spinning) {
            Thread.sleep(1);
        }
        System.out.println("bottom: suspend=" + suspend(bottom) + " pop=" + popFrame(bottom));
        finish(bottom);
    }

    static void sleeper() throws InterruptedException {
        Thread t = start("sleeper", L1W61JvmtiFrameControl::sleeperBody);
        while (t.getState() != Thread.State.TIMED_WAITING) {
            Thread.sleep(1);
        }
        System.out.println("sleeper: suspend=" + suspend(t) + " pop=" + popFrame(t) + " void="
            + earlyVoid(t));
        int depth = depthOf(t, "sleepIn");
        System.out.println("  found sleepIn: " + (depth > 0) + " notify=" + notifyFramePop(t, depth)
            + " notify again=" + notifyFramePop(t, depth));
        System.out.println("  resume: err=" + resume(t));
        t.interrupt();
        t.join();
        System.out.print(events());
    }

    static void sleeperBody() {
        spinning = true;
        sleepIn();
    }

    static void sleepIn() {
        try {
            Thread.sleep(600_000);
        } catch (InterruptedException stop) {
            result = "interrupted";
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W61JvmtiFrameControl <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.print(caps());
        earlyInt();
        earlyOthers();
        pop();
        clear();
        edges();
        sleeper();
        earlyInt();
        pop();
    }
}
