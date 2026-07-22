public class ShiftOrLongRepro {
    static long parseHexLong(String x) {
        if (x.length() == 16) {
            return (Long.parseLong(x.substring(0, 8), 16) << 32) |
                    Long.parseLong(x.substring(8, 16), 16);
        }
        return Long.parseLong(x, 16);
    }

    public static void main(String[] a) {
        long i = 1125899906842623L;
        long negI = -i;
        String hex = Long.toHexString(negI);
        System.out.println("hex=" + hex + " len=" + hex.length());
        long parsed = parseHexLong(hex);
        System.out.println("parsed=" + parsed);
        System.out.println("parsed==negI: " + (parsed == negI));
        if (parsed != negI) {
            throw new AssertionError("Expected: " + negI + " actual: " + parsed);
        }
    }
}
