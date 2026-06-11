// Ground-truth driver: exercises the REAL commons-math3 FastMath.sin from the
// jar. Run with CRATONVM_JIT_ALLOW_PACKAGES=org/apache/commons/ to lift the
// skip-list ban so the JIT compiles the real (old-javac) FastMath bytecode.
import org.apache.commons.math3.util.FastMath;

public final class RealFm {
    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 100000;

        long fails = 0;
        long total = 0;
        for (int it = 0; it < iters; it++) {
            for (int k = 1; k < 64; k++) {
                double x = k * (Math.PI / 32.0);
                double got = FastMath.sin(x);
                double want = Math.sin(x);
                total++;
                if (Math.abs(got - want) > 1e-12) {
                    fails++;
                }
            }
        }
        System.out.println("total=" + total + " fails=" + fails);

        int printed = 0;
        for (int k = 1; k < 64 && printed < 10; k++) {
            double x = k * (Math.PI / 32.0);
            double got = FastMath.sin(x);
            double want = Math.sin(x);
            if (Math.abs(got - want) > 1e-12) {
                System.out.println("MISMATCH k=" + k + " x=" + x + " got=" + got + " want=" + want + " ratio=" + (got / want));
                printed++;
            }
        }
        System.out.println("sin(3pi/4) = " + FastMath.sin(3 * Math.PI / 4) + "  want " + Math.sin(3 * Math.PI / 4));
    }
}
