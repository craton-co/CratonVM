import java.nio.file.*;

// Deep conformance sweep of java.nio.file.Path ops vs HotSpot.
// Prints one line per case so HotSpot and CratonVM output can be diffed directly.
public class PathDeep {
  static void p(String label, Object v) { System.out.println(label + " = " + v); }
  static Path g(String s) { return Paths.get(s); }
  public static void main(String[] a) {
    // relativize
    p("rel C:\\a\\b -> C:\\a\\b\\c\\d", g("C:\\a\\b").relativize(g("C:\\a\\b\\c\\d")));
    p("rel C:\\a\\b -> C:\\a\\x",      g("C:\\a\\b").relativize(g("C:\\a\\x")));
    p("rel C:\\a -> C:\\a",            g("C:\\a").relativize(g("C:\\a")));
    p("rel a\\b -> a\\b\\c",           g("a\\b").relativize(g("a\\b\\c")));
    p("rel a\\b\\c -> a\\b",           g("a\\b\\c").relativize(g("a\\b")));

    // normalize edge cases
    p("norm C:\\a\\..\\..\\b",  g("C:\\a\\..\\..\\b").normalize());
    p("norm ..\\..\\a",          g("..\\..\\a").normalize());
    p("norm a\\.\\b\\..",        g("a\\.\\b\\..").normalize());
    p("norm C:\\..",             g("C:\\..").normalize());
    p("norm .",                  g(".").normalize());
    p("norm empty",              g("").normalize());

    // subpath
    p("subpath C:\\a\\b\\c (0,2)", g("C:\\a\\b\\c").subpath(0, 2));
    p("subpath C:\\a\\b\\c (1,3)", g("C:\\a\\b\\c").subpath(1, 3));

    // resolve edge
    p("resolve empty",            g("C:\\a").resolve(""));
    p("resolve abs-other",        g("C:\\a").resolve("C:\\x"));
    p("resolveSibling root",      g("C:\\").resolveSibling("x"));

    // getName / count for tricky inputs
    p("nc C:\\",                  g("C:\\").getNameCount());
    p("nc \\\\s\\share\\",        g("\\\\s\\share\\").getNameCount());
    p("startsWith C:\\a\\b / C:\\a",   g("C:\\a\\b").startsWith(g("C:\\a")));
    p("startsWith C:\\a\\b / C:\\ab",  g("C:\\a\\b").startsWith(g("C:\\ab")));
    p("endsWith C:\\a\\b\\c / b\\c",   g("C:\\a\\b\\c").endsWith(g("b\\c")));
    p("endsWith C:\\a\\b\\c / \\c",    g("C:\\a\\b\\c").endsWith(g("c")));

    // equals / compareTo case-insensitivity (Windows)
    p("equals C:\\A == c:\\a",   g("C:\\A").equals(g("c:\\a")));

    // toUri
    p("toUri C:\\a\\b",          g("C:\\a\\b").toUri());
  }
}
