// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L3 (review): a type parameter's own
// annotations (`@Target(TYPE_PARAMETER)`, `RUNTIME`), through
// `TypeVariable.getAnnotations()` / `getAnnotation` / `getDeclaredAnnotations`
// / `getAnnotationsByType`, for a class's and a method's type parameter, and an
// annotated bound through `getAnnotatedBounds()`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// class-annotations: [@L3W56TypeParameterAnnotations.Tag("cls")]
// class-get: @L3W56TypeParameterAnnotations.Tag("cls")
// class-declared: 1
// class-by-type: 1
// class-unannotated: []
// method-annotations: [@L3W56TypeParameterAnnotations.Tag("meth")]
// bound-annotations: [@L3W56TypeParameterAnnotations.Use("bound")]
// class-annotations-again: [@L3W56TypeParameterAnnotations.Tag("cls")]
// HOTSPOT_EXPECTED_END
//
// CratonVM (wave 56): see
// docs/internal/fixed-bugs/interpreter-L3-type-annotations-answer-empty-on-several-doors-FIXED-20261021.md
// for the rows that still differ, per mode.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W56TypeParameterAnnotations`.
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.Method;
import java.lang.reflect.TypeVariable;
import java.util.Arrays;

public class L3W56TypeParameterAnnotations<@L3W56TypeParameterAnnotations.Tag("cls") T, U extends @L3W56TypeParameterAnnotations.Use("bound") Number> {
    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE_PARAMETER)
    @interface Tag {
        String value();
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE_USE)
    @interface Use {
        String value();
    }

    public <@Tag("meth") M> void pick(M m) {
    }

    public static void main(String[] args) throws Exception {
        Class<?> c = L3W56TypeParameterAnnotations.class;
        TypeVariable<?> t = c.getTypeParameters()[0];
        System.out.println("class-annotations: " + Arrays.toString(t.getAnnotations()));
        System.out.println("class-get: " + t.getAnnotation(Tag.class));
        System.out.println("class-declared: " + t.getDeclaredAnnotations().length);
        System.out.println("class-by-type: " + t.getAnnotationsByType(Tag.class).length);
        System.out.println("class-unannotated: " + Arrays.toString(c.getTypeParameters()[1].getAnnotations()));
        Method pick = c.getMethod("pick", Object.class);
        System.out.println("method-annotations: " + Arrays.toString(pick.getTypeParameters()[0].getAnnotations()));
        System.out.println("bound-annotations: "
                + Arrays.toString(c.getTypeParameters()[1].getAnnotatedBounds()[0].getAnnotations()));
        System.out.println("class-annotations-again: " + Arrays.toString(c.getTypeParameters()[0].getAnnotations()));
    }
}
