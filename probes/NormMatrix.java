import java.nio.file.*;

public class NormMatrix {
    static void p(String k, Object v) { System.out.println(k + "\t" + v); }

    static void n(String s) {
        try { p("normalize(" + s + ")", Paths.get(s).normalize().toString()); }
        catch (Throwable t) { p("normalize(" + s + ")", "EX " + t.getClass().getSimpleName()); }
    }

    static void r(String b, String t) {
        try { p("relativize(" + b + "," + t + ")", Paths.get(b).relativize(Paths.get(t)).toString()); }
        catch (Throwable e) { p("relativize(" + b + "," + t + ")", "EX " + e.getClass().getSimpleName()); }
    }

    public static void main(String[] a) {
        String bs = "\\";
        n("/a/../../b"); n("/.."); n("../../a"); n("a/../../b"); n("/a/./b/..");
        n("a/./b/.."); n("a/../b"); n("/a/b"); n("C:a/../b"); n("C:/a/../../b");
        n("a" + bs + "b/../c"); n("//s/sh/a/..");
        r("/a/b", "/a/x"); r("a/b/c", "a/b"); r("/a/b", "/a/b/c/d"); r("/a", "/a");
        r("a/b", "a/b/c"); r("/a", "rel/b"); r("C:/a", "D:/b");
        r("/a" + bs + "b", "/a" + bs + "x");
    }
}
