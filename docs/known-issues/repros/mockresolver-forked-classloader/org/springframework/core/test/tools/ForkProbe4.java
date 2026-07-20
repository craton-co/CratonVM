package org.springframework.core.test.tools;

public class ForkProbe4 {
    public static void main(String[] args) throws Exception {
        // Touch Mockito via the ORIGINAL app classloader FIRST, before forking,
        // mirroring Spring Test's own early Mockito-presence checks
        // (@MockitoBean / MockitoBeanContextCustomizerFactory-style scanning)
        // that might run before CompileWithForkedClassLoaderExtension forks.
        Class<?> mockitoAppLoader = Class.forName("org.mockito.Mockito");
        System.out.println("Mockito loaded via: " + mockitoAppLoader.getClassLoader());

        ClassLoader appLoader = ForkProbe4.class.getClassLoader();
        CompileWithForkedClassLoaderClassLoader forked =
                new CompileWithForkedClassLoaderClassLoader(appLoader);
        Thread.currentThread().setContextClassLoader(forked);

        TestCompiler.forSystem().compile(
                SourceFile.of("public class GeneratedDummy4 {}"),
                compiled -> {
                    ClassLoader dyn = Thread.currentThread().getContextClassLoader();
                    System.out.println("ctx classloader inside compile callback = " + dyn);
                    try {
                        Object mock = org.mockito.Mockito.mock(java.util.List.class);
                        System.out.println("MOCK OK: " + mock + " mockitoClassLoader="
                                + mock.getClass().getClassLoader());
                    } catch (Throwable t) {
                        System.out.println("MOCK FAILED: " + t);
                        t.printStackTrace(System.out);
                    }
                });
        System.out.println("DONE");
    }
}
