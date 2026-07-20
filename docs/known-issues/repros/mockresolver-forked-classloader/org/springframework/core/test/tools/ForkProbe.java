package org.springframework.core.test.tools;

public class ForkProbe {
    public static void main(String[] args) throws Exception {
        ClassLoader appLoader = ForkProbe.class.getClassLoader();
        System.out.println("appLoader=" + appLoader);
        System.out.println("appLoader.getClass()=" + appLoader.getClass());

        CompileWithForkedClassLoaderClassLoader forked =
                new CompileWithForkedClassLoaderClassLoader(appLoader);
        System.out.println("forked=" + forked);

        String resPath = "org/springframework/test/context/bean/override/mockito/SpringMockResolver.class";
        String resPathDot = "org.springframework.test.context.bean.override.mockito.SpringMockResolver";

        System.out.println("--- direct on appLoader ---");
        probe(appLoader, resPath, resPathDot);

        System.out.println("--- direct on forked (no DynamicClassLoader wrapping) ---");
        probe(forked, resPath, resPathDot);

        System.out.println("--- via forked as context classloader ---");
        ClassLoader orig = Thread.currentThread().getContextClassLoader();
        Thread.currentThread().setContextClassLoader(forked);
        try {
            probe(Thread.currentThread().getContextClassLoader(), resPath, resPathDot);
        } finally {
            Thread.currentThread().setContextClassLoader(orig);
        }
        System.out.println("DONE");
    }

    static void probe(ClassLoader cl, String resPath, String resPathDot) {
        try {
            java.io.InputStream in = cl.getResourceAsStream(resPath);
            System.out.println("  getResourceAsStream = " + in);
            if (in != null) {
                byte[] b = in.readAllBytes();
                System.out.println("  read " + b.length + " bytes");
                in.close();
            }
        } catch (Throwable t) {
            System.out.println("  getResourceAsStream THREW: " + t);
        }
        try {
            Class<?> c = cl.loadClass(resPathDot);
            System.out.println("  loadClass OK: " + c + " loader=" + c.getClassLoader());
        } catch (Throwable t) {
            System.out.println("  loadClass THREW: " + t);
        }
        try {
            java.util.Enumeration<java.net.URL> urls =
                    cl.getResources("mockito-extensions/org.mockito.plugins.MockResolver");
            int count = 0;
            while (urls.hasMoreElements()) {
                System.out.println("  mockito-extensions[" + count++ + "] = " + urls.nextElement());
            }
            System.out.println("  mockito-extensions count=" + count);
        } catch (Throwable t) {
            System.out.println("  getResources THREW: " + t);
        }
    }
}
