// Cause 2, minimised: no calls at all. Two nested loops; the inner one
// starts at the OUTER induction variable.
//
// `varStart` samples the indices `i = r` actually writes (11 + k*1024 on
// the last round); `zeroStart` samples the ones `i = 0` writes (k*1024).
// Sampling the wrong set is how the first attempt at this comparison
// reported a vacuous "same" against a row of untouched zeros.
public class OsrStridedValueMin {
    static void varStart(int n, int[] a) {
        for (int i = 0; i < n; i++) a[i] = 0;
        for (int r = 0; r < 12; r++)
            for (int i = r; i < n; i += 1024) a[i] = i + r;
    }

    static void zeroStart(int n, int[] a) {
        for (int i = 0; i < n; i++) a[i] = 0;
        for (int r = 0; r < 12; r++)
            for (int i = 0; i < n; i += 1024) a[i] = i + r;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 65536;
        int[] a = new int[n];
        varStart(n, a);
        StringBuilder s = new StringBuilder();
        for (int k = 0; k < 5; k++) s.append(a[11 + k * 1024]).append(' ');
        System.out.println("varStart  " + s);

        int[] b = new int[n];
        zeroStart(n, b);
        StringBuilder t = new StringBuilder();
        for (int k = 0; k < 5; k++) t.append(b[k * 1024]).append(' ');
        System.out.println("zeroStart " + t);
    }
}
