import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.FileReader;
import java.io.FileWriter;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * Differential census of java.lang.Math / java.lang.StrictMath against a
 * HotSpot-generated oracle file.
 *
 *   java MathCensus gen   <file>   -- run on HotSpot, writes the oracle
 *   <vm>  MathCensus check <file>  -- run on CratonVM, reports disagreements
 *
 * Line format: name TAB argbits[ TAB argbits] TAB resultbits   (all hex).
 */
public final class MathCensus {

    /** Unary double->double functions. */
    static final String[] UNARY = {
        "Math.sin", "Math.cos", "Math.tan", "Math.asin", "Math.acos", "Math.atan",
        "Math.exp", "Math.log", "Math.log10", "Math.sqrt", "Math.cbrt",
        "Math.expm1", "Math.log1p", "Math.sinh", "Math.cosh", "Math.tanh",
        "Math.ceil", "Math.floor", "Math.rint", "Math.signum", "Math.ulp",
        "Math.toRadians", "Math.toDegrees", "Math.nextUp", "Math.nextDown", "Math.abs",
        "StrictMath.sin", "StrictMath.cos", "StrictMath.tan", "StrictMath.asin",
        "StrictMath.acos", "StrictMath.atan", "StrictMath.exp", "StrictMath.log",
        "StrictMath.log10", "StrictMath.sqrt", "StrictMath.cbrt", "StrictMath.expm1",
        "StrictMath.log1p", "StrictMath.sinh", "StrictMath.cosh", "StrictMath.tanh",
        "StrictMath.ceil", "StrictMath.floor", "StrictMath.rint",
        "StrictMath.toRadians", "StrictMath.toDegrees",
    };

    /** Binary (double,double)->double functions. */
    static final String[] BINARY = {
        "Math.pow", "Math.atan2", "Math.hypot", "Math.IEEEremainder",
        "Math.copySign", "Math.nextAfter", "Math.max", "Math.min",
        "StrictMath.pow", "StrictMath.atan2", "StrictMath.hypot",
        "StrictMath.IEEEremainder", "StrictMath.copySign",
    };

    static long state = 0x0123456789abcdefL;

    static long nextBits() {
        state ^= state << 13;
        state ^= state >>> 7;
        state ^= state << 17;
        return state;
    }

    /** Deterministic input generator; deliberately avoids calling any Math method. */
    static double gen(int mode, long r) {
        switch (mode) {
            case 0: { // uniform in [-1, 1]
                double u = (r >>> 11) * 0x1.0p-53;
                return (r < 0) ? -u : u;
            }
            case 1: { // uniform in [-100, 100]
                double u = (r >>> 11) * 0x1.0p-53 * 100.0;
                return (r < 0) ? -u : u;
            }
            case 2: { // uniform in [0, 1e6]
                return (r >>> 11) * 0x1.0p-53 * 1.0e6;
            }
            case 3: { // wide exponent range, |x| in [2^-60, 2^60]
                double m = 1.0 + (r >>> 12) * 0x1.0p-52;
                int e = (int) ((r & 0xFFF) % 121) - 60;
                double v = m * Double.longBitsToDouble(((long) (1023 + e)) << 52);
                return ((r >>> 63) != 0) ? -v : v;
            }
            case 4: { // extreme exponents, |x| in [2^-320, 2^300]
                double m = 1.0 + (r >>> 12) * 0x1.0p-52;
                int e = (int) ((r & 0xFFF) % 621) - 320;
                double v = m * Double.longBitsToDouble(((long) (1023 + e)) << 52);
                return ((r >>> 63) != 0) ? -v : v;
            }
            default: { // raw bit patterns (includes NaN / Inf / subnormals)
                return Double.longBitsToDouble(r);
            }
        }
    }

    static final int MODES = 6;
    static final int PER_MODE = 900;

