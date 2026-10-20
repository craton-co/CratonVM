/**
 * nio21 wave 18 lane L3: the loop field forward reads a forwarded field again right after a call
 * on a cold arm ({@code CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD}, default ON). Every row here
 * has a call in the loop that WRITES the forwarded field (or a field next to it), so a reload that
 * was missing, misplaced or read through the wrong field would show up as a different sum.
 *
 * <p>Conformance (default, prints answers only; must match HotSpot 25 byte for byte). Each loop is
 * warmed with the cold arm taken once in 64 iterations, so the profile keeps it and never-taken
 * pruning does not remove the call. Then it is run with other periods, including 1 (every
 * iteration) and one longer than the loop (never).
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21LoopReload.java
 *   java -cp /tmp/p21 Nio21LoopReload                                     # HotSpot facts
 *   cratonvm -cp /tmp/p21 Nio21LoopReload                                 # --jdk-only
 *   cratonvm --compatible -cp /tmp/p21 Nio21LoopReload
 *   CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD=0 cratonvm -cp /tmp/p21 Nio21LoopReload
 *   CRATONVM_DBG=ir-compiles cratonvm -cp /tmp/p21 Nio21LoopReload 2>&1 | grep 'loop field forward'
 *   cratonvm -cp /tmp/p21 -Dnio21.rows=reload Nio21LoopReload             # bench: reload N ns/op
 * </pre>
 */
public class Nio21LoopReload {
    static final String ROWS = System.getProperty("nio21.rows", "");
    static final int WARM = Integer.getInteger("nio21.warm", 4_000);
    static volatile long sink;

    static final class Cell {
        int x;
        int y;
        long z;
        Object ref;
        int[] arr = new int[8];
    }

    static int calls;

    /** A void writer of {@code c.x} and {@code c.ref}, too big to splice. */
    static void bump(Cell c, int k) {
        int t = k;
        for (int j = 0; j < (k & 3) + 1; j++) {
            t += j * 31 + k;
            t ^= (t << 3);
            t = Integer.rotateLeft(t, 5);
        }
        c.x += 100 + (t & 1);
        c.ref = ((t & 2) == 0) ? "a" : "bb";
        calls++;
    }

    /** An int-returning writer of {@code c.x} and {@code c.z}. */
    static int bumpAndGet(Cell c, int k) {
        int t = k;
        for (int j = 0; j < (k & 3) + 1; j++) {
            t += j * 17 + k;
            t ^= (t >>> 2);
        }
        c.x -= 7;
        c.z += t & 15;
        calls++;
        return t & 255;
    }

    /** A writer of a DIFFERENT object's field of the same class. */
    static void bumpOther(Cell other, int k) {
        for (int j = 0; j < (k & 3) + 1; j++) {
            other.x += j;
        }
        other.y = k;
        calls++;
    }

    /** A writer that replaces the array the loop indexes. */
    static void swapArr(Cell c, int k) {
        int[] a = new int[8 + (k & 7)];
        for (int j = 0; j < a.length; j++) {
            a[j] = j * k;
        }
        c.arr = a;
        calls++;
    }

    /** A call that writes nothing the loop reads. */
    static int pure(int k) {
        int t = k;
        for (int j = 0; j < (k & 3) + 1; j++) {
            t = t * 31 + j;
        }
        calls++;
        return t;
    }

    static long voidWriter(Cell c, int n, int every) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += c.x;
            if (i % every == every - 1) {
                bump(c, i);
            }
            s += (c.ref == null) ? 0 : c.ref.hashCode() & 7;
            c.y = i;
        }
        return s;
    }

    static long valueWriter(Cell c, int n, int every) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += c.x + c.z;
            if (i % every == every - 1) {
                s += bumpAndGet(c, i);
            }
            s += c.x;
        }
        return s;
    }

    static long otherWriter(Cell c, Cell other, int n, int every) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += c.x * 3 + other.x;
            if (i % every == every - 1) {
                bumpOther(other, i);
            }
            c.x += 1;
        }
        return s;
    }

    static long arrayWriter(Cell c, int n, int every) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            int[] a = c.arr;
            s += a[i % a.length];
            if (i % every == every - 1) {
                swapArr(c, i);
            }
        }
        return s;
    }

    static long pureCall(Cell c, int n, int every) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += c.x;
            if (i % every == every - 1) {
                s += pure(i) & 3;
            }
            c.x += 2;
        }
        return s;
    }

    static Cell fresh() {
        Cell c = new Cell();
        c.x = 5;
        c.z = 1;
        for (int j = 0; j < c.arr.length; j++) {
            c.arr[j] = j + 1;
        }
        return c;
    }

    static String run(int n, int every) {
        calls = 0;
        Cell a = fresh();
        long v = voidWriter(a, n, every);
        Cell b = fresh();
        long w = valueWriter(b, n, every);
        Cell c = fresh();
        Cell o = fresh();
        long x = otherWriter(c, o, n, every);
        Cell d = fresh();
        long y = arrayWriter(d, n, every);
        Cell e = fresh();
        long z = pureCall(e, n, every);
        return "n=" + n + " every=" + every + " void=" + v + "/" + a.x + "/" + a.ref
                + " value=" + w + "/" + b.x + "/" + b.z
                + " other=" + x + "/" + c.x + "/" + o.x + "/" + o.y
                + " array=" + y + "/" + d.arr.length
                + " pure=" + z + "/" + e.x
                + " calls=" + calls;
    }

    public static void main(String[] args) {
        if (!ROWS.isEmpty()) {
            bench();
            return;
        }
        long warm = 0;
        for (int r = 0; r < WARM; r++) {
            warm += run(512, 64).length();
        }
        System.out.println("warm " + warm);
        int[] periods = {1, 2, 7, 64, 513, 100_000};
        for (int every : periods) {
            System.out.println(run(512, every));
        }
        for (int every : periods) {
            System.out.println(run(3, every));
        }
    }

    static void bench() {
        Cell c = fresh();
        for (int r = 0; r < 20_000; r++) {
            sink += voidWriter(c, 256, 64);
            c.x = 5;
        }
        for (String row : ROWS.split(",")) {
            if (!row.equals("reload")) {
                continue;
            }
            int reps = 200_000;
            long t0 = System.nanoTime();
            for (int r = 0; r < reps; r++) {
                sink += voidWriter(c, 256, 64);
                c.x = 5;
            }
            long dt = System.nanoTime() - t0;
            System.out.println("reload " + (dt / reps) + " ns/op");
        }
    }
}
