import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Path;

/**
 * Spring's @CompileWithForkedClassLoader tests define a second copy of every non-JDK class; a later
 * test in the app loader then runs the enum's compiler-generated values() (checkcast [LE;).
 * args[0] = dir holding a second copy of E and F; the child loader's parent is the platform loader.
 * HotSpot prints "app values = 2"; a VM that resolves "[LE;" by name alone throws ClassCastException.
 */
public class ForkEnumProbe {
    public static void main(String[] a) throws Exception {
        ClassLoader child = new URLClassLoader(new URL[] {Path.of(a[0]).toUri().toURL()},
                ClassLoader.getPlatformClassLoader());
        Class<?> forkE = Class.forName("E", true, child);
        Object arr = Class.forName("F", true, child).getMethod("go").invoke(null);
        System.out.println("fork values component is fork E = " + (arr.getClass().getComponentType() == forkE));
        System.out.println("app values = " + E.values().length);
    }
}
