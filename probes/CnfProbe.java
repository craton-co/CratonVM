public class CnfProbe {
  public static void main(String[] a) {
    String n = "org.springframework.batch.support.SerializationUtilsTests$Foo";
    try { Class<?> c = Class.forName(n); System.out.println("forName -> " + c + " loader=" + c.getClassLoader()); }
    catch (Throwable t) { System.out.println("forName threw " + t.getClass().getName() + ": " + t.getMessage()); }
    try { Class<?> c = Class.forName(n, false, CnfProbe.class.getClassLoader()); System.out.println("forName3 -> " + c); }
    catch (Throwable t) { System.out.println("forName3 threw " + t.getClass().getName()); }
  }
}
