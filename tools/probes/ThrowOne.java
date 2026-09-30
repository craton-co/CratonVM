/**
 * One {@link ThrowCost} kernel per process, for profiling a single path.
 *
 *   java ThrowOne {new|thrown|minted|rte} ITERATIONS ROUNDS
 *
 * `new`    - `new ArrayIndexOutOfBoundsException(msg)`, no throw (ThrowCost's javaNewOnly)
 * `thrown` - the same, thrown and caught in the same method (javaNewThrown)
 * `minted` - a bounds check's AIOOBE, caught in the same method (vmMinted)
 * `rte`    - `new RuntimeException(msg)`, a two-level `<init>` chain
 *
 * Prints ns per iteration for each round. ThrowCost runs every kernel in one
 * process, so a profile of it mixes them; this is the probe
 * docs/internal/fixed-bugs/perf-building-a-throwable-costs-2-microseconds-FIXED-20260923.md
 * was profiled with.
 */
public final class ThrowOne {
    static final int[] A = new int[4];
    static int sink;

    static int len(Throwable e) {
        String m = e.getMessage();
        return m == null ? 0 : m.length();
    }

    static int newOnly(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            ArrayIndexOutOfBoundsException e =
                new ArrayIndexOutOfBoundsException("Index 9 out of bounds for length 4");
            s += len(e);
        }
        return s;
    }

    static int thrown(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            try {
                throw new ArrayIndexOutOfBoundsException("Index 9 out of bounds for length 4");
            } catch (ArrayIndexOutOfBoundsException e) {
                s += len(e);
            }
        }
        return s;
    }

    static int minted(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            try {
                s += A[i + 8];
            } catch (ArrayIndexOutOfBoundsException e) {
                s += len(e);
            }
        }
        return s;
    }

    static int rte(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            RuntimeException e = new RuntimeException("x");
            s += len(e);
        }
        return s;
    }

    public static void main(String[] a) {
        String k = a[0];
        int n = Integer.parseInt(a[1]);
        int r = Integer.parseInt(a[2]);
        for (int j = 0; j < r; j++) {
            long t0 = System.nanoTime();
            switch (k) {
                case "new": sink += newOnly(n); break;
                case "thrown": sink += thrown(n); break;
                case "minted": sink += minted(n); break;
                default: sink += rte(n);
            }
            System.out.println(k + " " + ((System.nanoTime() - t0) / n));
        }
        System.out.println("sink " + sink);
    }
}
