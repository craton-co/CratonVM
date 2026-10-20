// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L3 (review): generic-signature
// reflection's less travelled answers -- owner types, exception types,
// `toGenericString()`, a type variable's bounds through a method, a raw
// supertype, `getTypeName()` of nested shapes, a recursive bound, and the
// equality / hash of two independently reified parameterized types.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// owner: L3W56GenericSignatureEdges$Outer<java.lang.String>$Inner<java.lang.Integer> owner=L3W56GenericSignatureEdges$Outer<java.lang.String> raw=class L3W56GenericSignatureEdges$Outer$Inner
// exceptions: [E, class java.io.IOException]
// to-generic-method: public <E extends java.lang.Exception> java.util.List<? extends java.lang.Number> L3W56GenericSignatureEdges.risky(java.util.Map<java.lang.String, E>) throws E,java.io.IOException
// to-generic-class: public class L3W56GenericSignatureEdges<K extends java.lang.Comparable<K>>
// to-generic-field: public java.util.Map<K, java.util.List<K>[]> L3W56GenericSignatureEdges.map
// method-var-bounds: [class java.lang.Exception]
// raw-super: class java.lang.Object
// raw-iface: [interface java.lang.Runnable]
// recursive: K [java.lang.Comparable<K>] decl=class L3W56GenericSignatureEdges
// param-equal: true true
// param-not-same: false
// owner-again: L3W56GenericSignatureEdges$Outer<java.lang.String>$Inner<java.lang.Integer> owner=L3W56GenericSignatureEdges$Outer<java.lang.String> raw=class L3W56GenericSignatureEdges$Outer$Inner
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W56GenericSignatureEdges`.
import java.io.IOException;
import java.lang.reflect.Method;
import java.lang.reflect.ParameterizedType;
import java.lang.reflect.Type;
import java.lang.reflect.TypeVariable;
import java.util.Arrays;
import java.util.List;
import java.util.Map;

public class L3W56GenericSignatureEdges<K extends Comparable<K>> {
    static class Outer<T> {
        class Inner<U> {
        }
    }

    static class RawRunner implements Runnable {
        public void run() {
        }
    }

    public Outer<String>.Inner<Integer> nested;
    public Map<K, List<K>[]> map;
    public Map<K, List<K>[]> map2;

    public <E extends Exception> List<? extends Number> risky(Map<String, E> in) throws E, IOException {
        return null;
    }

    static String owner(Type t) {
        ParameterizedType p = (ParameterizedType) t;
        return p.getTypeName() + " owner=" + p.getOwnerType().getTypeName() + " raw=" + p.getRawType();
    }

    public static void main(String[] args) throws Exception {
        Class<?> c = L3W56GenericSignatureEdges.class;
        System.out.println("owner: " + owner(c.getField("nested").getGenericType()));
        Method risky = c.getMethod("risky", Map.class);
        System.out.println("exceptions: " + Arrays.toString(risky.getGenericExceptionTypes()));
        System.out.println("to-generic-method: " + risky.toGenericString());
        System.out.println("to-generic-class: " + c.toGenericString());
        System.out.println("to-generic-field: " + c.getField("map").toGenericString());
        System.out.println("method-var-bounds: " + Arrays.toString(risky.getTypeParameters()[0].getBounds()));
        System.out.println("raw-super: " + RawRunner.class.getGenericSuperclass());
        System.out.println("raw-iface: " + Arrays.toString(RawRunner.class.getGenericInterfaces()));
        TypeVariable<?> k = c.getTypeParameters()[0];
        System.out.println("recursive: " + k + " " + Arrays.toString(k.getBounds()) + " decl=" + k.getGenericDeclaration());
        Type m1 = c.getField("map").getGenericType();
        Type m2 = c.getField("map2").getGenericType();
        System.out.println("param-equal: " + m1.equals(m2) + " " + (m1.hashCode() == m2.hashCode()));
        System.out.println("param-not-same: " + (m1 == m2));
        System.out.println("owner-again: " + owner(c.getField("nested").getGenericType()));
    }
}
