// Lane L2 probe (interpreter round i1 wave 22): ACC_SYNCHRONIZED methods with
// loops. Their optimizing (IR) METHOD-ENTRY bodies now give the loop's
// back-edge poll a mode exit (`ir_lower::ir_entry_mode_exits_admitted`), so a
// loop running in such a body leaves for the interpreter when an agent needs
// it; the doors that hold the method monitor hand it to the resumed frame
// (`jit_bridge::hand_monitor_to_resumed_frame`), which releases it once when
// it returns. A synchronized body whose graph can neither trap nor call
// (`pureSum`) keeps no exit, so a compiled caller keeps CALLing it directly
// under its own hold of the monitor (the caller-held sync-direct site).
// Docs: docs/internal/fixed-bugs/interpreter-L2-ir-entry-exits-refused-for-monitor-methods-FIXED-20260926.md
//
// Plain run (what the probe runner diffs), with and without --nojit, under
// --compatible and the default: stdout must equal HotSpot 25's:
//
//   divLoop=46367 arrayLoop=504 pureSum=1999000
//   trap call divLoop=-1 sideEffects=2
//   other thread took both locks=true
//   main holds a lock=false
//
// Without an agent no exit is taken; the lines check that the new exits and
// their pads change no result, that the guard trap in `divLoop` (a division
// by zero caught inside the method, never seen while warming up) is resumed
// once (sideEffects=2, not 3 or 4), and that both monitors are released
// exactly once afterwards (a lock left held makes the third line false: the
// daemon's join times out; one released twice throws
// IllegalMonitorStateException).
//
// Coverage (no extra setup): CRATONVM_DBG_JITC=1 prints at exit
//   [c2-supersede] ir entry poll mode exits: given=N | refused: ...
// `divLoop` and `arrayLoop` now contribute given exits when the optimizing
// tier compiled them at entry (before wave 22 they sought none); `pureSum`
// seeks none and counts nothing.
//
// By hand, under a debugger (the case the wave fixes; the runner cannot
// attach one): compile with `javac -g`, run with
//   -agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=5005 L2W22SyncMethodLoopExit long
// attach `jdb -attach 5005`, `run`, wait for "long call started" on stdout,
// then `stop at L2W22SyncMethodLoopExit:57` (the loop body of `divLoop`).
// HotSpot stops there within the running call. Before wave 22 CratonVM
// finished that call's loop compiled when the optimizing tier had compiled
// `divLoop` at entry, so the breakpoint fired only on a later call; now the
// back-edge poll leaves and the breakpoint fires in the same activation.
public class L2W22SyncMethodLoopExit {
    static final Object DONE = new Object();
    static int sideEffects;
    static int divisor = 3;

    int[] data = new int[64];

    /// A synchronized static loop whose division can trap: its graph can
    /// trap, so its method-entry body gets a back-edge exit.
    static synchronized long divLoop(int n, int d) {
        long s = 0;
        sideEffects++;
        try {
            for (int i = 0; i < n; i++) {
                s += (i * 7) / d; // line 57: the by-hand breakpoint
            }
        } catch (ArithmeticException e) {
            s = -1;
        }
        sideEffects++;
        return s;
    }

    /// A synchronized instance loop over an array (bounds and null checks).
    synchronized long arrayLoop(int reps) {
        long s = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < data.length; i++) {
                s += data[i];
            }
        }
        return s;
    }

    /// Trap-free and call-free: no exit, so a compiled caller may keep
    /// CALLing it directly while holding the class monitor itself.
    static synchronized int pureSum(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        L2W22SyncMethodLoopExit o = new L2W22SyncMethodLoopExit();
        for (int i = 0; i < o.data.length; i++) {
            o.data[i] = i % 5;
        }
        long t0 = System.nanoTime();
        long div = 0;
        long arr = 0;
        long pure = 0;
        for (int r = 0; r < 20_000; r++) {
            div = divLoop(200, divisor);
            arr = o.arrayLoop(4);
            pure = pureSum(2_000);
        }
        System.err.printf("warm-up: %.2f ms%n", (System.nanoTime() - t0) / 1e6);
        if (args.length > 0 && args[0].equals("long")) {
            // For the by-hand debugger check: one long call to set a
            // breakpoint in.
            System.out.println("long call started");
            System.out.flush();
            long s = 0;
            for (int k = 0; k < 2_000; k++) {
                s += divLoop(1_000_000, divisor);
            }
            System.out.println("long call done " + s);
        }
        System.out.println("divLoop=" + div + " arrayLoop=" + arr + " pureSum=" + pure);
        int before = sideEffects;
        long trapped = divLoop(200, 0);
        System.out.println("trap call divLoop=" + trapped + " sideEffects=" + (sideEffects - before));
        Thread other = new Thread(() -> {
            synchronized (L2W22SyncMethodLoopExit.class) {
                synchronized (o) {
                    synchronized (DONE) {
                        DONE.notifyAll();
                    }
                }
            }
        });
        other.setDaemon(true);
        other.start();
        other.join(10_000);
        System.out.println("other thread took both locks=" + !other.isAlive());
        System.out.println(
                "main holds a lock="
                        + (Thread.holdsLock(L2W22SyncMethodLoopExit.class) || Thread.holdsLock(o)));
    }
}
