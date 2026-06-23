import java.nio.file.*;

// Thorough verification for the keycloak-15 Windows-path residuals.
public class PVerify {
  static int fails = 0;
  static void chk(String label, Object got, Object want) {
    boolean ok = String.valueOf(got).equals(String.valueOf(want));
    if (!ok) fails++;
    System.out.println((ok ? "OK  " : "FAIL") + " | " + label
        + " | got=" + got + " want=" + want);
  }
  static void row(String in) {
    Path p = Paths.get(in);
    System.out.println("--- in=[" + in + "] str=" + p + " ---");
    System.out.println("   root=" + p.getRoot() + " nc=" + p.getNameCount()
        + " parent=" + p.getParent() + " abs=" + p.isAbsolute());
  }
  public static void main(String[] a) {
    // isAbsolute
    chk("isAbs C:\\foo\\bar", Paths.get("C:\\foo\\bar").isAbsolute(), true);
    chk("isAbs C:foo",        Paths.get("C:foo").isAbsolute(),        false);
    chk("isAbs \\foo\\bar",   Paths.get("\\foo\\bar").isAbsolute(),   false);
    chk("isAbs a\\b\\c",      Paths.get("a\\b\\c").isAbsolute(),      false);
    chk("isAbs UNC",          Paths.get("\\\\srv\\sh\\d").isAbsolute(), true);

    // getParent (toString)
    chk("parent C:\\a\\b\\.", String.valueOf(Paths.get("C:\\a\\b\\.").getParent()), "C:\\a\\b");
    chk("parent C:\\foo\\bar", String.valueOf(Paths.get("C:\\foo\\bar").getParent()), "C:\\foo");
    chk("parent C:foo\\bar",   String.valueOf(Paths.get("C:foo\\bar").getParent()),   "C:foo");
    chk("parent C:foo",        String.valueOf(Paths.get("C:foo").getParent()),        "C:");
    chk("parent \\foo\\bar",   String.valueOf(Paths.get("\\foo\\bar").getParent()),   "\\foo");
    chk("parent a\\b\\c",      String.valueOf(Paths.get("a\\b\\c").getParent()),      "a\\b");
    chk("parent a",            String.valueOf(Paths.get("a").getParent()),            "null");
    chk("parent C:\\",         String.valueOf(Paths.get("C:\\").getParent()),         "null");
    chk("parent UNC d\\e",     String.valueOf(Paths.get("\\\\srv\\sh\\d\\e").getParent()), "\\\\srv\\sh\\d");

    System.out.println("RESULT=" + (fails == 0 ? "OK" : ("FAIL(" + fails + ")")));
  }
}
