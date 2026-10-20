// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L3 --
// docs/internal/fixed-bugs/interpreter-i57-L3-proposal-compatible-declaration-annotations-run-the-jdks-parser-DONE-20261014.md:
// declaration annotations of a class and its members, where `--compatible`
// answered from natives and `--jdk-only` from the JDK's `AnnotationParser`.
// Rows: identity across doors (`equals`, `hashCode`, `toString` of the same
// annotation reached through a class, a method, a field, a type annotation),
// `@Inherited`, repeatable containers, an annotation's default values
// (`getDefaultValue`), parameter annotations of an inner class's constructor
// (one implicit parameter javac gives no annotation slot), an enum constant's
// field, and the order of `getAnnotations()`. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// class: [@L3W58DeclarationAnnotations.V(n=1, types={int.class}, s="cls"), @L3W58DeclarationAnnotations.Inh("base")]
// same-method: true true true
// same-field: true true
// same-type-use: true
// inherited: [@L3W58DeclarationAnnotations.Inh("base")] []
// repeatable: [@L3W58DeclarationAnnotations.R("a"), @L3W58DeclarationAnnotations.R("b")] 1
// default: 1 dflt [I]
// inner-ctor-params: 2 [[], [@L3W58DeclarationAnnotations.P("q")]]
// static-method-params: [[@L3W58DeclarationAnnotations.P("x")], []]
// enum-field: [@L3W58DeclarationAnnotations.V(n=3, types={int.class}, s="e")]
// proxy-class: true true
// method-ref: 2 1
// class-again: [@L3W58DeclarationAnnotations.V(n=1, types={int.class}, s="cls"), @L3W58DeclarationAnnotations.Inh("base")]
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W58DeclarationAnnotations`.
import java.lang.annotation.ElementType;
import java.lang.annotation.Inherited;
import java.lang.annotation.Repeatable;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.Proxy;
import java.util.Arrays;

public class L3W58DeclarationAnnotations {
    @Retention(RetentionPolicy.RUNTIME)
    @Target({ElementType.TYPE, ElementType.METHOD, ElementType.FIELD, ElementType.TYPE_USE})
    @interface V {
        int n() default 1;

        String s() default "dflt";

        Class<?>[] types() default {int.class};
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Inherited
    @interface Inh {
        String value();
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Repeatable(Rs.class)
    @interface R {
        String value();
    }

    @Retention(RetentionPolicy.RUNTIME)
    @interface Rs {
        R[] value();
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.PARAMETER)
    @interface P {
        String value();
    }

    @V(s = "cls")
    @Inh("base")
    static class Base {
        @V(s = "cls")
        void m() {
        }

        @V(s = "cls")
        int f;

        @V(s = "cls") String g;

        @R("a")
        @R("b")
        void rep() {
        }

        static void sm(@P("x") int a, int b) {
        }
    }

    static class Sub extends Base {
    }

    class Inner {
        Inner(@P("q") String q) {
        }
    }

    enum E {
        @V(n = 3, s = "e")
        ONE
    }

    public static void main(String[] args) throws Exception {
        System.out.println("class: " + Arrays.toString(Base.class.getAnnotations()));
        V onClass = Base.class.getAnnotation(V.class);
        V onMethod = Base.class.getDeclaredMethod("m").getAnnotation(V.class);
        System.out.println("same-method: " + onClass.equals(onMethod) + " " + onMethod.equals(onClass) + " "
                + (onClass.hashCode() == onMethod.hashCode()));
        V onField = Base.class.getDeclaredField("f").getAnnotation(V.class);
        System.out.println("same-field: " + onField.equals(onClass) + " " + onField.toString().equals(onClass.toString()));
        V onTypeUse = Base.class.getDeclaredField("g").getAnnotatedType().getAnnotation(V.class);
        System.out.println("same-type-use: " + onClass.equals(onTypeUse));
        System.out.println("inherited: " + Arrays.toString(Sub.class.getAnnotationsByType(Inh.class)) + " "
                + Arrays.toString(Sub.class.getDeclaredAnnotations()));
        System.out.println("repeatable: " + Arrays.toString(Base.class.getDeclaredMethod("rep").getAnnotationsByType(R.class))
                + " " + Base.class.getDeclaredMethod("rep").getAnnotations().length);
        Object n = V.class.getDeclaredMethod("n").getDefaultValue();
        Object s = V.class.getDeclaredMethod("s").getDefaultValue();
        Class<?>[] t = (Class<?>[]) V.class.getDeclaredMethod("types").getDefaultValue();
        System.out.println("default: " + n + " " + s + " " + Arrays.toString(t).replace("int", "I"));
        System.out.println("inner-ctor-params: " + Inner.class.getDeclaredConstructors()[0].getParameterCount() + " "
                + Arrays.deepToString(Inner.class.getDeclaredConstructors()[0].getParameterAnnotations()));
        System.out.println("static-method-params: " + Arrays.deepToString(
                Base.class.getDeclaredMethod("sm", int.class, int.class).getParameterAnnotations()));
        System.out.println("enum-field: " + Arrays.toString(E.class.getField("ONE").getAnnotations()));
        System.out.println("proxy-class: " + Proxy.isProxyClass(onClass.getClass()) + " "
                + (onClass.annotationType() == V.class));
        java.util.function.Function<Class<?>, java.lang.annotation.Annotation[]> declared = Class::getDeclaredAnnotations;
        java.util.function.Function<java.lang.reflect.Method, java.lang.annotation.Annotation[]> mdeclared =
                java.lang.reflect.Method::getDeclaredAnnotations;
        System.out.println("method-ref: " + declared.apply(Base.class).length + " "
                + mdeclared.apply(Base.class.getDeclaredMethod("m")).length);
        System.out.println("class-again: " + Arrays.toString(Base.class.getAnnotations()));
    }
}
