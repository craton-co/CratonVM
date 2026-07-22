public class Ver {
  public static void main(String[] a) {
    System.out.println("java.version=" + System.getProperty("java.version"));
    System.out.println("java.home=" + System.getProperty("java.home"));
    for (java.lang.reflect.Field f : Thread.class.getDeclaredFields()) {
      if (!java.lang.reflect.Modifier.isStatic(f.getModifiers()))
        System.out.println("  Thread field: " + f.getName() + " : " + f.getType().getSimpleName());
    }
  }
}
