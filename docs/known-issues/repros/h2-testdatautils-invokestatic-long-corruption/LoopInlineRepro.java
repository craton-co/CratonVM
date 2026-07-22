public class LoopInlineRepro {
    static long parseHexLong(String x) {
        if (x.length() == 16) {
            return (Long.parseLong(x.substring(0, 8), 16) << 32) |
                    Long.parseLong(x.substring(8, 16), 16);
        }
        return Long.parseLong(x, 16);
    }

    public static void main(String[] a) {
        for (long i = -1; i != 0; i >>>= 1) {
            String x = Long.toHexString(i);
            long p1 = parseHexLong(x);
            if (p1 != i) {
                throw new AssertionError("Expected: " + i + " actual: " + p1);
            }
            x = Long.toHexString(-i);
            long p2 = parseHexLong(x);
            if (p2 != -i) {
                throw new AssertionError("Expected: " + (-i) + " actual: " + p2);
            }
        }
        System.out.println("ALL OK");
    }
}
