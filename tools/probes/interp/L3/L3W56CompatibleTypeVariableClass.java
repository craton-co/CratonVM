// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L3 --
// docs/internal/fixed-bugs/interpreter-L3-the-compatible-type-variable-is-an-instance-of-the-interface-FIXED-20261020.md:
// `--compatible`'s `getTypeParameters()` handed out the VM's own carrier, an
// instance of the INTERFACE `java.lang.reflect.TypeVariable`, where HotSpot
// (and `--jdk-only`) hand out `sun.reflect.generics.reflectiveObjects.TypeVariableImpl`.
// Under `CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS` the natives build the JDK's
// class (`generics::type_param_to_java`).
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   class-*      -- the type variable's class, from a class, a method and a
//                   constructor declaration;
//   same-*       -- identity / equality across two `getTypeParameters()` calls,
//                   and with the `T` a generic field type names;
//   bounds-*     -- `getBounds()` of an unbounded and of an intersection-bounded
//                   variable, and of a variable whose bound names an earlier one;
//   decl, name   -- `getGenericDeclaration()` / `getName()` / `toString()`;
//   hash         -- `hashCode()` against HotSpot's formula;
//   annotated    -- `getAnnotatedBounds()`'s element classes;
//   raise        -- `getAnnotation(null)`'s frames above the probe;
//   instanceof   -- `instanceof TypeVariable`, and the class's interfaces;
//   bounds-copy  -- a store into `getBounds()`'s / `getUpperBounds()`'s answer
//                   does not reach the next answer (the JDK hands out a clone).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// class-class: sun.reflect.generics.reflectiveObjects.TypeVariableImpl
// class-method: sun.reflect.generics.reflectiveObjects.TypeVariableImpl
// class-ctor: sun.reflect.generics.reflectiveObjects.TypeVariableImpl
// same-calls: identical=true equal=true
// same-field: identical=true equal=true
// bounds-T: [class java.lang.Object]
// bounds-U: [class java.lang.Number, java.lang.Comparable<U>]
// bounds-V: [T]
// decl: class L3W56CompatibleTypeVariableClass
// name: T T
// hash: true
// annotated: [sun.reflect.annotation.AnnotatedTypeFactory$AnnotatedTypeBaseImpl, sun.reflect.annotation.AnnotatedTypeFactory$AnnotatedParameterizedTypeImpl]
// raise: java.lang.NullPointerException null [java.util.Objects.requireNonNull sun.reflect.generics.reflectiveObjects.TypeVariableImpl.getAnnotation] L3W56CompatibleTypeVariableClass.main
// instanceof: true [interface java.lang.reflect.TypeVariable]
// bounds-copy: class java.lang.Number class java.lang.Number
// class-class-again: sun.reflect.generics.reflectiveObjects.TypeVariableImpl
// HOTSPOT_EXPECTED_END
//
// CratonVM before wave 56 (`--compatible`): every `class-*` row printed
// `java.lang.reflect.TypeVariable`, the `raise` row printed `[]`, and the
// `instanceof` row listed the interface's own superinterfaces.
// Positive control: `CRATONVM_FRAME_TRACE=1` prints
//     [TYPE_VARIABLE_IMPL] built sun/reflect/generics/reflectiveObjects/TypeVariableImpl T
// for the first row (`--compatible`, the switch on).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W56CompatibleTypeVariableClass`.
import java.lang.reflect.AnnotatedType;
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.lang.reflect.ParameterizedType;
import java.lang.reflect.Type;
import java.lang.reflect.TypeVariable;
import java.lang.reflect.WildcardType;
import java.util.Arrays;
import java.util.List;

public class L3W56CompatibleTypeVariableClass<T, U extends Number & Comparable<U>, V extends T> {
    public List<T> items;
    public List<? extends Number> nums;

    public <M> L3W56CompatibleTypeVariableClass() {
    }

    public <M extends Runnable> M pick(M m) {
        return m;
    }

    static String names(Object[] xs) {
        StringBuilder b = new StringBuilder("[");
        for (int i = 0; i < xs.length; i++) {
            b.append(i == 0 ? "" : ", ").append(xs[i].getClass().getName());
        }
        return b.append(']').toString();
    }

    public static void main(String[] args) throws Exception {
        Class<?> c = L3W56CompatibleTypeVariableClass.class;
        System.out.println("class-class: " + c.getTypeParameters()[0].getClass().getName());
        Method pick = c.getMethod("pick", Runnable.class);
        System.out.println("class-method: " + pick.getTypeParameters()[0].getClass().getName());
        Constructor<?> ctor = c.getConstructor();
        System.out.println("class-ctor: " + ctor.getTypeParameters()[0].getClass().getName());

        TypeVariable<?> t1 = c.getTypeParameters()[0];
        TypeVariable<?> t2 = c.getTypeParameters()[0];
        System.out.println("same-calls: identical=" + (t1 == t2) + " equal=" + t1.equals(t2));
        Type fieldArg = ((ParameterizedType) c.getField("items").getGenericType()).getActualTypeArguments()[0];
        System.out.println("same-field: identical=" + (fieldArg == t1) + " equal=" + t1.equals(fieldArg));

        TypeVariable<?>[] tps = c.getTypeParameters();
        System.out.println("bounds-T: " + Arrays.toString(tps[0].getBounds()));
        System.out.println("bounds-U: " + Arrays.toString(tps[1].getBounds()));
        System.out.println("bounds-V: " + Arrays.toString(tps[2].getBounds()));
        System.out.println("decl: " + t1.getGenericDeclaration());
        System.out.println("name: " + t1.getName() + " " + t1);
        System.out.println("hash: " + (t1.hashCode() == (c.hashCode() ^ "T".hashCode())));
        AnnotatedType[] ab = tps[1].getAnnotatedBounds();
        System.out.println("annotated: " + names(ab));
        try {
            t1.getAnnotation(null);
            System.out.println("raise: no exception");
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            int k = 0;
            while (k < st.length && !st[k].getClassName().startsWith("L3W56")) {
                jdk.append(k == 0 ? "" : " ").append(st[k].getClassName()).append('.').append(st[k].getMethodName());
                k++;
            }
            String first = k < st.length ? st[k].getClassName() + "." + st[k].getMethodName() : "-";
            System.out.println("raise: " + t.getClass().getName() + " " + t.getMessage() + " [" + jdk + "] " + first);
        }
        Object o = t1;
        System.out.println("instanceof: " + (o instanceof TypeVariable) + " " + Arrays.toString(o.getClass().getInterfaces()));
        tps[1].getBounds()[0] = String.class;
        WildcardType w = (WildcardType) ((ParameterizedType) c.getField("nums").getGenericType()).getActualTypeArguments()[0];
        w.getUpperBounds();
        w.getUpperBounds()[0] = String.class;
        System.out.println("bounds-copy: " + tps[1].getBounds()[0] + " " + w.getUpperBounds()[0]);
        System.out.println("class-class-again: " + c.getTypeParameters()[0].getClass().getName());
    }
}
