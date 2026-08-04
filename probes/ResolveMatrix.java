import java.nio.file.*;

public class ResolveMatrix {
    static void p(String k, Object v) { System.out.println(k + "\t" + v); }

    public static void main(String[] a) {
        Path base = Paths.get("/tmp/base");
        String bs = "\\";
        String[] others = {"a:b", "C:x", bs + "foo", "a" + bs + "b", "x", "./x", "..", "a b", "a:b/c", "/abs"};
        for (String other : others) {
            try { p("resolve(" + other + ")", base.resolve(other).toString()); }
            catch (Throwable t) { p("resolve(" + other + ")", "EX " + t.getClass().getSimpleName()); }
        }
        Path b2 = Paths.get("a" + bs + "b");
        p("bsName.toString", b2.toString());
        p("bsName.resolve(c)", b2.resolve("c").toString());
        p("bsName.getNameCount", b2.getNameCount());
        p("bsName.getFileName", b2.getFileName().toString());
        p("bsName.getParent", String.valueOf(b2.getParent()));
        p("bsName.isAbsolute", b2.isAbsolute());
        p("get(a:b).isAbsolute", Paths.get("a:b").isAbsolute());
        p("get(" + bs + "foo).isAbsolute", Paths.get(bs + "foo").isAbsolute());
        p("get(" + bs + "foo).getRoot", String.valueOf(Paths.get(bs + "foo").getRoot()));
        p("normalize(a" + bs + "b/../c)", Paths.get("a" + bs + "b/../c").normalize().toString());
        p("relativize(a:b)", Paths.get("/tmp").relativize(Paths.get("/tmp/a:b")).toString());
        p("Path.of(a:b, c)", Path.of("a:b", "c").toString());
        p("base.resolveSibling(a:b)", base.resolveSibling("a:b").toString());
    }
}
