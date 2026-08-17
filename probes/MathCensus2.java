import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.FileReader;
import java.io.FileWriter;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * Second differential census: the parts of java.lang.Math that MathCensus does
 * not reach — float overloads, and the exactly-specified integer/bit-level
 * operations where a divergence would be a plain bug rather than a last-ULP
 * question.
 *
 *   java MathCensus2 gen   <file>
 *   <vm>  MathCensus2 check <file>
 */
public final class MathCensus2 {

    static long state = 0x0123456789abcdefL;

    static long nextBits() {
        state ^= state << 13;
        state ^= state >>> 7;
        state ^= state << 17;
        return state;
    }

    static double genD(int mode, long r) {
        switch (mode) {
            case 0: {
                double u = (r >>> 11) * 0x1.0p-53 * 4.0;
                return (r < 0) ? -u : u;
            }
            case 1: {
                double u = (r >>> 11) * 0x1.0p-53 * 1.0e9;
                return (r < 0) ? -u : u;
            }
            case 2: {
                double m = 1.0 + (r >>> 12) * 0x1.0p-52;
                int e = (int) ((r & 0xFFF) % 621) - 320;
                double v = m * Double.longBitsToDouble(((long) (1023 + e)) << 52);
                return ((r >>> 63) != 0) ? -v : v;
            }
            case 3: // half-integers and exact ties, which is where round() differs
                return ((r >>> 40) - 0x800000) * 0.5;
            default:
                return Double.longBitsToDouble(r);
        }
    }

    static final int MODES = 5;
    static final int PER_MODE = 1200;

    static final String[] NAMES = {
        "Math.round(D)J", "Math.round(F)I", "StrictMath.round(D)J", "StrictMath.round(F)I",
        "Math.fma(DDD)D", "Math.fma(FFF)F", "StrictMath.fma(DDD)D",
        "Math.scalb(DI)D", "Math.scalb(FI)F", "StrictMath.scalb(DI)D",
        "Math.getExponent(D)I", "Math.getExponent(F)I",
        "Math.abs(F)F", "Math.signum(F)F", "Math.ulp(F)F",
        "Math.nextUp(F)F", "Math.nextDown(F)F", "Math.nextAfter(FD)F",
        "Math.copySign(FF)F", "StrictMath.copySign(FF)F",
        "Math.ceil(D)D", "Math.rint(D)D", "Math.floor(D)D",
        "Math.max(FF)F", "Math.min(FF)F",
        "Math.clamp(DDD)D", "Math.clamp(JII)I",
        "Math.floorDiv(JJ)J", "Math.floorMod(JJ)J",
        "Math.multiplyHigh(JJ)J", "Math.unsignedMultiplyHigh(JJ)J",
        "Math.toIntExact(J)I", "Math.absExact(I)I",
    };

    /** Result as a canonical string; exceptions are part of the contract here. */
    static String apply(String name, long a, long b, long c) {
        double da = Double.longBitsToDouble(a);
        double db = Double.longBitsToDouble(b);
        double dc = Double.longBitsToDouble(c);
        float fa = Float.intBitsToFloat((int) a);
        float fb = Float.intBitsToFloat((int) b);
        float fc = Float.intBitsToFloat((int) c);
        try {
            switch (name) {
                case "Math.round(D)J": return Long.toHexString(Math.round(da));
                case "Math.round(F)I": return Integer.toHexString(Math.round(fa));
                case "StrictMath.round(D)J": return Long.toHexString(StrictMath.round(da));
                case "StrictMath.round(F)I": return Integer.toHexString(StrictMath.round(fa));
                case "Math.fma(DDD)D": return d(Math.fma(da, db, dc));
                case "Math.fma(FFF)F": return f(Math.fma(fa, fb, fc));
                case "StrictMath.fma(DDD)D": return d(StrictMath.fma(da, db, dc));
                case "Math.scalb(DI)D": return d(Math.scalb(da, (int) (b % 2200) - 1100));
                case "Math.scalb(FI)F": return f(Math.scalb(fa, (int) (b % 300) - 150));
                case "StrictMath.scalb(DI)D": return d(StrictMath.scalb(da, (int) (b % 2200) - 1100));
                case "Math.getExponent(D)I": return Integer.toHexString(Math.getExponent(da));
                case "Math.getExponent(F)I": return Integer.toHexString(Math.getExponent(fa));
                case "Math.abs(F)F": return f(Math.abs(fa));
                case "Math.signum(F)F": return f(Math.signum(fa));
                case "Math.ulp(F)F": return f(Math.ulp(fa));
                case "Math.nextUp(F)F": return f(Math.nextUp(fa));
                case "Math.nextDown(F)F": return f(Math.nextDown(fa));
                case "Math.nextAfter(FD)F": return f(Math.nextAfter(fa, db));
                case "Math.copySign(FF)F": return f(Math.copySign(fa, fb));
                case "StrictMath.copySign(FF)F": return f(StrictMath.copySign(fa, fb));
                case "Math.ceil(D)D": return d(Math.ceil(da));
                case "Math.rint(D)D": return d(Math.rint(da));
                case "Math.floor(D)D": return d(Math.floor(da));
                case "Math.max(FF)F": return f(Math.max(fa, fb));
                case "Math.min(FF)F": return f(Math.min(fa, fb));
                case "Math.clamp(DDD)D": {
                    double lo = Math.min(db, dc);
                    double hi = Math.max(db, dc);
                    if (Double.isNaN(lo) || Double.isNaN(hi) || lo > hi) {
                        return "skip";
                    }
                    return d(Math.clamp(da, lo, hi));
                }
                case "Math.clamp(JII)I": {
                    int lo = (int) b;
                    int hi = (int) c;
                    if (lo > hi) {
                        return "skip";
                    }
                    return Integer.toHexString(Math.clamp(a, lo, hi));
                }
                case "Math.floorDiv(JJ)J": return Long.toHexString(Math.floorDiv(a, b));
                case "Math.floorMod(JJ)J": return Long.toHexString(Math.floorMod(a, b));
                case "Math.multiplyHigh(JJ)J": return Long.toHexString(Math.multiplyHigh(a, b));
                case "Math.unsignedMultiplyHigh(JJ)J":
                    return Long.toHexString(Math.unsignedMultiplyHigh(a, b));
                case "Math.toIntExact(J)I": return Integer.toHexString(Math.toIntExact(a));
                case "Math.absExact(I)I": return Integer.toHexString(Math.absExact((int) a));
                default: throw new IllegalArgumentException(name);
            }
        } catch (ArithmeticException e) {
            return "AE";
        }
    }

