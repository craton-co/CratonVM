public class LongArgCallRepro {
    static void check(long expected, long actual) {
        if (expected != actual) {
            throw new AssertionError("Expected: " + expected + " actual: " + actual);
        }
        System.out.println("OK expected=" + expected + " actual=" + actual);
    }

    public static void main(String[] a) {
        long i = 1125899906842623L;
        long negI = -i;
        check(negI, negI);
        check(-1125899906842623L, negI);
        // Mimic parseHexLong: recompute from the hex string like DataUtils does.
        String hex = Long.toHexString(negI);
        long parsed = parseHexLong(hex);
        System.out.println("parsed=" + parsed + " hex=" + hex);
        check(negI, parsed);
    }

    // Minimal stand-in for org.h2.mvstore.DataUtils.parseHexLong
    static long parseHexLong(String x) {
        return Long.parseUnsignedLong(x, 16);
    }
}
