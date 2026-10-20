// zr z2/g2 (2026-10-12): the Java-level reproduction
// docs/internal/gc/zgc-zr1gn-unmarked-objects-below-the-floor-stay-registered-FIXED-20261012.md
// asks for.
//
// The heap state the page describes: a young cycle pre-marks by header AGE and
// sweeps by ADDRESS ([floor, cursor) plus young grains). An object that
// survived a whole-heap cycle without reaching the promotion age sits BELOW the
// floor and is still young. Dead, it is neither marked nor swept -- it stays
// registered -- while the fresh child only it reached is in the nursery and is
// freed. The VM's post-GC liveness verdict on ZGC is registry membership, so a
// WeakReference to such a parent can survive the cycle and hand the program a
// parent whose `child` field names freed (and here: deliberately overwritten)
// memory. HotSpot never does that: a referent it keeps is whole.
//
// Shape, per attempt:
//   1. `per` parents, each weakly referenced, plus ~600 KiB of held padding
//      allocated after them (so no parent shares a 64 KiB young grain with
//      the floor or with later allocations);
//   2. two System.gc(): at CRATONVM_ZGC_GEN_MINORS_PER_MAJOR=1 the cycles
//      alternate young/whole-heap, so one of the two is whole-heap and leaves
//      the parents below the floor at age 1-2 (< promotion age 3);
//   3. every parent gets a FRESH child (nursery), then the strong array is
//      dropped: the parents are weakly reachable only;
//   4. one System.gc() -- on every other attempt a young cycle;
//   5. 200k filler long[4] overwrite whatever that cycle freed;
//   6. every WeakReference still non-null must lead to an intact child.
//
// Output (stdout, deterministic, compared against HotSpot):
//   zr2g2-weak: attempts=.. per_attempt=.. corrupt=<n> ok=<corrupt==0>
// stderr (GC-timing dependent, NOT compared): live/cleared counts.
//
// Usage: java Zr2G2WeakBelowFloor [attempts=40] [perAttempt=256]
// Rows: docs/internal/zgc-round-zr-20261012/z2-g2-report.md (W0..W3).
import java.lang.ref.WeakReference;

public class Zr2G2WeakBelowFloor {
    static final class Child {
        final long value;
        long pad0, pad1;
        Child(long v) { value = v; }
    }

    static final class Parent {
        final long id;
        Child child;
        Parent(long id) { this.id = id; }
    }

    static final long SALT = 0x5DEECE66DL;
    static volatile Object sink;

    public static void main(String[] args) {
        int attempts = arg(args, 0, 40);
        int per = arg(args, 1, 256);

        // An old generation, so young cycles can happen at all: anchors that
        // live all run and are aged past the promotion age by full cycles.
        Object[] anchors = new Object[20_000];
        for (int i = 0; i < anchors.length; i++) anchors[i] = new long[4];
        for (int i = 0; i < 4; i++) {
            churn(20_000);
            System.gc();
        }

        long corrupt = 0, live = 0, cleared = 0;
        for (int t = 0; t < attempts; t++) {
            Parent[] ps = new Parent[per];
            @SuppressWarnings("unchecked")
            WeakReference<Parent>[] ws = new WeakReference[per];
            for (int i = 0; i < per; i++) {
                ps[i] = new Parent((long) t * 1_000_003L + i);
                ws[i] = new WeakReference<>(ps[i]);
            }
            Object[] pad = new Object[8192];
            for (int j = 0; j < pad.length; j++) pad[j] = new long[8];
            System.gc();
            System.gc();
            for (int i = 0; i < per; i++) ps[i].child = new Child(ps[i].id ^ SALT);
            ps = null;
            System.gc();
            for (int k = 0; k < 200_000; k++) {
                long[] f = new long[4];
                f[0] = f[1] = f[2] = f[3] = 0x7A7A7A7A7A7A7A7AL;
                sink = f;
            }
            for (int i = 0; i < per; i++) {
                Parent p = ws[i].get();
                if (p == null) {
                    cleared++;
                    continue;
                }
                live++;
                Child c = p.child;
                if (c == null || c.value != (p.id ^ SALT)) corrupt++;
            }
            sink = pad;
        }
        sink = anchors;
        System.out.println("zr2g2-weak: attempts=" + attempts + " per_attempt=" + per
                + " corrupt=" + corrupt + " ok=" + (corrupt == 0));
        System.err.println("zr2g2-weak-detail: live_after=" + live + " cleared=" + cleared);
        if (corrupt != 0) System.exit(3);
    }

    static void churn(int n) {
        for (int i = 0; i < n; i++) sink = new long[4];
    }

    static int arg(String[] a, int i, int d) {
        return a.length > i ? Integer.parseInt(a[i]) : d;
    }
}
