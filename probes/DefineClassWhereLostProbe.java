import java.net.URL;
import java.net.URLClassLoader;

/**
 * Localises WHERE the defineClass1 ClassFormatError stops being catchable:
 * a try/catch immediately around `defineClass` inside findClass, one around
 * `loadClass`, and one around `Class.forName`.
 */
public class DefineClassWhereLostProbe {

    static final class InnerCatchLoader extends URLClassLoader {

        InnerCatchLoader() {
            super(new URL[0], InnerCatchLoader.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            System.out.println("    findClass entered: " + name);
            try {
                Class<?> c = defineClass(name, new byte[10], 0, 10);
                System.out.println("    defineClass returned " + c + "  <-- WRONG, expected throw");
                return c;
            }
            catch (Throwable ex) {
                System.out.println("    [inner catch] " + ex.getClass().getName() + ": " + ex.getMessage());
                throw new ClassNotFoundException(name, ex);
            }
        }
    }

    static final class PlainLoader extends URLClassLoader {

        PlainLoader() {
            super(new URL[0], PlainLoader.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            return defineClass(name, new byte[10], 0, 10);
        }
    }

    public static void main(String[] args) {
        System.out.println("[1] catch INSIDE findClass, right around defineClass");
        try {
            new InnerCatchLoader().loadClass("probe.inner.Sample");
        }
        catch (Throwable ex) {
            System.out.println("  outer saw: " + ex.getClass().getName() + ": " + ex.getMessage());
        }
        System.out.println("  survived stage 1");

        System.out.println("[2] catch around loadClass only");
        try {
            new PlainLoader().loadClass("probe.plain.Sample");
        }
        catch (Throwable ex) {
            System.out.println("  outer saw: " + ex.getClass().getName() + ": " + ex.getMessage());
        }
        System.out.println("  survived stage 2");

        System.out.println("DONE");
    }
}
