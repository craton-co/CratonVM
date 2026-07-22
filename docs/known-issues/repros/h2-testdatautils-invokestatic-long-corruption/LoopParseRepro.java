public class LoopParseRepro {
    static long parseHexLong(String x) {
        if (x.length() == 16) {
            return (Long.parseLong(x.substring(0, 8), 16) << 32) |
                    Long.parseLong(x.substring(8, 16), 16);
        }
        return Long.parseLong(x, 16);
    }

    static void check(long expected, long actual) {
        if (expected != actual) {
            throw new AssertionError("Expected: " + expected + " actual: " + actual);
        }
    }

    public static void main(String[] a) {
        for (long i = -1; i != 0; i >>>= 1) {
            String x = Long.toHexString(i);
            check(i, parseHexLong(x));
            x = Long.toHexString(-i);
            check(-i, parseHexLong(x));
        }
        System.out.println("ALL OK");
    }
}
