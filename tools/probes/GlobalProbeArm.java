import java.lang.reflect.Method;
public class GlobalProbeArm {
    static String s(Object p) { return p == null ? "null" : String.valueOf(p); }
    static class Custom extends ClassLoader { Custom() { super(null); } }
    public static void main(String[] a) throws Exception {
        Custom c = new Custom();
        for (String p : new String[]{"java.lang","java.util","java.sql","com.example.app","no.such"}) {
            System.out.println("ROW custom.getDefinedPackage(" + p + ") = " + s(c.getDefinedPackage(p)));
        }
        Class<?> cls = Class.forName("jdk.internal.loader.ClassLoaders");
        Method bm = cls.getDeclaredMethod("bootLoader"); bm.setAccessible(true);
        ClassLoader boot = (ClassLoader) bm.invoke(null);
        for (String p : new String[]{"java.lang","java.util","java.sql","com.example.app","no.such"}) {
            System.out.println("ROW boot.getDefinedPackage(" + p + ") = " + s(boot.getDefinedPackage(p)));
        }
        System.out.println("ROW boot.class = " + boot.getClass().getName());
        System.out.println("DONE GlobalProbeArm");
    }
}
