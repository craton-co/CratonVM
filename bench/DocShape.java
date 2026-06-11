// Mirrors the gap-doc repro's exact expression shape: the FastMath.sin result
// stays LIVE ON THE OPERAND STACK across the Math.sin call (the Bug-1 XMM0
// clobber shape), unlike RealFm which stores both results to locals first.
import org.apache.commons.math3.util.FastMath;

public final class DocShape {
    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 100000;
        long fails = 0;
        long total = 0;
        for (int it = 0; it < iters; it++) {
            for (int k = 1; k < 32; k++) {
                double x = k * (Math.PI / 32.0);
                total++;
                if (Math.abs(FastMath.sin(x) - Math.sin(x)) > 1e-12) {
                    fails++;
                }
            }
        }
        System.out.println("total=" + total + " fails=" + fails);
        for (int k = 1; k < 32; k++) {
            double x = k * (Math.PI / 32.0);
            if (Math.abs(FastMath.sin(x) - Math.sin(x)) > 1e-12) {
                System.out.println("MISMATCH k=" + k + " x=" + x + " fm=" + FastMath.sin(x) + " want=" + Math.sin(x));
            }
        }
        System.out.println("probe sin(3pi/4) = " + FastMath.sin(3 * Math.PI / 4) + "  want " + Math.sin(3 * Math.PI / 4));
    }
}
