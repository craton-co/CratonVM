import java.io.*;
public class L4AbsPath {
    static void p(String k, Object v) { System.out.println(k + " |" + v + "|"); }
    static String shape(String s) {
        if (s == null) return "null";
        return "abs=" + s.startsWith("/") + " len=" + s.length()
             + " slashes=" + s.replaceAll("[^/]", "").length()
             + " tail=" + s.substring(Math.max(0, s.lastIndexOf('/')));
    }
    public static void main(String[] a) throws Exception {
        File t = File.createTempFile("l4ap", ".tmp");
        p("temp.getPath", shape(t.getPath()));
        p("temp.getAbsolutePath", shape(t.getAbsolutePath()));
        p("temp.getCanonicalPath", shape(t.getCanonicalPath()));
        p("temp.getName", t.getName());
        p("temp.isAbsolute", t.isAbsolute());
        p("temp.getParent", shape(t.getParent()));
        p("temp.toURI.scheme", t.toURI().getScheme());
        p("temp.toURI.abs", t.toURI().getPath().startsWith("/"));
        t.delete();
        File r = new File("rel-l4ap.txt");
        p("rel.getPath", shape(r.getPath()));
        p("rel.getAbsolutePath", shape(r.getAbsolutePath()));
        p("rel.isAbsolute", r.isAbsolute());
        File d = new File("/tmp", "l4ap-x.txt");
        p("two.getPath", shape(d.getPath()));
        p("two.getAbsolutePath", shape(d.getAbsolutePath()));
        System.out.println("DONE L4AbsPath");
    }
}
