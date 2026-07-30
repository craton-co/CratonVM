/**
 * Correctness probe for the compiled StringConcatFactory bridge.
 *
 * Every shape below is a `makeConcatWithConstants` call site reached from a hot
 * loop, so the bridge (not the interpreter) produces the string once the method
 * is compiled. A checksum over every produced string is printed; it must match
 * HotSpot byte for byte. Category-2 values (long/double) and null references are
 * included deliberately: the bridge decodes raw i64 slots using the call site's
 * descriptor, so a mis-typed slot would silently corrupt exactly those.
 */
public final class ConcatBridgeProbe {

    private static long checksum(String s) {
        long h = 1469598103934665603L;
        for (int i = 0; i < s.length(); i++) {
            h ^= s.charAt(i);
            h *= 1099511628211L;
        }
        return h;
    }

    public static void main(String[] args) {
        int iterations = args.length == 0 ? 200_000 : Integer.parseInt(args[0]);
        long acc = 0;
        String sample = null;

        for (int i = 0; i < iterations; i++) {
            int iv = i;
            long lv = ((long) i << 33) ^ i;          // category-2, needs full 64 bits
            double dv = i * 0.5d;                    // category-2 fp
            float fv = i * 0.25f;                    // category-1 fp
            char cv = (char) ('a' + (i & 15));
            boolean bv = (i & 1) == 0;
            byte yv = (byte) i;
            short sv = (short) i;
            String rv = (i & 7) == 0 ? null : "s" + (i & 7);
            Object ov = (i & 15) == 0 ? null : Integer.valueOf(i & 15);

            acc += checksum("i=" + iv);
            acc += checksum("l=" + lv);
            acc += checksum("d=" + dv);
            acc += checksum("f=" + fv);
            acc += checksum("c=" + cv);
            acc += checksum("b=" + bv);
            acc += checksum("y=" + yv);
            acc += checksum("h=" + sv);
            acc += checksum("r=" + rv);
            acc += checksum("o=" + ov);
            // mixed arity / interleaved category-2 in one site
            acc += checksum("m=" + iv + ":" + lv + ":" + dv + ":" + rv);
            acc += checksum(iv + "-" + fv + "-" + cv + "-" + bv);
            // no-constant form and leading/trailing constants
            acc += checksum("" + lv);
            acc += checksum("[" + iv + "]");

            if (i == iterations - 1) {
                sample = "m=" + iv + ":" + lv + ":" + dv + ":" + rv;
            }
        }

        System.out.println("iterations=" + iterations);
        System.out.println("acc=" + acc);
        System.out.println("sample=" + sample);
    }
}
