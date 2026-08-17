import java.util.Random;

/**
 * Measures CratonVM's integer-exponent fast path in Math.pow against a HotSpot
 * oracle. The native takes `b.fract()==0 && |b|<64` off to `powi` (repeated
 * multiplication) instead of libm `powf`; this probe is what says whether that
 * shortcut costs accuracy, and on which side of zero.
 */
public class PowIntExpProbe {
    public static void main(String[] a) {
        StringBuilder sb = new StringBuilder();
        Random r = new Random(20260816L);
        double[] bases = new double[400];
        for (int i = 0; i < bases.length; i++) {
            switch (i % 4) {
                case 0: bases[i] = r.nextDouble() * 2 - 1; break;
                case 1: bases[i] = r.nextDouble() * 200 - 100; break;
                case 2: bases[i] = Math.exp(r.nextDouble() * 60 - 30); break;
                default: bases[i] = -Math.exp(r.nextDouble() * 60 - 30); break;
            }
        }
        for (double b : bases) {
            for (int e = -70; e <= 70; e++) {
                double v = Math.pow(b, (double) e);
                sb.append("pow(").append(Long.toHexString(Double.doubleToRawLongBits(b)))
                  .append(',').append(e).append(") = ")
                  .append(Long.toHexString(Double.doubleToRawLongBits(v)))
                  .append(System.lineSeparator());
            }
        }
        System.out.print(sb);
        System.out.println("POW_END");
    }
}