    static double apply1(String name, double x) {
        switch (name) {
            case "Math.sin": return Math.sin(x);
            case "Math.cos": return Math.cos(x);
            case "Math.tan": return Math.tan(x);
            case "Math.asin": return Math.asin(x);
            case "Math.acos": return Math.acos(x);
            case "Math.atan": return Math.atan(x);
            case "Math.exp": return Math.exp(x);
            case "Math.log": return Math.log(x);
            case "Math.log10": return Math.log10(x);
            case "Math.sqrt": return Math.sqrt(x);
            case "Math.cbrt": return Math.cbrt(x);
            case "Math.expm1": return Math.expm1(x);
            case "Math.log1p": return Math.log1p(x);
            case "Math.sinh": return Math.sinh(x);
            case "Math.cosh": return Math.cosh(x);
            case "Math.tanh": return Math.tanh(x);
            case "Math.ceil": return Math.ceil(x);
            case "Math.floor": return Math.floor(x);
            case "Math.rint": return Math.rint(x);
            case "Math.signum": return Math.signum(x);
            case "Math.ulp": return Math.ulp(x);
            case "Math.toRadians": return Math.toRadians(x);
            case "Math.toDegrees": return Math.toDegrees(x);
            case "Math.nextUp": return Math.nextUp(x);
            case "Math.nextDown": return Math.nextDown(x);
            case "Math.abs": return Math.abs(x);
            case "StrictMath.sin": return StrictMath.sin(x);
            case "StrictMath.cos": return StrictMath.cos(x);
            case "StrictMath.tan": return StrictMath.tan(x);
            case "StrictMath.asin": return StrictMath.asin(x);
            case "StrictMath.acos": return StrictMath.acos(x);
            case "StrictMath.atan": return StrictMath.atan(x);
            case "StrictMath.exp": return StrictMath.exp(x);
            case "StrictMath.log": return StrictMath.log(x);
            case "StrictMath.log10": return StrictMath.log10(x);
            case "StrictMath.sqrt": return StrictMath.sqrt(x);
            case "StrictMath.cbrt": return StrictMath.cbrt(x);
            case "StrictMath.expm1": return StrictMath.expm1(x);
            case "StrictMath.log1p": return StrictMath.log1p(x);
            case "StrictMath.sinh": return StrictMath.sinh(x);
            case "StrictMath.cosh": return StrictMath.cosh(x);
            case "StrictMath.tanh": return StrictMath.tanh(x);
            case "StrictMath.ceil": return StrictMath.ceil(x);
            case "StrictMath.floor": return StrictMath.floor(x);
            case "StrictMath.rint": return StrictMath.rint(x);
            case "StrictMath.toRadians": return StrictMath.toRadians(x);
            case "StrictMath.toDegrees": return StrictMath.toDegrees(x);
            default: throw new IllegalArgumentException(name);
        }
    }

    static double apply2(String name, double x, double y) {
        switch (name) {
            case "Math.pow": return Math.pow(x, y);
            case "Math.atan2": return Math.atan2(x, y);
            case "Math.hypot": return Math.hypot(x, y);
            case "Math.IEEEremainder": return Math.IEEEremainder(x, y);
            case "Math.copySign": return Math.copySign(x, y);
            case "Math.nextAfter": return Math.nextAfter(x, y);
            case "Math.max": return Math.max(x, y);
            case "Math.min": return Math.min(x, y);
            case "StrictMath.pow": return StrictMath.pow(x, y);
            case "StrictMath.atan2": return StrictMath.atan2(x, y);
            case "StrictMath.hypot": return StrictMath.hypot(x, y);
            case "StrictMath.IEEEremainder": return StrictMath.IEEEremainder(x, y);
            case "StrictMath.copySign": return StrictMath.copySign(x, y);
            default: throw new IllegalArgumentException(name);
        }
    }

    static String h(double v) {
        return Long.toHexString(Double.doubleToRawLongBits(v));
    }

    public static void main(String[] args) throws Exception {
        String mode = args[0];
        String file = args[1];
        if ("gen".equals(mode)) {
            gen(file);
        } else {
            check(file);
        }
    }

