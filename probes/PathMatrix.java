import java.nio.file.*;

public class PathMatrix {
    static void p(String k, Object v) { System.out.println(k + "\t" + v); }

    static void probe(String s) {
        try {
            Path x = Paths.get(s);
            p("get(" + s + ").toString", x.toString());
            p("get(" + s + ").nameCount", x.getNameCount());
            p("get(" + s + ").root", x.getRoot());
            p("get(" + s + ").parent", x.getParent());
            p("get(" + s + ").fileName", x.getFileName());
            p("get(" + s + ").normalize", x.normalize());
            p("get(" + s + ").isAbsolute", x.isAbsolute());
        } catch (Throwable t) {
            p("get(" + s + ")", "EX " + t.getClass().getName());
        }
    }

    static void resolveProbe(String base, String other) {
        try {
            Path r = Paths.get(base).resolve(other);
            p("res(" + base + "|" + other + ")", r.toString() + " nc=" + r.getNameCount()
                + " parent=" + r.getParent() + " eqTrim=" + r.equals(Paths.get(base).resolve(other.replaceAll("/+$", ""))));
        } catch (Throwable t) {
            p("res(" + base + "|" + other + ")", "EX " + t.getClass().getName());
        }
    }

    public static void main(String[] a) throws Exception {
        for (String s : new String[]{"a/b/", "a/b//", "/tmp/", "/", "//", "a//b", "//tmp/x", "", "a/", "/a/b/c///"}) probe(s);
        for (String[] rp : new String[][]{{"/tmp","one/two/three/"},{"/tmp","one//two"},{"/tmp","/abs/"},{"/tmp/","x"},{"a","b/"}}) resolveProbe(rp[0], rp[1]);
        p("startsWith", Paths.get("/a/b/").startsWith(Paths.get("/a")));
        p("endsWith", Paths.get("/a/b/").endsWith(Paths.get("b")));
        p("relativize", Paths.get("/a").relativize(Paths.get("/a/b/")));
        p("subpath", Paths.get("/a/b/c/").subpath(0,2));
        p("iterLast", Paths.get("/a/b/c/").iterator().next());
        p("compare", Paths.get("/a/b/").compareTo(Paths.get("/a/b")));
        p("uri", Paths.get("/tmp/x/").toUri());
        p("toFile", Paths.get("/tmp/x/").toFile().getPath());
        p("ofFile", new java.io.File("/tmp/x/").toPath().toString());
    }
}
