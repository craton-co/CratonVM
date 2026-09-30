/*
 * Interpreter round i1, wave 24, lane L7: the `aload_N; iload_M; <x>aload`
 * superinstruction (javac's `a[i]` with both in locals 0-3; proposal
 * `i1-L1-proposal-superinstruction-coverage`).
 *
 * Every row is a javac array loop whose body reads `a[i]` once; only the
 * element type changes. `a` is local 0 and `i` local 2 in each method (3 in
 * `sumLong`, whose `long s` takes two slots), so the body's
 * `aload_0; iload_{2,3}; <x>aload` is the fused group (`javap -c` shows it):
 *
 *   int      iaload       sum of an int[]
 *   long     laload       sum of a long[]
 *   byte     baload       sum of a byte[]
 *   char     caload       sum of a char[]
 *   ref      aaload       count of non-null elements of an Object[]
 *   control  (none)       the same loop over `a.length` adding `i`, no
 *                         element read -- must not move
 *
 * Run: CratonVM `--nojit` (the interpreter; with the JIT the loops compile),
 * default mode. A/B against the wave-23 build, interleaved; ns per element
 * (median of 5 rounds) go to stderr. Expected direction: the five element
 * rows faster (two of the three dispatches of each element read and a push
 * and pop pair are gone), `control` flat. Setting
 * `CRATONVM_JIT_NO_FIELD_FAST_PATH=1` (no quickened array reads at all)
 * turns the fusion off with the arm it reuses, for a same-build control.
 *
 * stdout is deterministic and identical on HotSpot 25 (`-Xint` or not):
 *
 *   checksum int=49995000 long=49995000 byte=-4872 char=29994 ref=5000 control=49995000
 */
public class L7W24ArrayElementBench {
    static final int N = 10_000;

    static int sumInt(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static long sumLong(long[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static int sumByte(byte[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static int sumChar(char[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static int countRef(Object[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            if (a[i] != null) {
                s++;
            }
        }
        return s;
    }

    static int control(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s += i;
        }
        return s;
    }

    interface Row {
        long run();
    }

    static long time(String name, Row row, int reps) {
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
        System.err.printf("%-8s %8.2f ns/element%n", name, ns[2] / (double) reps / N);
        return check;
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int[] ints = new int[N];
        long[] longs = new long[N];
        byte[] bytes = new byte[N];
        char[] chars = new char[N];
        Object[] refs = new Object[N];
        for (int i = 0; i < N; i++) {
            ints[i] = i;
            longs[i] = i;
            bytes[i] = (byte) i;
            chars[i] = (char) (i % 7);
            refs[i] = (i % 2 == 0) ? "x" : null;
        }
        long ci = time("int", () -> sumInt(ints), reps);
        long cl = time("long", () -> sumLong(longs), reps);
        long cb = time("byte", () -> sumByte(bytes), reps);
        long cc = time("char", () -> sumChar(chars), reps);
        long cr = time("ref", () -> countRef(refs), reps);
        long cn = time("control", () -> control(ints), reps);
        System.out.println("checksum int=" + ci + " long=" + cl + " byte=" + cb + " char=" + cc
                + " ref=" + cr + " control=" + cn);
    }
}
