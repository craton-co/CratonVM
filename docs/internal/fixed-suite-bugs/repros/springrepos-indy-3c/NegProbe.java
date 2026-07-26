import groovy.lang.GroovyClassLoader;
public class NegProbe {
  public static void main(String[] a) throws Throwable {
    GroovyClassLoader gcl = new GroovyClassLoader(NegProbe.class.getClassLoader());
    String src =
      "class N {\n" +
      "  def notEq(s) { return !\"commercial\".equalsIgnoreCase(s) }\n" +
      "  def guard(s) { if (!\"commercial\".equalsIgnoreCase(s)) { return 'RETURN' } else { return 'CONTINUE' } }\n" +
      "  def guardEnds(v){ if (v.endsWith('-SNAPSHOT')) { return 'SNAP' } else { return 'NOSNAP' } }\n" +
      "}\n";
    Class<?> c = gcl.parseClass(src);
    Object n = c.getDeclaredConstructor().newInstance();
    System.out.println("notEq('oss')=" + c.getDeclaredMethod("notEq",Object.class).invoke(n,"oss") + " (expect true)");
    System.out.println("guard('oss')=" + c.getDeclaredMethod("guard",Object.class).invoke(n,"oss") + " (expect RETURN)");
    System.out.println("guardEnds('0.0.1')=" + c.getDeclaredMethod("guardEnds",Object.class).invoke(n,"0.0.1") + " (expect NOSNAP)");
    System.out.println("NEGPROBE_DONE");
  }
}
