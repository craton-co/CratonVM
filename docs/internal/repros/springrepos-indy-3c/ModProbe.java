public class ModProbe {
  public static void main(String[] x) throws Throwable {
    Class<?>[] cs = { Object.class, String.class, ModProbe.class,
        Class.forName("org.gradle.api.Action"),
        Class.forName("groovy.lang.Closure") };
    for (Class<?> c : cs) {
      Module m = c.getModule();
      System.out.println(c.getName() + " -> module name=" + m.getName() + " isNamed=" + m.isNamed());
    }
    // The access check that NPEs:
    try {
      boolean op = Object.class.getModule().isExported("java.lang");
      System.out.println("java.base.isExported(java.lang)=" + op);
    } catch (Throwable t) { System.out.println("isExported THREW: " + t); }
    System.out.println("MODPROBE_DONE");
  }
}
