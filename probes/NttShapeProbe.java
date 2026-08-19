/**
 * Minimal repro of the loop shape in BouncyCastle's HAETAEEngine.ntt, which the
 * JIT miscompiles (bug-bcjava-pqc-53class-20260818.md).
 *
 * The shape that matters: `j` is declared OUTSIDE both loops, the INNER loop is
 * what advances it, and the MIDDLE loop's update expression reads it back
 * (`start = j + len`). So `j` is live across the inner loop's exit and feeds the
 * enclosing loop's induction — a compiler that treats the inner induction
 * variable as dead at the inner loop's exit, or keeps a stale register copy of
 * it, produces a different traversal without ever reading out of bounds.
 *
 * Self-checking and VM-independent: it computes the same transform twice, once
 * through the suspect shape and once through a rewrite that keeps `j` local, and
 * compares. Two runs of the same arithmetic must agree on any correct VM.
 *
 * usage: NttShapeProbe [iterations]   (needs enough iterations to tier up)
 */
public class NttShapeProbe
{
    private static final int N = 256;
    private static final int[] ZETAS = new int[N];

    static
    {
        int z = 1;
        for (int i = 0; i != N; i++)
        {
            z = (z * 1103515245 + 12345) & 0x7fffffff;
            ZETAS[i] = z % 8380417;
        }
    }

    /** The HAETAEEngine.ntt shape, verbatim in structure. */
    private static void suspect(int[] a)
    {
        int k = 0, j;
        for (int len = 128; len > 0; len >>= 1)
        {
            for (int start = 0; start < N; start = j + len)
            {
                int zeta = ZETAS[++k];
                for (j = start; j < start + len; ++j)
                {
                    int t = reduce((long)zeta * a[j + len]);
                    a[j + len] = a[j] - t;
                    a[j] = a[j] + t;
                }
            }
        }
    }

    /** Same arithmetic, but `j` cannot outlive the inner loop. */
    private static void reference(int[] a)
    {
        int k = 0;
        for (int len = 128; len > 0; len >>= 1)
        {
            for (int start = 0; start < N; start += (len << 1))
            {
                int zeta = ZETAS[++k];
                for (int j = start; j < start + len; ++j)
                {
                    int t = reduce((long)zeta * a[j + len]);
                    a[j + len] = a[j] - t;
                    a[j] = a[j] + t;
                }
            }
        }
    }

    private static int reduce(long a)
    {
        int t = (int)(a * 58728449L);
        return (int)((a - (long)t * 8380417L) >> 32);
    }

    private static int[] seed()
    {
        int[] a = new int[N];
        for (int i = 0; i != N; i++)
        {
            a[i] = (i * 7919) % 8380417;
        }
        return a;
    }

    public static void main(String[] args)
    {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int firstBad = -1;
        int bad = 0;
        for (int i = 0; i != iterations; i++)
        {
            int[] x = seed();
            int[] y = seed();
            suspect(x);
            reference(y);
            if (!java.util.Arrays.equals(x, y))
            {
                bad++;
                if (firstBad < 0)
                {
                    firstBad = i;
                    int at = -1;
                    for (int q = 0; q != N; q++)
                    {
                        if (x[q] != y[q]) { at = q; break; }
                    }
                    System.out.println("DIVERGED at iteration " + i
                        + ", first differing element [" + at + "]: suspect=" + x[at]
                        + " reference=" + y[at]);
                }
            }
        }
        System.out.println("iterations=" + iterations + " diverged=" + bad
            + " firstBad=" + firstBad
            + "  => " + (bad == 0 ? "PASS" : "FAIL"));
    }
}
