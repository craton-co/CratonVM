public class StringRegexErrorProbe {
    static void t(String label, java.util.function.Supplier<Object> f) {
        try {
            System.out.println(label + " => " + f.get());
        } catch (Throwable e) {
            System.out.println(label + " => " + e.getClass().getName() + " msg=" + e.getMessage());
        }
    }
    public static void main(String[] a) {
        String s = "hello world!";
        t("split(\"[\")       ", () -> java.util.Arrays.toString(s.split("[")));
        t("matches(\"[\")     ", () -> s.matches("["));
        t("replaceAll(\"[\")  ", () -> s.replaceAll("[", "x"));
        t("replaceFirst(\"[\")", () -> s.replaceFirst("[", "x"));
        t("badgroup          ", () -> "abc".replaceAll("(b)", "$9"));
        t("badgroup2         ", () -> "abc".replaceFirst("(b)", "$9"));
        t("goodgroup         ", () -> "abc".replaceAll("(b)", "[$1]"));
        t("unclosed-paren    ", () -> s.matches("(a"));
        t("dangling-quant    ", () -> s.matches("*a"));
        t("bad-escape        ", () -> s.matches("\\q"));
        System.out.println("REGEX-PROBE-DONE");
    }
}
