// The value a strided loop stores is an expression over the induction variable.
// `i += 1024` does not fit in the signed byte a narrow `iinc` carries, so javac
// advances `i` with `wide iinc` -- and the single-pass arith-LICM's invariance
// mask did not decode `wide`, so it judged `i + r` loop-invariant and hoisted
// it into the pre-header.
//
// Deliberately NOT an OSR shape: `fill` is INVOKED hot, so it reaches the
// compiled tier through the ordinary invocation-count door. Run it under
// `CRATONVM_JIT_OSR=0` and the answer does not change, which is what says the
// defect is not the OSR entry path.
//
//   correct   7 1031 2055 3079 4103 5127 6151 7175
//   wrong     7    7    7    7    7    7    7    7
//
// `stride1` is the control: same shape, same expression, narrow `iinc`. It was
// always correct. Its rep count is divided down because it walks every element
// and the `--nojit` arm has to run it too.
public class StridedInvariantValue {
    static final int N = 8192;

    static void fill(int n, int[] a, int r) {
        for (int i = 0; i < n; i += 1024) a[i] = i + r;
    }

    static void fillStride1(int n, int[] a, int r) {
        for (int i = 0; i < n; i += 1) a[i] = i + r;
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 200000;

        int[] a = new int[N];
        for (int k = 0; k < reps; k++) fill(N, a, k & 7);
        StringBuilder s = new StringBuilder();
        for (int j = 0; j < 8; j++) s.append(a[j * 1024]).append(' ');
        System.out.println("stride1024 " + s);

        int[] b = new int[N];
        // Rounded down to a multiple of 8 so the last `r` is 7 in both arms
        // and the two lines are directly comparable.
        int reps1 = Math.max(8, (reps / 128) & ~7);
        for (int k = 0; k < reps1; k++) fillStride1(N, b, k & 7);
        StringBuilder t = new StringBuilder();
        for (int j = 0; j < 8; j++) t.append(b[j * 1024]).append(' ');
        System.out.println("stride1    " + t);
    }
}
