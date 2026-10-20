// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * SBR wave 6, lane {@code classrefl}, measurement 7: the cost of the retired
 * {@code java.lang.Class} reflection surface (the JDK's own bytecode over the
 * real {@code ReflectionData} / {@code AnnotationData} caches) against the
 * VM's natives. ns/op per row, printed per round; run it interleaved against
 * the control binary.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . SbrW6ClassreflBench [rounds] [iters]
 * </pre>
 */
public class SbrW6ClassreflBench {
    @Retention(RetentionPolicy.RUNTIME)
    @Target({ElementType.FIELD, ElementType.METHOD, ElementType.PARAMETER, ElementType.TYPE})
    @interface Tag {
        String value();

        int n() default 3;
    }

    @Tag("c")
    public static class T {
        @Tag("f")
        public int f;
        private int g;

        public T() {}

        public T(int a) {}

        @Tag("m")
        public int m(int a) {
            return a;
        }

        public void other() {}

        public static void stat() {}
    }

    public static class Big {
        public int f0, f1, f2, f3, f4, f5, f6, f7, f8, f9;

        public void m0() {}
        public void m1() {}
        public void m2() {}
        public void m3() {}
        public void m4() {}
        public void m5() {}
        public void m6() {}
        public void m7() {}
        public void m8() {}
        public void m9() {}
    }

    public static class Nest {
        public class Inner {}
    }

    enum E { A, B, C }

    static volatile Object sink;

    interface Op {
        Object run() throws Exception;
    }

    static long time(Op op, int iters) throws Exception {
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) sink = op.run();
        return (System.nanoTime() - t0) / iters;
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        final Class<?> tc = T.class;
        final Class<?> inner = Nest.Inner.class;
        final Class<?> arr = String[][].class;
        final Class<?> shared = E.class;
        String[] names = {
            "getMethod", "getField", "getDeclaredField", "getConstructor()", "getDeclaredConstructor(int)",
            "getDeclaredFields (10)", "getSimpleName (nested)", "getCanonicalName (nested)", "getTypeName (array)",
            "descriptorString (array)", "Class.getAnnotation", "Class.isAnnotationPresent", "Class.getAnnotations",
            "fresh getMethod+Method.getAnnotation", "fresh getField+Field.getAnnotation", "Enum.valueOf",
            "getEnumConstants", "isPrimitive+isInterface+getModifiers", "newInstance", "getMethod (miss, NSME)",
            "getMethod m9 on a 10-method class"
        };
        Op[] ops = {
            () -> tc.getMethod("m", int.class),
            () -> tc.getField("f"),
            () -> tc.getDeclaredField("g"),
            () -> tc.getConstructor(),
            () -> tc.getDeclaredConstructor(int.class),
            () -> Big.class.getDeclaredFields(),
            () -> inner.getSimpleName(),
            () -> inner.getCanonicalName(),
            () -> arr.getTypeName(),
            () -> arr.descriptorString(),
            () -> tc.getAnnotation(Tag.class),
            () -> tc.isAnnotationPresent(Tag.class),
            () -> tc.getAnnotations(),
            () -> tc.getMethod("m", int.class).getAnnotation(Tag.class),
            () -> tc.getField("f").getAnnotation(Tag.class),
            () -> Enum.valueOf(E.class, "B"),
            () -> shared.getEnumConstants(),
            () -> tc.isPrimitive() ? 1 : tc.isInterface() ? 2 : tc.getModifiers(),
            () -> T.class.newInstance(),
            () -> {
                try {
                    return tc.getMethod("nope");
                } catch (NoSuchMethodException e) {
                    return e;
                }
            },
            () -> Big.class.getMethod("m9"),
        };
        for (int r = 0; r < rounds; r++) {
            StringBuilder b = new StringBuilder("round " + r);
            for (int i = 0; i < ops.length; i++) b.append(" | ").append(names[i]).append('=').append(time(ops[i], iters));
            System.out.println(b);
        }
    }
}
