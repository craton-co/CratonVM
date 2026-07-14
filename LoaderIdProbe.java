public class LoaderIdProbe {
  public static void main(String[] args) {
    ClassLoader a = new ClassLoader(ClassLoader.getSystemClassLoader()) { };
    ClassLoader b = new ClassLoader(ClassLoader.getSystemClassLoader()) { };
    System.out.println("a=" + System.identityHashCode(a));
    System.out.println("b=" + System.identityHashCode(b));
    System.out.println("same=" + (a == b));
  }
}
