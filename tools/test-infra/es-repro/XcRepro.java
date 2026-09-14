public class XcRepro {
  public static void main(String[] a) throws Throwable {
    Class<?> xp = Class.forName("org.elasticsearch.xcontent.spi.XContentProvider");
    java.lang.reflect.Method m = xp.getDeclaredMethod("provider");
    m.setAccessible(true);
    Object p = m.invoke(null);
    System.out.println("XContentProvider.provider() = " + p);
  }
}
