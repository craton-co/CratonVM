/*
 * Interpreter round i1, wave 25, lane L7: the `iload{_N, n}; <x>aload`
 * superinstruction -- an element read whose ARRAY is already on the operand
 * stack (proposal `i1-L1-proposal-superinstruction-coverage`, chosen from the
 * pair census `tools/interp-pair-census/PairCensus.java`, where
 * `aaload -> iload`, `iload -> aaload` and `iload -> iaload` are the top
 * three pairs of CratonBench's matrix kernel, 33% of its dispatched pairs).
 *
 *   matrix   `c += a[i][k] * b[k][j]` with i, j, k past local 3: the inner
 *            `aaload; iload k; iaload` and `aaload; iload j; iaload` fuse
 *   field    `s += this.a[i]`: `getfield a; iload_2; iaload` fuses
 *   wide     `s += a[i]` with `i` in local 5: `aload_0; iload 5; iaload`
 *            (the wave-24 group needs `i` in locals 0-3)
 *   control  the `field` loop adding `i` instead of an element -- must not move
 *
 * Run: CratonVM `--nojit` (the interpreter), default mode. A/B against the
 * wave-24 build, interleaved; ns per element (median of 5 rounds) go to
 * stderr. Expected direction: `matrix`, `field`, `wide` faster (one dispatch
 * and a push/pop pair fewer per element read), `control` flat.
 * `CRATONVM_JIT_NO_FIELD_FAST_PATH=1` turns the quickened array reads (and
 * with them this fusion) off, for a same-build control.
 *
 * stdout is deterministic and identical on HotSpot 25:
 *
 *   checksum matrix=16016000 field=49995000 wide=49995000 control=49995000
 */
public class L7W25StackedElementBench {
    static final int N = 10_000;
    static final int M = 40;

    final int[] a;

    L7W25StackedElementBench(int[] a) {
        this.a = a;
    }

    /** `i`, `j`, `k` land in locals 4, 5, 6: `a`, `b`, `n`, `c` come first. */
    static int matrix(int[][] a, int[][] b, int n) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                for (int k = 0; k < n; k++) {
                    c += a[i][k] * b[k][j];
                }
            }
        }
        return c;
    }

    int field() {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    int control() {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s += i;
        }
        return s;
    }

    /** `i` in local 5 (`a`, `x`, `y`, `z`, `s` come first). */
    static int wide(int[] a, int x, int y, int z) {
        int s = x + y + z;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    interface Row {
        long run();
    }

    static long time(String name, Row row, int reps, long elements) {
        long[] ns = new long[5];
        long check = 0;
        for (int round = 0; round < 5; round++) {
            long t0 = System.nanoTime();
            for (int r = 0; r < reps; r++) {
                check = row.run();
            }
            ns[round] = System.nanoTime() - t0;
        }
        java.util.Arrays.sort(ns);
        System.err.printf("%-8s %8.2f ns/element%n", name, ns[2] / (double) reps / elements);
        return check;
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int[] ints = new int[N];
        for (int i = 0; i < N; i++) {
            ints[i] = i;
        }
        int[][] ma = new int[M][M];
        int[][] mb = new int[M][M];
        for (int i = 0; i < M; i++) {
            for (int j = 0; j < M; j++) {
                ma[i][j] = i + j;
                mb[i][j] = i - j + 3;
            }
        }
        L7W25StackedElementBench self = new L7W25StackedElementBench(ints);
        long cm = time("matrix", () -> matrix(ma, mb, M), reps, (long) M * M * M);
        long cf = time("field", self::field, reps, N);
        long cw = time("wide", () -> wide(ints, 0, 0, 0), reps, N);
        long cn = time("control", self::control, reps, N);
        System.out.println("checksum matrix=" + cm + " field=" + cf + " wide=" + cw + " control=" + cn);
    }
}
