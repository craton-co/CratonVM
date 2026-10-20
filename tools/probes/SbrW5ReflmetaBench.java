// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * SBR wave 5, lane {@code reflmeta}, measurement 7: the cost of the retired
 * annotation getters (now the JDK's own {@code AnnotationParser} over the real
 * {@code ConstantPool}) and of the retired copy door. ns/op per row, printed
 * per round; run it interleaved against the control binary.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . SbrW5ReflmetaBench [rounds] [iters]
 * </pre>
 */
public class SbrW5ReflmetaBench {
    @Retention(RetentionPolicy.RUNTIME)
    @Target({ElementType.FIELD, ElementType.METHOD, ElementType.PARAMETER})
    @interface Tag {
        String value();

        int n() default 3;
    }

    public static class T {
        @Tag("f")
        public int f;

        @Tag("m")
        public int m(@Tag("p") int a) {
            return a;
        }
    }

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
        Method cm = T.class.getMethod("m", int.class);
        Field cf = T.class.getField("f");
        String[] names = {
            "cached Method.getAnnotation", "cached Field.getAnnotation", "fresh getMethod+getAnnotation",
            "fresh getField+getAnnotation", "cached getParameterAnnotations", "getMethods (copy door)",
            "getDeclaredFields (copy door)", "annotation member call"
        };
        Tag tag = cm.getAnnotation(Tag.class);
        Op[] ops = {
            () -> cm.getAnnotation(Tag.class),
            () -> cf.getAnnotation(Tag.class),
            () -> T.class.getMethod("m", int.class).getAnnotation(Tag.class),
            () -> T.class.getField("f").getAnnotation(Tag.class),
            () -> cm.getParameterAnnotations(),
            () -> T.class.getMethods(),
            () -> T.class.getDeclaredFields(),
            () -> tag.value() + tag.n()
        };
        for (int r = 0; r < rounds; r++) {
            StringBuilder b = new StringBuilder("round " + r);
            for (int i = 0; i < ops.length; i++) b.append(" | ").append(names[i]).append('=').append(time(ops[i], iters));
            System.out.println(b);
        }
    }
}
