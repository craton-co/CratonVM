// Interpreter round i1 wave 6, lane L7 -- OSR refusals that are now denials.
// (Wave 10: `synchronized` is no longer an OSR gate at all -- patch P1 of
// i1-L7-osr-optimizing-door-bypasses-osr-admission -- so the synchronized
// loops below now go OSR like their twin; the stdout is unchanged.)
//
// `compile_osr_artifact` refused a `synchronized` method's hot loop (the
// compiled entry has no monitor prologue); wave 6 makes that refusal deny OSR
// for the method (under background compilation, the default) instead of
// re-offering it five times per activation and spending background compile
// retries. This probe runs such loops over many activations -- each loop long
// enough to cross the per-frame OSR offer threshold -- in a static and an
// instance `synchronized` method, beside an unsynchronized twin whose loop DOES
// go OSR, and through an interface call inside a loop (the MIC helper's arms,
// which wave 6 also changed). Every result must be exact.
//
// HotSpot 25 prints exactly:
//   static-sync   4999000000
//   inst-sync     4999000000
//   plain         4999000000
//   iface         4000000
//   counter       800
//
// Compare `--compatible` with and without `--nojit`: identical stdout. With
// CRATONVM_DBG_JITC=1, stderr should show the synchronized methods refused
// once each rather than once per activation.
public class L7SyncLoopOsrDenied {
    interface Step {
        long step(long acc, int i);
    }

    static final class Add implements Step {
        public long step(long acc, int i) {
            return acc + (i & 1);
        }
    }

    static final class AddTwice implements Step {
        public long step(long acc, int i) {
            return acc + 2 * (i & 1);
        }
    }

    static int counter;

    static synchronized long staticSync(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        counter++;
        return s;
    }

    synchronized long instSync(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        counter++;
        return s;
    }

    static long plain(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static long viaInterface(Step[] steps, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc = steps[i & 1].step(acc, i);
        }
        return acc;
    }

    public static void main(String[] a) {
        L7SyncLoopOsrDenied self = new L7SyncLoopOsrDenied();
        long st = 0;
        long in = 0;
        long pl = 0;
        // 5 000 iterations per activation: past the 1 000-back-edge OSR offer
        // threshold within every activation.
        for (int r = 0; r < 400; r++) {
            st += staticSync(5_000);
            in += self.instSync(5_000);
            pl += plain(5_000);
        }
        System.out.println("static-sync   " + st);
        System.out.println("inst-sync     " + in);
        System.out.println("plain         " + pl);
        Step[] steps = {new Add(), new AddTwice()};
        long it = 0;
        for (int r = 0; r < 200; r++) {
            it += viaInterface(steps, 20_000);
        }
        System.out.println("iface         " + it);
        System.out.println("counter       " + counter);
    }
}
