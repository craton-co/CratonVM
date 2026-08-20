package org.bouncycastle.pqc.crypto.haetae;

import java.util.Arrays;

/**
 * Drives the REAL HAETAEEngine.ntt (through its public wrapper polyNtt) and needs
 * no reference implementation to detect the miscompile.
 *
 * The check is self-evident: every iteration feeds polyNtt a freshly built array
 * with IDENTICAL contents, so every iteration must produce an identical result.
 * Iteration 0 runs interpreted and defines the expected answer; any later
 * iteration that disagrees was served by a compiled body that computes something
 * else. That makes the probe independent of what the transform is supposed to be,
 * and independent of the other VM.
 *
 * usage: NttRealProbe [iterations] [2|3|5]
 */
public class NttRealProbe
{
    public static void main(String[] args)
    {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 5000;
        String which = args.length > 1 ? args[1] : "2";
        HAETAEParameters params = which.equals("5")
            ? HAETAEParameters.haetae5
            : which.equals("3") ? HAETAEParameters.haetae3 : HAETAEParameters.haetae2;

        HAETAEEngine engine = new HAETAEEngine(params);

        int[] expected = null;
        int firstBad = -1;
        int diverged = 0;
        for (int i = 0; i != iterations; i++)
        {
            int[] a = seed();
            engine.polyNtt(a);
            if (i == 0)
            {
                expected = a;
                continue;
            }
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
        System.out.println("iterations=" + iterations
            + " diverged=" + diverged
            + " firstBad=" + firstBad
            + "  => " + (diverged == 0 ? "PASS" : "FAIL"));
    }

    private static int[] seed()
    {
        int[] a = new int[HAETAEParameters.N];
        for (int i = 0; i != a.length; i++)
        {
            a[i] = (i * 7919) % 8380417;
        }
        return a;
    }
}
