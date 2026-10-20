// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L3 (review): `TYPE_USE` annotations
// through every `AnnotatedType` door a member offers -- a method's return,
// parameter, exception and receiver types, a constructor's parameter, a
// field's nested type argument and array component, a class's superclass and
// interface -- the guard for `CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS`, which
// hands `--compatible` the type-annotation bytes `TypeVariableImpl` parses.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// return: [@L3W56AtaUse("ret")] arg=[@L3W56AtaUse("ret-arg")]
// param: [@L3W56AtaUse("param")]
// exception: [@L3W56AtaUse("throws")]
// receiver: [@L3W56AtaUse("this")]
// ctor-param: [@L3W56AtaUse("ctor")]
// field-arg: [@L3W56AtaUse("field-arg")]
// field-array: [@L3W56AtaUse("outer")] component=[@L3W56AtaUse("inner")]
// super: [@L3W56AtaUse("super")]
// iface: [@L3W56AtaUse("iface")]
// method-tv-bound: [@L3W56AtaUse("m-bound")]
// return-again: [@L3W56AtaUse("ret")] arg=[@L3W56AtaUse("ret-arg")]
// HOTSPOT_EXPECTED_END
//
// CratonVM (wave 56): see
// docs/internal/fixed-bugs/interpreter-L3-type-annotations-answer-empty-on-several-doors-FIXED-20261021.md
// for the rows that still differ, per mode.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W56AnnotatedTypeAnswers`.
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.AnnotatedArrayType;
import java.lang.reflect.AnnotatedParameterizedType;
import java.lang.reflect.AnnotatedType;
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.List;

public class L3W56AnnotatedTypeAnswers extends @L3W56AtaUse("super") Object
        implements @L3W56AtaUse("iface") Runnable {
    public List<@L3W56AtaUse("field-arg") String> items;
    public @L3W56AtaUse("inner") String @L3W56AtaUse("outer") [] names;

    public L3W56AnnotatedTypeAnswers() {
    }

    public L3W56AnnotatedTypeAnswers(@L3W56AtaUse("ctor") String s) {
    }

    public void run() {
    }

    public @L3W56AtaUse("ret") List<@L3W56AtaUse("ret-arg") String> m(@L3W56AtaUse("this") L3W56AnnotatedTypeAnswers this,
            @L3W56AtaUse("param") int p) throws @L3W56AtaUse("throws") Exception {
        return null;
    }

    public <T extends @L3W56AtaUse("m-bound") Number> void bounded(T t) {
    }

    static String ann(AnnotatedType t) {
        return Arrays.toString(t.getAnnotations());
    }

    static String ret(Method m) {
        AnnotatedType r = m.getAnnotatedReturnType();
        return ann(r) + " arg=" + ann(((AnnotatedParameterizedType) r).getAnnotatedActualTypeArguments()[0]);
    }

    public static void main(String[] args) throws Exception {
        Class<?> c = L3W56AnnotatedTypeAnswers.class;
        Method m = c.getMethod("m", int.class);
        System.out.println("return: " + ret(m));
        System.out.println("param: " + ann(m.getAnnotatedParameterTypes()[0]));
        System.out.println("exception: " + ann(m.getAnnotatedExceptionTypes()[0]));
        System.out.println("receiver: " + ann(m.getAnnotatedReceiverType()));
        Constructor<?> ctor = c.getConstructor(String.class);
        System.out.println("ctor-param: " + ann(ctor.getAnnotatedParameterTypes()[0]));
        System.out.println("field-arg: " + ann(((AnnotatedParameterizedType) c.getField("items").getAnnotatedType())
                .getAnnotatedActualTypeArguments()[0]));
        AnnotatedArrayType arr = (AnnotatedArrayType) c.getField("names").getAnnotatedType();
        System.out.println("field-array: " + ann(arr) + " component=" + ann(arr.getAnnotatedGenericComponentType()));
        System.out.println("super: " + ann(c.getAnnotatedSuperclass()));
        System.out.println("iface: " + ann(c.getAnnotatedInterfaces()[0]));
        Method bounded = c.getMethod("bounded", Number.class);
        System.out.println("method-tv-bound: " + ann(bounded.getTypeParameters()[0].getAnnotatedBounds()[0]));
        System.out.println("return-again: " + ret(m));
    }
}

@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE_USE)
@interface L3W56AtaUse {
    String value();
}