    static void gen(String file) throws Exception {
        BufferedWriter w = new BufferedWriter(new FileWriter(file));
        for (String name : UNARY) {
            state = 0x0123456789abcdefL;
            for (int m = 0; m < MODES; m++) {
                for (int i = 0; i < PER_MODE; i++) {
                    double x = gen(m, nextBits());
                    w.write(name);
                    w.write('\t');
                    w.write(h(x));
                    w.write('\t');
                    w.write(h(apply1(name, x)));
                    w.write('\n');
                }
            }
        }
        for (String name : BINARY) {
            state = 0x0123456789abcdefL;
            for (int m = 0; m < MODES; m++) {
                for (int i = 0; i < PER_MODE; i++) {
                    double x = gen(m, nextBits());
                    double y = gen(m, nextBits());
                    w.write(name);
                    w.write('\t');
                    w.write(h(x));
                    w.write('\t');
                    w.write(h(y));
                    w.write('\t');
                    w.write(h(apply2(name, x, y)));
                    w.write('\n');
                }
            }
        }
        w.close();
        System.out.println("ORACLE-WRITTEN " + file);
    }

    static long ulpDelta(long a, long b) {
        // Monotone ordering of doubles by bits.
        long ka = a < 0 ? Long.MIN_VALUE - a : a;
        long kb = b < 0 ? Long.MIN_VALUE - b : b;
        long d = ka - kb;
        return d < 0 ? -d : d;
    }

    static void check(String file) throws Exception {
        BufferedReader r = new BufferedReader(new FileReader(file));
        Map<String, int[]> counts = new LinkedHashMap<>();   // name -> {total, bad}
        Map<String, Long> maxUlp = new LinkedHashMap<>();
        Map<String, StringBuilder> samples = new LinkedHashMap<>();
        String line;
        while ((line = r.readLine()) != null) {
            String[] f = line.split("\t");
            String name = f[0];
            double got;
            String argDesc;
            long expBits;
            if (f.length == 3) {
                double x = Double.longBitsToDouble(Long.parseUnsignedLong(f[1], 16));
                expBits = Long.parseUnsignedLong(f[2], 16);
                got = apply1(name, x);
                argDesc = f[1];
            } else {
                double x = Double.longBitsToDouble(Long.parseUnsignedLong(f[1], 16));
                double y = Double.longBitsToDouble(Long.parseUnsignedLong(f[2], 16));
                expBits = Long.parseUnsignedLong(f[3], 16);
                got = apply2(name, x, y);
                argDesc = f[1] + "," + f[2];
            }
            long gotBits = Double.doubleToRawLongBits(got);
            int[] c = counts.get(name);
            if (c == null) {
                c = new int[2];
                counts.put(name, c);
                maxUlp.put(name, 0L);
                samples.put(name, new StringBuilder());
            }
            c[0]++;
            boolean bothNaN = Double.isNaN(got) && Double.isNaN(Double.longBitsToDouble(expBits));
            if (gotBits != expBits && !bothNaN) {
                c[1]++;
                long d = ulpDelta(gotBits, expBits);
                if (d > maxUlp.get(name)) {
                    maxUlp.put(name, d);
                }
                StringBuilder sb = samples.get(name);
                if (c[1] <= 3) {
                    sb.append("      e.g. f(").append(argDesc).append(") got=")
                      .append(Long.toHexString(gotBits)).append(" want=")
                      .append(Long.toHexString(expBits)).append(" ulp=").append(d).append('\n');
                }
            }
        }
        r.close();
        System.out.println("== MathCensus: CratonVM vs HotSpot oracle ==");
        int badFuncs = 0;
        for (Map.Entry<String, int[]> e : counts.entrySet()) {
            int[] c = e.getValue();
            if (c[1] == 0) {
                continue;
            }
            badFuncs++;
            System.out.printf("DIFF %-26s %5d/%5d (%.2f%%) maxUlp=%d%n",
                    e.getKey(), c[1], c[0], 100.0 * c[1] / c[0], maxUlp.get(e.getKey()));
            System.out.print(samples.get(e.getKey()));
        }
        System.out.println("-- clean:");
        StringBuilder ok = new StringBuilder();
        for (Map.Entry<String, int[]> e : counts.entrySet()) {
            if (e.getValue()[1] == 0) {
                ok.append(e.getKey()).append(' ');
            }
        }
        System.out.println("   " + ok);
        System.out.println("CENSUS-DONE badFuncs=" + badFuncs + " funcs=" + counts.size());
    }
}
