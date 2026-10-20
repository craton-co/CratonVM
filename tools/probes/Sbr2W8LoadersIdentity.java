// SBR2 wave 8, lane LOADERS: the built-in loaders are the JDK's own ClassLoaders objects.
// Run: javac -d out Sbr2W8LoadersIdentity.java
//      java|cratonvm [--jdk-only|--compatible] --add-opens java.base/jdk.internal.loader=ALL-UNNAMED -cp out Sbr2W8LoadersIdentity
// Diff the output against HotSpot 25 verbatim.
import java.lang.reflect.Field;

public class Sbr2W8LoadersIdentity {
    static Object st(Class<?> c, String n) throws Exception {
        Field f = c.getDeclaredField(n);
        f.setAccessible(true);
        return f.get(null);
    }

    public static void main(String[] a) throws Exception {
        Class<?> c = Class.forName("jdk.internal.loader.ClassLoaders");
        Object boot = st(c, "BOOT_LOADER"), plat = st(c, "PLATFORM_LOADER"), app = st(c, "APP_LOADER");
        System.out.println("boot class: " + (boot == null ? null : boot.getClass().getName()));
        System.out.println("plat class: " + (plat == null ? null : plat.getClass().getName()));
        System.out.println("app class: " + (app == null ? null : app.getClass().getName()));
        System.out.println("sys == APP_LOADER: " + (ClassLoader.getSystemClassLoader() == app));
        System.out.println("platform == PLATFORM_LOADER: " + (ClassLoader.getPlatformClassLoader() == plat));
        System.out.println("sys.parent == platform: " + (ClassLoader.getSystemClassLoader().getParent() == plat));
        System.out.println("platform.parent: " + ClassLoader.getPlatformClassLoader().getParent());
        System.out.println("app loader of me: " + (Sbr2W8LoadersIdentity.class.getClassLoader() == app));
        System.out.println("String loader: " + String.class.getClassLoader());
        System.out.println("java.sql loader == platform: " + (java.sql.Connection.class.getClassLoader() == plat));
        System.out.println("sys name: " + ClassLoader.getSystemClassLoader().getName());
        System.out.println("platform name: " + ClassLoader.getPlatformClassLoader().getName());
        System.out.println("forName via sys: " + Class.forName("java.util.ArrayList", false, ClassLoader.getSystemClassLoader()).getName());
        System.out.println("sys.loadClass: " + ClassLoader.getSystemClassLoader().loadClass("Sbr2W8LoadersIdentity").getName());
        try { ClassLoader.getSystemClassLoader().loadClass("no.such.Cls"); } catch (ClassNotFoundException e) { System.out.println("CNFE " + e.getMessage()); }
        System.out.println("forName(Module): " + Class.forName(Object.class.getModule(), "java.lang.String"));
        System.out.println("forName(Module,app): " + Class.forName(Object.class.getModule(), "Sbr2W8LoadersIdentity"));
        System.out.println("getResource: " + (ClassLoader.getSystemClassLoader().getResource("Sbr2W8LoadersIdentity.class") != null));
        System.out.println("pkg: " + Sbr2W8LoadersIdentity.class.getPackage());
    }
}
