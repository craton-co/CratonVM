import java.util.Arrays;

/**
 * NttShapeProbe with the one structural difference the first cut missed: `ntt` is
 * a private INSTANCE method reached through a public wrapper, not a static one.
 * Self-checking the same way NttRealProbe is — identical input every iteration, so
 * every iteration must return an identical result, and iteration 0 defines it.
 *
 * usage: NttShape2Probe [iterations]
 */
public class NttShape2Probe
{
    private static final int[] ZETAS = new int[256];

    static
    {
        int z = 1;
        for (int i = 0; i != 256; i++)
        {
            z = (z * 1103515245 + 12345) & 0x7fffffff;
            ZETAS[i] = z % 8380417;
        }
    }

    private static int montgomeryReduce(long a)
    {
        int t = (int)(a * 58728449L);
        return (int)((a - (long)t * 8380417L) >> 32);
    }

    private void ntt(int[] a)
    {
        int k = 0, j;
        for (int len = 128; len > 0; len >>= 1)
        {
            for (int start = 0; start < 256; start = j + len)
            {
                int zeta = ZETAS[++k];
                for (j = start; j < start + len; ++j)
                {
                    int t = montgomeryReduce((long)zeta * a[j + len]);
                    a[j + len] = a[j] - t;
                    a[j] = a[j] + t;
                }
            }
        }
    }

    public void polyNtt(int[] a)
    {
        ntt(a);
    }

    private static int[] seed()
    {
        int[] a = new int[256];
        for (int i = 0; i != a.length; i++)
        {
            a[i] = (i * 7919) % 8380417;
        }
        return a;
    }

    public static void main(String[] args)
    {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 5000;
        NttShape2Probe engine = new NttShape2Probe();
        int[] expected = null;
        int diverged = 0, firstBad = -1;
        for (int i = 0; i != iterations; i++)
        {
            int[] a = seed();
            engine.polyNtt(a);
            if (i == 0) { expected = a; continue; }
            if (!Arrays.equals(a, expected))
            {
                diverged++;
                if (firstBad < 0)
                {
                    firstBad = i;
                    int at = -1;
                    for (int q = 0; q != a.length; q++)
                    {
                        if (a[q] != expected[q]) { at = q; break; }
                    }
                    System.out.println("DIVERGED first at iteration " + i
                        + ", element [" + at + "]: got=" + a[at]
                        + " expected(iteration 0)=" + expected[at]);
                }
            }
        }
        System.out.println("iterations=" + iterations + " diverged=" + diverged
            + " firstBad=" + firstBad + "  => " + (diverged == 0 ? "PASS" : "FAIL"));
    }
}
