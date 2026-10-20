// JIT round 21 wave 1, lane indy: a `static final` table read before its class's <clinit> ends.
//
// Target (r20 handoff perf table, R20PIRTableMod late-init): 2054 ns per late(64) on w11d, 185 after
// CRATONVM_JIT_STATIC_INIT_RECOMPILE (w17s; its control row is ~147), against HotSpot 50. That
// switch recompiles a body compiled while the holder was uninitialized once the holder's <clinit>
// ENDS. What it cannot serve is a read made while the class is still being initialized -- by the
// initializing thread itself, from code its <clinit> runs. Both tiers lower such a `getstatic` to
// the `jit_getstatic` helper (an inline load runs no <clinit>; `jit_resolve_static_base` answers
// only an initialized class), and the helper, finding the class not initialized, runs the whole
// `ensure_class_initialized_shared` walk (the JVMS 5.5 step 3 recursive request: Ok) and then the
// `Initialized` re-check (false) -- three tracked class-manager reads per static read. HotSpot
// compiles a class-initialization barrier there (`init_thread == current`) and reads inline.
//
// CRATONVM_JIT_GETSTATIC_OWN_CLINIT_FAST=1 (r21 lane indy, opt-in) answers that recursive request
// from one untracked class-manager read (`jit_bridge::class_initializing_on`). Rows:
//   in-clinit        late(64) timed INSIDE Late.<clinit> (the helper path; the switch's row)
//   after-init       late(64) after Late's <clinit> ended (CRATONVM_JIT_STATIC_INIT_RECOMPILE's row)
//   control          the same kernel over an initialized class's table
// Facts: the in-<clinit> values, another thread's read waiting for the <clinit> to finish, and a
// failed <clinit> read from compiled code (NoClassDefFoundError afterwards, never a stale value).
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21LateInitTable.java):
//   P=tools/probes/out
//   cratonvm -Xmx256m -cp $P R21LateInitTable                                     # default
//   CRATONVM_JIT_GETSTATIC_OWN_CLINIT_FAST=1 cratonvm -Xmx256m -cp $P R21LateInitTable
//   CRATONVM_JIT_IR_CLINIT_BARRIER=1 cratonvm -Xmx256m -cp $P R21LateInitTable  # INDY21-2
//   CRATONVM_JIT_STATIC_INIT_RACE_SERVE=0 cratonvm -Xmx256m -cp $P R21LateInitTable  # INDY21-3 kill
//   CRATONVM_JIT_STATIC_INIT_RECOMPILE=0 cratonvm -Xmx256m -cp $P R21LateInitTable  # after-init control
//   cratonvm --compatible -Xmx256m -cp $P R21LateInitTable                        # both modes
//   java -Xmx256m -cp $P R21LateInitTable                                         # HotSpot 25
// Engagement: CRATONVM_GS_PROF-style counters are not needed; CRATONVM_DBG_JITC=1 shows
//   `static-init WAITING R21LateInitTable.late(I)I on class id ...` (the kernel compiled during
//   the <clinit>) and `late-bind recompile requested ...#static-init:...` after it.
//   INDY21-2 engagement: `ir clinit barrier getstatic n.. class_id=..` for the optimizing body of
//   late(I)I compiled during the <clinit> (the single-pass tier keeps the helper). INDY21-3:
//   `static-init RACE` lines only when a holder finished initializing during a compile.
//
// HotSpot expectation: every non-`time` line identical. time rows ~30-60 ns per late(64) on all
// three rows. Pass for this lane's target: in-clinit within ~3x control under the opt-in.

import java.util.Arrays;

public class R21LateInitTable {
    static final int REPS = 9;
    static final int CALLS = 20_000;
    static int sink;

    static final class Late {
        static final int[] LATE;
        static int inClinitSum;
        static Thread waiter;
        static volatile int waiterSaw = -1;
        static {
            int[] t = new int[37];
            for (int i = 0; i < t.length; i++) t[i] = i * 7 + 3;
            LATE = t;
            // Another thread's read must wait for this <clinit> to finish (JVMS 5.5 step 2).
            waiter = new Thread(() -> waiterSaw = Late.LATE.length + Late.inClinitSum);
            waiter.start();
            inClinitSum = late(64);
            row("in-clinit", 0);
        }
    }

    static final class Ready {
        static final int[] TABLE;
        static {
            int[] t = new int[37];
            for (int i = 0; i < t.length; i++) t[i] = i * 7 + 3;
            TABLE = t;
        }
    }

    static boolean fail = true;

    static final class Bad {
        static final int[] T;
        static int seen;
        static {
            T = new int[] {5, 6, 7};
            for (int i = 0; i < 20_000; i++) seen += bad();
            if (fail) throw new IllegalStateException("boom");
        }
    }

    static int late(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) s += Late.LATE[i % Late.LATE.length] + Late.LATE.length;
        return s;
    }

    static int ready(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) s += Ready.TABLE[i % Ready.TABLE.length] + Ready.TABLE.length;
        return s;
    }

    static int bad() {
        return Bad.T[1] + Bad.T.length;
    }

    /** ns per kernel call; which: 0 late, 1 ready. */
    static double once(int which) {
        long t0 = System.nanoTime();
        int s = 0;
        for (int i = 0; i < CALLS; i++) s += which == 0 ? late(64) : ready(64);
        sink += s;
        return (System.nanoTime() - t0) / (double) CALLS;
    }

    static void row(String name, int which) {
        for (int w = 0; w < 3; w++) once(which);
        double[] t = new double[REPS];
        for (int i = 0; i < REPS; i++) t[i] = once(which);
        Arrays.sort(t);
        System.out.println("time " + name + " " + Math.round(t[REPS / 2]) + " ns/op");
    }

    public static void main(String[] args) throws Exception {
        // Touch Late: its <clinit> times the in-clinit row.
        System.out.println("fact late-length " + Late.LATE.length);
        System.out.println("fact late-in-clinit-sum " + Late.inClinitSum);
        Late.waiter.join();
        System.out.println("fact late-waiter-saw " + Late.waiterSaw);
        System.out.println("fact late-after " + late(64) + " ready " + ready(64));
        row("after-init", 0);
        row("control", 1);
        for (int k = 0; k < 2; k++) {
            try {
                System.out.println("fact bad-" + k + " " + bad());
            } catch (Throwable e) {
                Throwable c = e.getCause();
                System.out.println("fact bad-" + k + " " + e.getClass().getName() + ": " + e.getMessage()
                        + (c == null ? "" : " / cause " + c.getClass().getName() + ": " + c.getMessage()));
            }
        }
        System.out.println("fact sink-parity " + (sink & 1));
    }
}
