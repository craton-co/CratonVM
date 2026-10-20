// Interpreter round i2 wave 62, lane L7: a C JVMTI observer armed while a
// loop runs is seen by that loop -- the agent half of the control for
// `CRATONVM_INTERP_HOISTED_OBSERVERS` (proposal
// i61-L7-proposal-the-loop-hoists-the-observer-flags-and-the-poll-word-refreshes-them-20261025.md;
// the JDWP half, field watches included, is L7W62JdiObserverArmedMidLoop).
// With the switch on, the dispatch loop no longer re-reads the debugger gate
// and the single-step union at its back edges; it reads them where every
// writer sends it, its loop-top slow path. A writer that missed that bump
// would leave the loop blind, and a row below would print `seen=no` after
// waiting ten seconds.
//
// Each row starts a worker in a call-free loop (the method-entry row's loop
// makes one call), lets it run 300 ms, arms a JVMTI observer FROM THE MAIN
// THREAD (single stepping for the worker, method entry for the worker,
// breakpoints on every line of the loop's method), and waits up to ten
// seconds for an event from the worker's loop; the `self` row arms single
// stepping from inside the loop (a native call in the loop body) and counts
// the steps the rest of the loop takes. The rows run twice (the cold-VM
// first-call rule). No field watch: the C JVMTI table has none yet (slots
// 41-44, item 5 of
// docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007.md).
//
// Needs the native shim tools/probes/interp/L7/L7W62ObserverArmedMidLoop.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl7w62obs.so tools/probes/interp/L7/L7W62ObserverArmedMidLoop.c
//   javac -d /tmp/l7w62obs tools/probes/interp/L7/L7W62ObserverArmedMidLoop.java
//   java     -agentpath:/tmp/libl7w62obs.so -cp /tmp/l7w62obs L7W62ObserverArmedMidLoop /tmp/libl7w62obs.so
//   cratonvm -agentpath:/tmp/libl7w62obs.so -cp /tmp/l7w62obs L7W62ObserverArmedMidLoop /tmp/libl7w62obs.so
//
// Run it under CratonVM with `CRATONVM_INTERP_HOISTED_OBSERVERS=1` and `=0`,
// with and without `--nojit`: every variant must print the HotSpot lines.
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L7/rust-ports/L7W62ObserverArmedMidLoop (built
// like the ports of tools/probes/interp/L1/rust-ports/README.md).
//
// Positive control (wave 62, a debug `cratonvm` with `experimental-debug`,
// `--nojit`, the switch on): with the writers' bumps disabled by a temporary
// edit, the `step` and `self` rows print `seen=no` (each after its ten
// seconds); with them, every row matches HotSpot. The `entry` row reads a
// union the loop does not hoist (stage 2), and setting a JVMTI breakpoint
// reaches the loop's slow path another way (measured: the row passes with the
// bumps disabled), so those two rows pass either way.
// The main thread's method entry, enabled first (`pre-arm`), is what keeps
// the arming of the later rows free of a pause.
//
// Expected stdout (HotSpot 25.0.3, default flags): MEASURED on the Windows
// box with the Rust port of the C shim, three runs, the same each time:
//
//   pre-arm=0
//   step: arm=0 seen=yes
//   entry: arm=0 seen=yes
//   breakpoint: arm=0 seen=yes
//   self: seen=yes
//   step: arm=0 seen=yes
//   entry: arm=0 seen=yes
//   breakpoint: arm=0 seen=yes
//   self: seen=yes
//   done
//
// stderr: HotSpot's four `WARNING:` lines about `System::load` (a restricted
// method); exit code 0.
public class L7W62ObserverArmedMidLoop {
    static final int SINGLE_STEP = 60;
    static final int METHOD_ENTRY = 65;

    /** `SetEventNotificationMode(on ? ENABLE : DISABLE, event, thread)`. */
    static native int setEvent(boolean on, int event, Thread thread);

    /** `SetBreakpoint` / `ClearBreakpoint` at every line of `spinBreak`. */
    static native int breakpoints(Class<?> owner, boolean on);

    /** Events counted in (or, for method entry, of) the method `which` names. */
    static native int count(int which);

    /** Zero every count. */
    static native void reset();

    // The methods `count` knows, by index (the shim's table).
    static final int SPIN_STEP = 0;
    static final int TICK = 1;
    static final int SPIN_BREAK = 2;
    static final int SPIN_SELF = 3;

    static volatile boolean stop;
    static volatile boolean running;
    static long sink;

    static void spinStep() {
        long s = 0;
        int i = 0;
        running = true;
        while (!stop) {
            s += i;
            i++;
        }
        sink += s;
    }

    static int tick(int x) {
        return x + 1;
    }

    static void spinEntry() {
        int s = 0;
        running = true;
        while (!stop) {
            s = tick(s);
        }
        sink += s;
    }

    static void spinBreak() {
        long s = 0;
        int i = 0;
        running = true;
        while (!stop) {
            s += i;
            i++;
        }
        sink += s;
    }

    static void spinSelf() {
        long s = 0;
        Thread me = Thread.currentThread();
        for (int i = 0; i < 400_000; i++) {
            if (i == 300_000) {
                setEvent(true, SINGLE_STEP, me);
            }
            s += i;
        }
        setEvent(false, SINGLE_STEP, me);
        sink += s;
    }

    interface Arm {
        int apply(Thread worker, boolean on);
    }

    static String row(String label, Runnable body, Arm arm, int which) throws Exception {
        reset();
        stop = false;
        running = false;
        Thread worker = new Thread(body, "l7w62-" + label);
        worker.start();
        while (!running) {
            Thread.onSpinWait();
        }
        Thread.sleep(300);
        int err = arm.apply(worker, true);
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (count(which) == 0 && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
        boolean seen = count(which) > 0;
        arm.apply(worker, false);
        stop = true;
        worker.join();
        return label + ": arm=" + err + " seen=" + (seen ? "yes" : "no");
    }

    static String selfRow() throws Exception {
        reset();
        Thread worker = new Thread(L7W62ObserverArmedMidLoop::spinSelf, "l7w62-self");
        worker.start();
        worker.join();
        return "self: seen=" + (count(SPIN_SELF) > 0 ? "yes" : "no");
    }

    static void rows() throws Exception {
        System.out.println(row("step", L7W62ObserverArmedMidLoop::spinStep,
                (t, on) -> setEvent(on, SINGLE_STEP, t), SPIN_STEP));
        System.out.println(row("entry", L7W62ObserverArmedMidLoop::spinEntry,
                (t, on) -> setEvent(on, METHOD_ENTRY, t), TICK));
        System.out.println(row("breakpoint", L7W62ObserverArmedMidLoop::spinBreak,
                (t, on) -> breakpoints(L7W62ObserverArmedMidLoop.class, on), SPIN_BREAK));
        System.out.println(selfRow());
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L7W62ObserverArmedMidLoop <path of the shim library>");
            return;
        }
        System.load(args[0]);
        // Method entry for the main thread, for the whole run: the VM enters
        // its interpreter-only mode now, once, so arming a row's observer
        // later takes no withdrawal pause (a pause also sends every loop to
        // its slow path, and would hide a missed bump).
        System.out.println("pre-arm=" + setEvent(true, METHOD_ENTRY, Thread.currentThread()));
        rows();
        rows();
        System.out.println("done");
    }
}
