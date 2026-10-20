/*
 * Interpreter round i1, wave 22, lane L7: the per-access cost of a quickened
 * `getfield` hit (`field_fast::getfield_fast_keyed`).
 *
 * The hit path carries eleven phase boundaries of the `CRATONVM_DBG_FIELD_PHASES`
 * instrument, and each boundary's `now()` / `charge()` re-read the
 * instrument's gate byte, so an unarmed field read paid about a dozen relaxed
 * loads and branches for an instrument nobody enabled. Wave 22 reads the gate
 * once per access (`field_phases::now_if` / `charge_if`).
 *
 * Run: CratonVM `--nojit` (the interpreter; with the JIT the loops compile),
 * A/B against the previous build, interleaved; timings (ns per loop
 * iteration, median of 5 rounds) go to stderr:
 *
 *   int     four int field reads per iteration   (`aload_0; getfield` pairs:
 *                                                  the fused arm, same helper)
 *   ref     a reference field chain p.next.next  (one fused, one plain)
 *   long    two long field reads
 *
 * Expected direction: every row a few ns faster per iteration (four / two /
 * two quickened hits each). `CRATONVM_DBG_FIELD_SITE=1` must report
 * `fast-field: get hit=` in the millions, or the rows measured the slow
 * handler instead.
 *
 * stdout is deterministic and identical on HotSpot 25 (`-Xint` or not):
 *
 *   int 2000000000
 *   ref 1000000
 *   long 3000000000000
 */
public class L7W22FieldReadBench {
    static final int N = 1_000_000;

    static final class Cell {
        int a = 100;
        int b = 200;
        int c = 300;
        int d = 1400;
        long x = 1_000_000L;
        long y = 2_000_000L;
        Cell next;
    }

    static long ints(Cell p) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += p.a + p.b + p.c + p.d;
        }
        return sum;
    }

    static long refs(Cell p) {
        long n = 0;
        for (int i = 0; i < N; i++) {
            if (p.next.next == p) {
                n++;
            }
        }
        return n;
    }

    static long longs(Cell p) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += p.x + p.y;
        }
        return sum;
    }

    interface Round {
        long run();
    }

    static long timed(String label, Round r) {
        long[] ns = new long[5];
        long result = 0;
        for (int k = 0; k < 5; k++) {
            long t0 = System.nanoTime();
            result = r.run();
            ns[k] = System.nanoTime() - t0;
        }
        java.util.Arrays.sort(ns);
        System.err.printf(java.util.Locale.ROOT, "%-5s %6.1f ns/iter%n", label, ns[2] / (double) N);
        return result;
    }

    public static void main(String[] args) {
        Cell c = new Cell();
        Cell d = new Cell();
        c.next = d;
        d.next = c;
        System.out.println("int " + timed("int", () -> ints(c)));
        System.out.println("ref " + timed("ref", () -> refs(c)));
        System.out.println("long " + timed("long", () -> longs(c)));
    }
}
