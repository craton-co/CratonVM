public class EligibleNestedLoop {
    // Rectangular 2-D loop; both bounds are recovered from array
    // lengths (`rows`/`cols` are dummy arrays whose LENGTH — not
    // contents — encodes the loop bound; the canonical-loop recognizer
    // in this crate requires every loop bound to be a plain `iload` of
    // a local populated by `arraylength`, same idiom as every other
    // fixture in this directory — see `EligibleOffsetLoop.java`'s doc
    // comment). The lowering flattens this into one thread per (i, j)
    // pair: `i = tid / c`, `j = tid % c`. `a` must be a real
    // `rows.length * cols.length`-length array.
    public static void fill(int[] a, int[] rows, int[] cols) {
        int r = rows.length;
        int c = cols.length;
        for (int i = 0; i < r; i++) {
            for (int j = 0; j < c; j++) {
                a[i * c + j] = i * c + j;
            }
        }
    }

    // Same shape, element-wise add into `a`. Exercises the ParamLen
    // bound-resolution path for both the outer AND inner loop of a
    // nested loop with two distinct array sources.
    public static void addRows(int[] a, int[] b, int[] rows, int[] cols) {
        int r = rows.length;
        int c = cols.length;
        for (int i = 0; i < r; i++) {
            for (int j = 0; j < c; j++) {
                a[i * c + j] = a[i * c + j] + b[i * c + j];
            }
        }
    }

    // Non-canonical nested loop: the outer body contains code AFTER the
    // inner loop (`out[i] = i;`), not just the inner loop itself. The
    // 2-D lowering must reject this — it never walks that trailing
    // code, so silently dropping it would be a mis-lower.
    public static void trailingCode(int[] a, int[] out, int[] dim) {
        int n = dim.length;
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                a[i * n + j] = 0;
            }
            out[i] = i;
        }
    }

    // Triangular (non-rectangular) nested loop: the inner bound is the
    // outer induction variable itself, not an array length. Both loops
    // are individually canonical (`if_icmpge` exit, stride +1, start
    // 0), so loop recognition succeeds, but lowering's bound resolution
    // cannot prove the inner bound and must reject.
    public static void triangular(int[] a, int[] dim) {
        int n = dim.length;
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < i; j++) {
                a[i * n + j] = 0;
            }
        }
    }
}
