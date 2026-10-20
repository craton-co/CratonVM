// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L3 (review): the classes of the
// `java.lang.reflect.Type` objects generic reflection hands out, for every
// shape a signature can name, through every member kind. HotSpot hands out
// the JDK's `sun.reflect.generics.reflectiveObjects.*Impl` classes; a VM
// carrier is an instance of the INTERFACE (`java.lang.reflect.WildcardType`,
// ...), whose `getClass()`, `toString()` and `equals` differ.
//
// Each row prints `<member> <shape>: <class> | <toString>`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// field param: sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl | java.util.Map<java.lang.String, ? extends java.lang.Number>
// field wildcard-extends: sun.reflect.generics.reflectiveObjects.WildcardTypeImpl | ? extends java.lang.Number
// field wildcard-super: sun.reflect.generics.reflectiveObjects.WildcardTypeImpl | ? super java.lang.Integer
// field wildcard-any: sun.reflect.generics.reflectiveObjects.WildcardTypeImpl | ?
// field array-of-var: sun.reflect.generics.reflectiveObjects.GenericArrayTypeImpl | T[]
// field array-of-param: sun.reflect.generics.reflectiveObjects.GenericArrayTypeImpl | java.util.List<java.lang.String>[]
// method return: sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl | java.util.List<? super T>
// method return-arg: sun.reflect.generics.reflectiveObjects.WildcardTypeImpl | ? super T
// method param: sun.reflect.generics.reflectiveObjects.GenericArrayTypeImpl | java.util.List<?>[]
// class super: sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl | java.util.ArrayList<java.util.Map<T, ?>>
// class super-arg-arg: sun.reflect.generics.reflectiveObjects.WildcardTypeImpl | ?
// class interface: sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl | java.lang.Comparable<L3W56CompatibleGenericTypeClasses<T>>
// wildcard equals: true true
// field param-again: sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl | java.util.Map<java.lang.String, ? extends java.lang.Number>
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W56CompatibleGenericTypeClasses`.
import java.lang.reflect.GenericArrayType;
import java.lang.reflect.ParameterizedType;
import java.lang.reflect.Type;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

public class L3W56CompatibleGenericTypeClasses<T> extends ArrayList<Map<T, ?>>
        implements Comparable<L3W56CompatibleGenericTypeClasses<T>> {
    public Map<String, ? extends Number> param;
    public List<? super Integer> sup;
    public List<?> any;
    public T[] arrayOfVar;
    public List<String>[] arrayOfParam;

    public List<? super T> pick(List<?>[] in) {
        return null;
    }

    public int compareTo(L3W56CompatibleGenericTypeClasses<T> o) {
        return 0;
    }

    static void row(String name, Type t) {
        System.out.println(name + ": " + t.getClass().getName() + " | " + t);
    }

    static Type arg(Type t, int i) {
        return ((ParameterizedType) t).getActualTypeArguments()[i];
    }

    public static void main(String[] args) throws Exception {
        Class<?> c = L3W56CompatibleGenericTypeClasses.class;
        Type param = c.getField("param").getGenericType();
        row("field param", param);
        row("field wildcard-extends", arg(param, 1));
        row("field wildcard-super", arg(c.getField("sup").getGenericType(), 0));
        row("field wildcard-any", arg(c.getField("any").getGenericType(), 0));
        row("field array-of-var", c.getField("arrayOfVar").getGenericType());
        row("field array-of-param", c.getField("arrayOfParam").getGenericType());
        java.lang.reflect.Method pick = c.getMethod("pick", List[].class);
        row("method return", pick.getGenericReturnType());
        row("method return-arg", arg(pick.getGenericReturnType(), 0));
        row("method param", pick.getGenericParameterTypes()[0]);
        Type sup = c.getGenericSuperclass();
        row("class super", sup);
        row("class super-arg-arg", arg(arg(sup, 0), 1));
        row("class interface", c.getGenericInterfaces()[0]);
        Type w1 = arg(c.getField("any").getGenericType(), 0);
        Type w2 = arg(c.getField("any").getGenericType(), 0);
        System.out.println("wildcard equals: " + w1.equals(w2) + " " + (w1.hashCode() == w2.hashCode()));
        row("field param-again", c.getField("param").getGenericType());
    }
}
