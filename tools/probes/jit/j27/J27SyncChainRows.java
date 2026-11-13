// j27 wave 3 (lane BACKLOG, CN16-3): the IR planner's three sync-method
// chain conditions are now one per-compile binding
// (`deopt::ir_method_admits_chains`). Hot synchronized bodies with spliced
// callees, a callee exception caught inside the synchronized body, and a late
// receiver type that invalidates a speculated splice must answer the same
// with CRATONVM_JIT_IR_SYNC_METHOD_CHAINS unset and =1.
// Page: docs/feature-designs/jit-ir-optimizations.md (CN16-3)
//
// Expected HotSpot 25 output (facts only), both switch settings:
//   fact sync-inst-sum 999999000000
//   fact sync-static-sum 999997000000
//   fact sync-caught 2000
//   fact sync-holds true
//   fact sync-late-sum 2000020000
// (Values computed off-line from the Java definitions; confirm on the
// HotSpot reference run.)
public class J27SyncChainRows {
    static class Adder {
        long add(long a, int b) {
            return a + b;
        }
    }

    static final class Adder2 extends Adder {
        long add(long a, int b) {
            return a + b + 1;
        }
    }

    static int check(int i) {
        if (i % 100 == 99) {
            throw new IllegalStateException("x" + i);
        }
        return i;
    }

    long total;
    Adder adder = new Adder();

    synchronized long instSum(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s = adder.add(s, i);
        }
        total += s;
        return s;
    }

    static synchronized long staticSum(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i - 1;
        }
        return s;
    }

    synchronized int caught(int n) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            try {
                check(i);
            } catch (IllegalStateException e) {
                c++;
            }
        }
        return c;
    }

    synchronized boolean holds() {
        return Thread.holdsLock(this);
    }

    public static void main(String[] args) {
        J27SyncChainRows r = new J27SyncChainRows();
        long inst = 0;
        long stat = 0;
        int caught = 0;
        boolean holds = true;
        for (int round = 0; round < 10; round++) {
            inst += r.instSum(1000000) / 10;
            stat += staticSum(1000000) / 10;
            caught += r.caught(20000) / 10;
            holds &= r.holds();
        }
        System.out.println("fact sync-inst-sum " + inst * 2);
        System.out.println("fact sync-static-sum " + stat * 2);
        System.out.println("fact sync-caught " + caught * 10);
        System.out.println("fact sync-holds " + holds);
        // A second receiver type after the body is hot: the splice of
        // Adder.add is no longer the only target.
        r.adder = new Adder2();
        long late = r.instSum(100000) + r.instSum(100000);
        System.out.println("fact sync-late-sum " + late / 5);
    }
}
