package org.springframework.boot.testsupport.classpath;

import java.net.URL;
import java.util.HashSet;
import java.util.Set;
import java.util.concurrent.atomic.AtomicInteger;

// SPRINGBOOT-WITHOUT-JACKSON.2 repro: drives the real
// ModifiedClassPathClassLoader.loadClass(String) -- a package-private class,
// hence this probe lives in the same package -- with heavy repeated calls
// across many class names, some excluded (mimicking "without jackson"),
// exercising both the exclusion-check branch and the super.loadClass()
// delegation branch many times to cross JIT invocation thresholds.
public class SpringBootLoadClassProbe {
    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20000;

        ClassLoader system = ClassLoader.getSystemClassLoader();
        String cp = System.getProperty("java.class.path");
        String[] parts = cp.split(java.io.File.pathSeparator);
        URL[] urls = new URL[parts.length];
        for (int i = 0; i < parts.length; i++) {
            urls[i] = new java.io.File(parts[i]).toURI().toURL();
        }

        Set<String> excluded = new HashSet<>();
        excluded.add("com.fasterxml.jackson.databind");
        excluded.add("com.fasterxml.jackson.core");

        ModifiedClassPathClassLoader loader = new ModifiedClassPathClassLoader(
                urls, excluded, system, system);

        String[] classNames = {
                "java.lang.String",
                "java.util.ArrayList",
                "java.util.HashMap",
                "org.springframework.core.io.Resource",
                "org.springframework.context.ApplicationContext",
                "org.springframework.util.ClassUtils",
                "com.fasterxml.jackson.databind.ObjectMapper",
                "com.fasterxml.jackson.core.JsonFactory",
                "org.junit.jupiter.api.Test",
                "org.hamcrest.Matchers",
        };

        AtomicInteger failures = new AtomicInteger(0);
        for (int i = 0; i < iterations; i++) {
            String name = classNames[i % classNames.length];
            boolean shouldBeExcluded = name.startsWith("com.fasterxml.jackson.databind")
                    || name.startsWith("com.fasterxml.jackson.core");
            try {
                Class<?> c = loader.loadClass(name);
                if (shouldBeExcluded) {
                    failures.incrementAndGet();
                    if (failures.get() <= 5) {
                        System.out.println("EXPECTED-EXCLUSION-MISS at i=" + i + " name=" + name
                                + " loaded=" + c);
                    }
                } else if (c == null || !c.getName().equals(name)) {
                    failures.incrementAndGet();
                    if (failures.get() <= 5) {
                        System.out.println("WRONG-CLASS at i=" + i + " expected=" + name
                                + " got=" + (c == null ? "null" : c.getName()));
                    }
                }
            } catch (ClassNotFoundException e) {
                if (!shouldBeExcluded) {
                    failures.incrementAndGet();
                    if (failures.get() <= 5) {
                        System.out.println("UNEXPECTED-CNFE at i=" + i + " name=" + name);
                    }
                }
            }

            if (i % 2000 == 0) {
                System.out.println("progress i=" + i);
                System.out.flush();
            }
        }
        System.out.println("DONE iterations=" + iterations + " failures=" + failures.get());
        if (failures.get() > 0) {
            System.exit(1);
        }
    }
}
