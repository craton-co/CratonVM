import java.util.Random;

/**
 * java.util.Random fidelity probe.
 *
 * `new Random(seed)` is a fully specified LCG (JLS / Random javadoc), so every
 * derived sequence is byte-reproducible across conforming VMs. Any divergence
 * here is a VM defect, and it silently rewrites the input distribution of every
 * seeded test — which is how it surfaced: H2's TestMemoryEstimator feeds
 * `100 + nextGaussian()*30` into a statistical estimator and asserts bounds on
 * the resulting error.
 *
 * Reports each generator separately so the log names WHICH one diverges.
 *
 * Usage: RandomFidelityProbe [n] [seed]
 */
public class RandomFidelityProbe {

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 100000;
        long seed = args.length > 1 ? Long.parseLong(args[1]) : 42L;

        // --- exact first values, the sharpest diff ---
        Random r = new Random(seed);
        StringBuilder g = new StringBuilder();
        for (int i = 0; i < 6; i++) {
            g.append(Double.doubleToRawLongBits(r.nextGaussian())).append(' ');
        }
        System.out.println("gaussianBits6=" + g.toString().trim());

        r = new Random(seed);
        StringBuilder ni = new StringBuilder();
        for (int i = 0; i < 8; i++) {
            ni.append(r.nextInt(48)).append(' ');
        }
        System.out.println("nextInt48x8=" + ni.toString().trim());

        r = new Random(seed);
        StringBuilder nd = new StringBuilder();
        for (int i = 0; i < 4; i++) {
            nd.append(Double.doubleToRawLongBits(r.nextDouble())).append(' ');
        }
        System.out.println("nextDoubleBits4=" + nd.toString().trim());

        r = new Random(seed);
        StringBuilder nl = new StringBuilder();
        for (int i = 0; i < 4; i++) {
            nl.append(r.nextLong()).append(' ');
        }
        System.out.println("nextLong4=" + nl.toString().trim());

        // --- distribution shape: mean/stddev of nextGaussian over n draws ---
        r = new Random(seed);
        double sum = 0;
        double sumSq = 0;
        double min = Double.MAX_VALUE;
        double max = -Double.MAX_VALUE;
        for (int i = 0; i < n; i++) {
            double v = r.nextGaussian();
            sum += v;
            sumSq += v * v;
            if (v < min) min = v;
            if (v > max) max = v;
        }
        double mean = sum / n;
        double sd = Math.sqrt(sumSq / n - mean * mean);
        System.out.printf("gaussian n=%d mean=%.6f sd=%.6f min=%.4f max=%.4f%n", n, mean, sd, min, max);

        // The exact expression H2's TestMemoryEstimator feeds its estimator.
        r = new Random(seed);
        long xs = 0;
        long xs2 = 0;
        for (int i = 0; i < n; i++) {
            int x = (int) Math.abs(100 + r.nextGaussian() * 30);
            xs += x;
            xs2 += (long) x * x;
        }
        double xmean = 1.0 * xs / n;
        double xsd = Math.sqrt(1.0 * xs2 / n - xmean * xmean);
        System.out.printf("h2input n=%d mean=%.4f sd=%.4f%n", n, xmean, xsd);

        // StrictMath primitives nextGaussian is built on, in case the divergence
        // is in the math rather than the generator.
        System.out.println("sqrtBits=" + Double.doubleToRawLongBits(StrictMath.sqrt(2.0))
                + " logBits=" + Double.doubleToRawLongBits(StrictMath.log(0.5)));
    }
}
