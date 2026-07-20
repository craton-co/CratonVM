package org.springframework.core.test.tools;

public class ForkProbe2 {
    public static void main(String[] args) throws Exception {
        ClassLoader appLoader = ForkProbe2.class.getClassLoader();
        CompileWithForkedClassLoaderClassLoader forked =
                new CompileWithForkedClassLoaderClassLoader(appLoader);
        Thread.currentThread().setContextClassLoader(forked);

        String resPath = "org/springframework/test/context/bean/override/mockito/SpringMockResolver.class";
        String resPathDot = "org.springframework.test.context.bean.override.mockito.SpringMockResolver";

        TestCompiler.forSystem().compile(
                SourceFile.of("public class GeneratedDummy2 {}"),
                compiled -> {
                    ClassLoader dyn = Thread.currentThread().getContextClassLoader();
                    System.out.println("ctx classloader inside compile callback = " + dyn
                            + " (" + dyn.getClass() + ")");
                    ForkProbe.probe(dyn, resPath, resPathDot);
                });
        System.out.println("DONE");
    }
}