    static String d(double v) {
        return Long.toHexString(Double.doubleToRawLongBits(v));
    }

    static String f(float v) {
        return Integer.toHexString(Float.floatToRawIntBits(v));
    }

    public static void main(String[] args) throws Exception {
        if ("gen".equals(args[0])) {
            BufferedWriter w = new BufferedWriter(new FileWriter(args[1]));
            for (String name : NAMES) {
                state = 0x0123456789abcdefL;
                for (int m = 0; m < MODES; m++) {
                    for (int i = 0; i < PER_MODE; i++) {
                        long a = Double.doubleToRawLongBits(genD(m, nextBits()));
                        long b = Double.doubleToRawLongBits(genD(m, nextBits()));
                        long c = Double.doubleToRawLongBits(genD(m, nextBits()));
                        w.write(name + "\t" + Long.toHexString(a) + "\t" + Long.toHexString(b)
                                + "\t" + Long.toHexString(c) + "\t" + apply(name, a, b, c) + "\n");
                    }
                }
            }
            w.close();
            System.out.println("ORACLE2-WRITTEN " + args[1]);
            return;
        }
        BufferedReader r = new BufferedReader(new FileReader(args[1]));
        Map<String, int[]> counts = new LinkedHashMap<>();
        Map<String, StringBuilder> samples = new LinkedHashMap<>();
        String line;
        while ((line = r.readLine()) != null) {
            String[] p = line.split("\t");
            long a = Long.parseUnsignedLong(p[1], 16);
            long b = Long.parseUnsignedLong(p[2], 16);
            long c = Long.parseUnsignedLong(p[3], 16);
            String want = p.length > 4 ? p[4] : "";
            String got = apply(p[0], a, b, c);
            int[] k = counts.computeIfAbsent(p[0], x -> new int[2]);
            samples.computeIfAbsent(p[0], x -> new StringBuilder());
            k[0]++;
            if (!got.equals(want)) {
                k[1]++;
                if (k[1] <= 3) {
                    samples.get(p[0]).append("      f(").append(p[1]).append(",").append(p[2])
                            .append(",").append(p[3]).append(") got=").append(got)
                            .append(" want=").append(want).append('\n');
                }
            }
        }
        r.close();
        System.out.println("== MathCensus2: CratonVM vs HotSpot oracle ==");
        int bad = 0;
        for (Map.Entry<String, int[]> e : counts.entrySet()) {
            if (e.getValue()[1] == 0) {
                continue;
            }
            bad++;
            System.out.printf("DIFF %-30s %5d/%5d%n", e.getKey(), e.getValue()[1], e.getValue()[0]);
            System.out.print(samples.get(e.getKey()));
        }
        System.out.println("CENSUS2-DONE badFuncs=" + bad + " funcs=" + counts.size());
    }
}
