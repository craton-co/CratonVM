package cratonvm;

/**
 * Differential testing helper for String operations.
 * Each static method prints its result to stdout so that the differential
 * harness can compare CratonVM vs HotSpot output line-by-line.
 */
public class DiffString {
    public static void main(String[] args) {
        System.out.println(length());
        System.out.println(concat());
        System.out.println(charAt());
        System.out.println(substring());
        System.out.println(indexOf());
        System.out.println(toUpperCase());
        System.out.println(trim());
        System.out.println(equals());
        System.out.println(valueOf());
    }

    public static int length() { return "hello".length(); }
    public static String concat() { return "hello" + " " + "world"; }
    public static char charAt() { return "abcdef".charAt(3); }
    public static String substring() { return "hello world".substring(6); }
    public static int indexOf() { return "abcabc".indexOf("bc"); }
    public static String toUpperCase() { return "hello".toUpperCase(); }
    public static String trim() { return "  hello  ".trim(); }
    public static boolean equals() { return "abc".equals("abc"); }
    public static String valueOf() { return String.valueOf(42); }
}
