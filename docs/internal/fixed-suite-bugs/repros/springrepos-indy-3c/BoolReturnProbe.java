import groovy.lang.GroovyClassLoader;

// Narrow 3e: a boolean-returning method (String.endsWith) called via Groovy indy
// from a compiled Groovy class returns null instead of true/false on CratonVM.
public class BoolReturnProbe {
  public static void main(String[] a) throws Throwable {
    GroovyClassLoader gcl = new GroovyClassLoader(BoolReturnProbe.class.getClassLoader());
    String src =
        "class B {\n" +
        "  def endsW(s)   { return s.endsWith('-SNAPSHOT') }\n" +
        "  def equalsIc(s){ return 'commercial'.equalsIgnoreCase(s) }\n" +
        "  def ifTest(s)  { if (s.endsWith('-SNAPSHOT')) { return 'YES' } else { return 'NO' } }\n" +
        "  def lenGt(s)   { return s.length() > 2 }\n" +
        "}\n";
    Class<?> c = gcl.parseClass(src);
    Object b = c.getDeclaredConstructor().newInstance();
    Object r1 = c.getDeclaredMethod("endsW", Object.class).invoke(b, "0.0.0-SNAPSHOT");
    Object r2 = c.getDeclaredMethod("endsW", Object.class).invoke(b, "0.0.1");
    Object r3 = c.getDeclaredMethod("equalsIc", Object.class).invoke(b, "oss");
    Object r4 = c.getDeclaredMethod("ifTest", Object.class).invoke(b, "0.0.0-SNAPSHOT");
    Object r5 = c.getDeclaredMethod("lenGt", Object.class).invoke(b, "abcd");
    System.out.println("endsW('..-SNAPSHOT')=" + r1 + " (expect true)");
    System.out.println("endsW('0.0.1')=" + r2 + " (expect false)");
    System.out.println("equalsIc('oss')=" + r3 + " (expect false)");
    System.out.println("ifTest('..-SNAPSHOT')=" + r4 + " (expect YES)");
    System.out.println("lenGt('abcd')=" + r5 + " (expect true)");
    System.out.println("BOOLRETURNPROBE_DONE");
  }
}
