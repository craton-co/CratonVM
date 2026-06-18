import java.lang.reflect.*;

// bug-06 family 3 isolation: Spring's assertClassNotLoaded calls
// ClassLoader.findLoadedClass(name) reflectively and fails if it returns
// non-null. Test whether CratonVM's findLoadedClass over-reports: it must
// return null for a class that has NOT been loaded through this loader, and
// non-null only after the class is actually initialized/used.
public class FindLoaded {
  static Object flc(ClassLoader cl, String name) throws Exception {
    Method m = ClassLoader.class.getDeclaredMethod("findLoadedClass", String.class);
    m.setAccessible(true);
    return m.invoke(cl, name);
  }
  public static void main(String[] a) throws Exception {
    ClassLoader sys = ClassLoader.getSystemClassLoader();
    // A class that exists on the classpath but we never reference/use:
    String unused = "java.util.zip.CRC32C";
    System.out.println("findLoadedClass(CRC32C) before use = " + flc(sys, unused) + " (expect null)");
    // A bootstrap class never referenced as a *system*-loaded class:
    System.out.println("findLoadedClass(java.lang.ProcessHandleImpl) = " + flc(sys, "java.lang.ProcessHandleImpl") + " (expect null on system loader)");
    // A class we definitely use -> should be findable (loaded), but it's a
    // bootstrap class so findLoadedClass on the SYSTEM loader is still null in
    // HotSpot (bootstrap classes aren't in the app loader's table):
    System.out.println("findLoadedClass(java.lang.String) on system = " + flc(sys, "java.lang.String") + " (HotSpot: null)");
    // Force-load CRC32C through the system loader, then it should appear:
    Class.forName(unused);
    System.out.println("findLoadedClass(CRC32C) after forName = " + (flc(sys, unused)!=null) + " (expect true)");
    System.out.println("DONE");
  }
}
