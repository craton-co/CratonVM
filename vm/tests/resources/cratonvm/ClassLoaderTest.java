package cratonvm;

/**
 * Session 3: ClassLoader parent delegation fix — JVM spec §5.3 compliance.
 */
public class ClassLoaderTest {

    // 91.3: Test that Thread.getContextClassLoader returns non-null
    public static int testContextClassLoader() {
        ClassLoader cl = Thread.currentThread().getContextClassLoader();
        return cl != null ? 1 : 0;  // 1
    }

    // 91.3: Test parent delegation — getParent() chain
    public static int testParentDelegation() {
        ClassLoader app = ClassLoaderTest.class.getClassLoader();
        // App class loader should not be null for user classes
        if (app == null) return 0;
        // App's parent should be the platform class loader
        ClassLoader parent = app.getParent();
        if (parent == null) return 0;
        // Platform's parent is bootstrap (null)
        ClassLoader grandparent = parent.getParent();
        if (grandparent != null) return 0;
        return 1;  // correct chain: app → platform → null (bootstrap)
    }

    // 91.3: Test getContextClassLoader / setContextClassLoader round-trip
    public static int testSetContextClassLoader() {
        Thread t = Thread.currentThread();
        ClassLoader original = t.getContextClassLoader();
        // Set a new context class loader
        ClassLoader newLoader = ClassLoaderTest.class.getClassLoader();
        t.setContextClassLoader(newLoader);
        ClassLoader retrieved = t.getContextClassLoader();
        // Restore original
        t.setContextClassLoader(original);
        // Verify the set/get round-trip worked
        return retrieved != null ? 1 : 0;  // 1
    }

    // 91.3: Test Class.getClassLoader returns non-null for user classes
    public static int testClassGetClassLoader() {
        ClassLoader cl = ClassLoaderTest.class.getClassLoader();
        // User class should have a non-null loader
        return cl != null ? 1 : 0;
    }

    // Session 3: Bootstrap classes return null from getClassLoader()
    public static int testBootstrapClassLoaderIsNull() {
        // java.lang.Object is loaded by bootstrap — getClassLoader() must return null
        ClassLoader cl = Object.class.getClassLoader();
        return cl == null ? 1 : 0;
    }

    // Session 3: String is a bootstrap class too
    public static int testStringBootstrapLoader() {
        ClassLoader cl = String.class.getClassLoader();
        return cl == null ? 1 : 0;
    }

    // Session 3: User class has non-null loader with working getName()
    public static int testLoaderName() {
        ClassLoader cl = ClassLoaderTest.class.getClassLoader();
        if (cl == null) return 0;
        String name = cl.getName();
        // App loader's name should be "app"
        if (name == null) return 0;
        return "app".equals(name) ? 1 : 0;
    }

    // Session 3: Platform class loader name is "platform"
    public static int testPlatformLoaderName() {
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        if (platform == null) return 0;
        String name = platform.getName();
        if (name == null) return 0;
        return "platform".equals(name) ? 1 : 0;
    }

    // Session 3: getSystemClassLoader returns app loader with correct parent chain
    public static int testSystemClassLoaderChain() {
        ClassLoader system = ClassLoader.getSystemClassLoader();
        if (system == null) return 0;
        // System loader's parent should be platform
        ClassLoader parent = system.getParent();
        if (parent == null) return 0;
        // Platform's parent should be null (bootstrap)
        ClassLoader gp = parent.getParent();
        if (gp != null) return 0;
        return 1;
    }

