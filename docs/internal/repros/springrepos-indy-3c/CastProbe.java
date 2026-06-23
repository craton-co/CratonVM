import groovy.lang.GroovyClassLoader;
public class CastProbe {
  public static void main(String[] a) throws Throwable {
    GroovyClassLoader gcl = new GroovyClassLoader(CastProbe.class.getClassLoader());
    String src =
      "class C {\n" +
      "  def castFalseObj() { Object o = Boolean.FALSE; if (o) { return 'TRUE' } else { return 'FALSE' } }\n" +
      "  def castTrueObj()  { Object o = Boolean.TRUE;  if (o) { return 'TRUE' } else { return 'FALSE' } }\n" +
      "  def castBoolLit()  { if (false) { return 'TRUE' } else { return 'FALSE' } }\n" +
      "  def castNull()     { Object o = null; if (o) { return 'TRUE' } else { return 'FALSE' } }\n" +
      "  def castStr()      { Object o = 'x'; if (o) { return 'TRUE' } else { return 'FALSE' } }\n" +
      "  def castEmptyStr() { Object o = ''; if (o) { return 'TRUE' } else { return 'FALSE' } }\n" +
      "}\n";
    Class<?> c = gcl.parseClass(src);
    Object o = c.getDeclaredConstructor().newInstance();
    for (String m : new String[]{"castFalseObj","castTrueObj","castBoolLit","castNull","castStr","castEmptyStr"})
      System.out.println(m + "=" + c.getDeclaredMethod(m).invoke(o));
    System.out.println("CASTPROBE_DONE");
  }
}
