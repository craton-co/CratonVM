/**
 * Regression: optional dependencies must remain absent when they are not on
 * the class path. Spring Data uses this exact Class.forName form for
 * org.jmolecules.ddd.types.Association. Keep the lookup contract covered as
 * an independent class-loading boundary.
 */
public class ROptionalClassForName {
    private static final String MISSING = "org.jmolecules.ddd.types.Association";
    private static int checks;

    private static void check(boolean condition, String message) {
        checks++;
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    private static void expectClassNotFound(ThrowingRunnable action, String message)
            throws Exception {
        try {
            action.run();
            throw new AssertionError(message + " did not throw ClassNotFoundException");
        }
        catch (ClassNotFoundException expected) {
            check(MISSING.equals(expected.getMessage()), message + " exception message");
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader loader = ROptionalClassForName.class.getClassLoader();
        check(loader != null, "application class loader");
        expectClassNotFound(() -> Class.forName(MISSING, false, loader),
                "Class.forName(String, boolean, ClassLoader)");
        expectClassNotFound(() -> loader.loadClass(MISSING), "ClassLoader.loadClass(String)");
        System.out.println("PASS ROptionalClassForName (" + checks + " checks)");
    }

    @FunctionalInterface
    private interface ThrowingRunnable {
        void run() throws Exception;
    }
}