    // Session 3: ClassLoader.loadClass delegates to parent first
    public static int testLoadClassDelegation() {
        ClassLoader cl = ClassLoader.getSystemClassLoader();
        if (cl == null) return 0;
        try {
            // Loading a bootstrap class via app loader should succeed
            Class<?> objClass = cl.loadClass("java.lang.Object");
            if (objClass == null) return 0;
            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // Session 3: findLoadedClass returns null for unloaded class
    // (We can't directly test this from Java since findLoadedClass is protected,
    // but loadClass for a known class should work.)
    public static int testLoadClassForUserClass() {
        ClassLoader cl = ClassLoader.getSystemClassLoader();
        if (cl == null) return 0;
        try {
            Class<?> c = cl.loadClass("cratonvm.ClassLoaderTest");
            return c != null ? 1 : 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // Session 3: getClassLoader() returns the same object each time (singleton identity)
    public static int testClassLoaderIdentity() {
        ClassLoader cl1 = ClassLoaderTest.class.getClassLoader();
        ClassLoader cl2 = ClassLoaderTest.class.getClassLoader();
        if (cl1 == null || cl2 == null) return 0;
        // Must be the exact same instance (==, not just equals)
        return cl1 == cl2 ? 1 : 0;
    }

    // Session 3: getSystemClassLoader returns the same instance each time
    public static int testSystemClassLoaderIdentity() {
        ClassLoader s1 = ClassLoader.getSystemClassLoader();
        ClassLoader s2 = ClassLoader.getSystemClassLoader();
        if (s1 == null || s2 == null) return 0;
        return s1 == s2 ? 1 : 0;
    }

    // Session 3: Loader isolation — same class loaded by different loader types
    // are from different namespaces. Bootstrap-loaded Object != app-loaded user class.
    public static int testLoaderIsolation() {
        // Object is loaded by bootstrap (getClassLoader returns null)
        ClassLoader objLoader = Object.class.getClassLoader();
        // ClassLoaderTest is loaded by app loader (getClassLoader returns non-null)
        ClassLoader testLoader = ClassLoaderTest.class.getClassLoader();
        // They should be different: one null (bootstrap), one non-null (app)
        if (objLoader != null) return 0;  // Object must be bootstrap
        if (testLoader == null) return 0; // User class must NOT be bootstrap
        return 1;
    }

    // ------------------------------------------------------------------
    // Regression: custom ClassLoader must NOT be ignored by loadClass /
    // Class.forName (SC-custom-classloader-ignored).
    //
    // A loader that overrides the protected loadClass(String,boolean) — like
    // Spring's OverridingClassLoader and any classloader-isolation pattern —
    // must have that override actually invoked, instead of the VM silently
    // resolving the class through the global/app class store. A fresh custom
    // loader's findLoadedClass must also return null for a class only the app
    // loader has loaded (JVMS §5.3: it is not yet an initiating loader for it).
    // ------------------------------------------------------------------
    static final class ProbingLoader extends ClassLoader {
        volatile boolean overrideRan = false;
        volatile boolean findLoadedWasNull = false;

        ProbingLoader(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            overrideRan = true;
            if ("cratonvm.ClassLoaderTest".equals(name)) {
                // This loader has never defined/initiated the class; per JVMS
                // findLoadedClass must return null here — NOT the app copy.
                findLoadedWasNull = (findLoadedClass(name) == null);
            }
            return super.loadClass(name, resolve);
        }
    }

    // FIX #1 + #2: loadClass(String) entry must dispatch the subclass
    // loadClass(String,boolean) override, and findLoadedClass must be
    // loader-scoped.
    public static int testCustomLoaderOverrideInvoked() {
        ProbingLoader pl = new ProbingLoader(ClassLoaderTest.class.getClassLoader());
        try {
            Class<?> c = pl.loadClass("cratonvm.ClassLoaderTest");
            if (c == null) return 0;              // delegation still resolves it
            if (!pl.overrideRan) return 0;        // FIX #1: override must run
            if (!pl.findLoadedWasNull) return 0;  // FIX #2: no global leak
            return 1;
        } catch (ClassNotFoundException e) {
            return 0;
        }
    }

    // FIX #1 via the Class.forName(name, false, loader) entry point.
    public static int testForNameHonorsCustomLoaderOverride() {
        ProbingLoader pl = new ProbingLoader(ClassLoaderTest.class.getClassLoader());
        try {
            Class<?> c = Class.forName("cratonvm.ClassLoaderTest", false, pl);
            return (c != null && pl.overrideRan) ? 1 : 0;
        } catch (ClassNotFoundException e) {
            return 0;
        }
    }
}
