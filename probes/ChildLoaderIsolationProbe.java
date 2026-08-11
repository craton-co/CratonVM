import java.io.File;
import java.net.URL;
import java.net.URLClassLoader;

/**
 * A {@code URLClassLoader} whose parent is the bootstrap loader must define its
 * OWN copy of any class it finds on its URLs — that is the whole mechanism
 * behind Spring Boot's {@code ModifiedClassPathClassLoader}, Mockito's plugin
 * isolation, and every "two versions of one library in one JVM" test.
 *
 * CratonVM was observed handing such a loader the APPLICATION loader's class
 * instead, so the two "worlds" are one. That silently breaks anything that
 * compares {@code Class} objects by identity — {@code Method.equals} does, which
 * is how Byte Buddy's {@code JavaDispatcher} ends up throwing
 * {@code No proxy target found for ...}.
 *
 * Reports, for each name: whether the child produced a distinct Class, what
 * loader each Class reports, and whether the child loader is the defining
 * loader. Exits non-zero if any name failed to isolate.
 *
 * Usage: {@code ChildLoaderIsolationProbe <jar> <class name>...}
 */
public final class ChildLoaderIsolationProbe {

    public static void main(String[] args) throws Exception {
        URL jar = new File(args[0]).toURI().toURL();
        boolean failed = false;

        for (int i = 1; i < args.length; i++) {
            String name = args[i];
            Class<?> app = Class.forName(name);
            URLClassLoader child = new URLClassLoader(new URL[] { jar }, null);
            Class<?> viaForName = Class.forName(name, false, child);
            Class<?> viaLoadClass = child.loadClass(name);

            boolean isolatedForName = viaForName != app && viaForName.getClassLoader() == child;
            boolean isolatedLoadClass = viaLoadClass != app && viaLoadClass.getClassLoader() == child;
            if (!isolatedForName || !isolatedLoadClass) {
                failed = true;
            }
            System.out.println("PROBE name=" + name
                    + " forNameIsolated=" + isolatedForName
                    + " loadClassIsolated=" + isolatedLoadClass
                    + " sameClass(forName)=" + (viaForName == app)
                    + " sameClass(loadClass)=" + (viaLoadClass == app)
                    + " appLoader=" + loaderName(app.getClassLoader())
                    + " forNameLoader=" + loaderName(viaForName.getClassLoader())
                    + " loadClassLoader=" + loaderName(viaLoadClass.getClassLoader()));
            child.close();
        }

        System.out.println("PROBE result=" + (failed ? "NOT_ISOLATED" : "OK"));
        if (failed) {
            System.exit(1);
        }
    }

    private static String loaderName(ClassLoader loader) {
        return loader == null ? "<bootstrap>" : loader.getClass().getName() + "@"
                + System.identityHashCode(loader);
    }
}
