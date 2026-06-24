import groovy.lang.GroovyClassLoader;
public class AsBoolProbe {
  public static void main(String[] a) throws Throwable {
    GroovyClassLoader gcl = new GroovyClassLoader(AsBoolProbe.class.getClassLoader());
    String src =
      "class A {\n" +
      "  def boolF()  { return Boolean.FALSE.asBoolean() }\n" +
      "  def boolT()  { return Boolean.TRUE.asBoolean() }\n" +
      "  def strEmpty(){ return ''.asBoolean() }\n" +
      "  def strX()   { return 'x'.asBoolean() }\n" +
      "  def listEmpty(){ return [].asBoolean() }\n" +
      "  def num0()   { return (0 as Integer).asBoolean() }\n" +
      "}\n";
    Class<?> c = gcl.parseClass(src);
    Object o = c.getDeclaredConstructor().newInstance();
    for (String m : new String[]{"boolF","boolT","strEmpty","strX","listEmpty","num0"})
      System.out.println(m + "=" + c.getDeclaredMethod(m).invoke(o));
    System.out.println("ASBOOLPROBE_DONE");
  }
}
