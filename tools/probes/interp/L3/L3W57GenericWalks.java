// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L3 --
// docs/internal/fixed-bugs/interpreter-i56-L3-proposal-compatible-generic-reflection-runs-the-jdks-reifier-DONE-20261014.md:
// the generic walks Spring's `ResolvableType` / `GenericTypeResolver` and
// Jackson's `TypeFactory` make over `java.lang.reflect`, without their jars:
// a type variable resolved through a generic superclass and interface by
// `equals` / `hashCode`, nested owner types, wildcards, generic arrays,
// method type variables, the carriers' class names, and which answers are
// the SAME object on a second call (the JDK caches its repositories).
// `CRATONVM_COMPATIBLE_JDK_REIFIER` must keep every row; the walk is
// repeated hot (no trace is printed after it).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// sub-T: class java.lang.String
// repo: L3W57GenericWalks$User java.lang.Long
// tv-equal: true true decl=class L3W57GenericWalks$Base bounds=[class java.lang.Object]
// tv-same: true array-same=false
// field-same: true other-field-equal=true
// super-same: true equal=true
// field-items: java.util.List<T> | List<TV(T)> | sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl
// field-arr: T[] | TV(T)[] | sun.reflect.generics.reflectiveObjects.GenericArrayTypeImpl
// field-wild: java.util.Map<java.lang.String, ? extends T> | Map<String,? extends TV(T)> | sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl
// field-sink: java.util.List<? super T> | List<? super TV(T)> | sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl
// holder-nested: java.util.Map<java.lang.String, java.util.List<java.lang.Integer>> | Map<String,List<Integer>> | sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl
// holder-owned: L3W57GenericWalks$Outer<java.lang.String>$Inner<java.lang.Integer> | Inner<Integer>@Outer<String>@L3W57GenericWalks | sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl
// holder-arrays: java.util.List<int[]>[] | List<int[]>[] | sun.reflect.generics.reflectiveObjects.GenericArrayTypeImpl
// method-tv: R bounds=[java.lang.Comparable<? super R>] decl=pick ret-equal=true
// method-params: [java.util.List<? super T>, R]
// generic-string: public <R extends java.lang.Comparable<? super R>> R L3W57GenericWalks$Base.pick(java.util.List<? super T>,R)
// ifaces: [L3W57GenericWalks$Repo<L3W57GenericWalks$User, java.lang.Long>] sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl
// jdk-classes: [java.util.List<E>, interface java.util.RandomAccess, interface java.lang.Cloneable, interface java.io.Serializable] super=java.util.AbstractList<E>
// hashmap-tv: [K, V] sun.reflect.generics.reflectiveObjects.TypeVariableImpl
// walk-total: 84000
// sub-T-again: class java.lang.String
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W57GenericWalks`.
import java.lang.reflect.Field;
import java.lang.reflect.GenericArrayType;
import java.lang.reflect.Method;
import java.lang.reflect.ParameterizedType;
import java.lang.reflect.Type;
import java.lang.reflect.TypeVariable;
import java.lang.reflect.WildcardType;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L3W57GenericWalks {
    public interface Repo<T, ID> {
        T find(ID id);
    }

    public static class User {
    }

    public static class UserRepo implements Repo<User, Long> {
        public User find(Long id) {
            return null;
        }
    }

    public abstract static class Base<T> {
        public List<T> items;
        public T[] arr;
        public Map<String, ? extends T> wild;
        public List<? super T> sink;

        public <R extends Comparable<? super R>> R pick(List<? super T> in, R r) {
            return r;
        }
    }

    public static class Sub extends Base<String> {
    }

    public static class Outer<O> {
        public class Inner<I> {
        }

        public Inner<Integer> inner;
    }

    public static class Holder {
        public Map<String, List<Integer>> nested;
        public Outer<String>.Inner<Integer> owned;
        public List<int[]>[] arrays;
    }

    /** Spring's `GenericTypeResolver.getTypeVariableMap`, in miniature. */
    static Map<TypeVariable<?>, Type> variables(Class<?> c) {
        Map<TypeVariable<?>, Type> map = new HashMap<>();
        for (Class<?> k = c; k != null && k != Object.class; k = k.getSuperclass()) {
            collect(k.getGenericSuperclass(), map);
            for (Type i : k.getGenericInterfaces()) {
                collect(i, map);
            }
        }
        return map;
    }

    static void collect(Type t, Map<TypeVariable<?>, Type> map) {
        if (t instanceof ParameterizedType p) {
            TypeVariable<?>[] vars = ((Class<?>) p.getRawType()).getTypeParameters();
            Type[] args = p.getActualTypeArguments();
            for (int i = 0; i < vars.length; i++) {
                map.put(vars[i], args[i]);
            }
        }
    }

    /** Jackson's `TypeFactory._fromAny`, in miniature. */
    static String describe(Type t) {
        if (t instanceof Class<?> c) {
            return c.isArray() ? describe(c.getComponentType()) + "[]" : c.getSimpleName();
        }
        if (t instanceof ParameterizedType p) {
            StringBuilder b = new StringBuilder(describe(p.getRawType())).append('<');
            Type[] args = p.getActualTypeArguments();
            for (int i = 0; i < args.length; i++) {
                b.append(i == 0 ? "" : ",").append(describe(args[i]));
            }
            String owner = p.getOwnerType() == null ? "" : "@" + describe(p.getOwnerType());
            return b.append('>').append(owner).toString();
        }
        if (t instanceof WildcardType w) {
            return "?" + (w.getLowerBounds().length > 0 ? " super " + describe(w.getLowerBounds()[0])
                    : " extends " + describe(w.getUpperBounds()[0]));
        }
        if (t instanceof GenericArrayType g) {
            return describe(g.getGenericComponentType()) + "[]";
        }
        if (t instanceof TypeVariable<?> v) {
            return "TV(" + v.getName() + ")";
        }
        return "?" + t;
    }

    static String kind(Type t) {
        return t.getClass().getName();
    }

    static long walk() throws Exception {
        long n = 0;
        Map<TypeVariable<?>, Type> m = variables(Sub.class);
        TypeVariable<?> t = Base.class.getTypeParameters()[0];
        n += m.get(t) == String.class ? 1 : 0;
        Type items = Base.class.getField("items").getGenericType();
        n += ((ParameterizedType) items).getActualTypeArguments()[0].equals(t) ? 1 : 0;
        Map<TypeVariable<?>, Type> r = variables(UserRepo.class);
        n += r.get(Repo.class.getTypeParameters()[1]) == Long.class ? 1 : 0;
        n += describe(Holder.class.getField("nested").getGenericType()).length();
        return n;
    }

    public static void main(String[] args) throws Exception {
        Map<TypeVariable<?>, Type> m = variables(Sub.class);
        TypeVariable<?> t = Base.class.getTypeParameters()[0];
        System.out.println("sub-T: " + m.get(t));
        Map<TypeVariable<?>, Type> r = variables(UserRepo.class);
        TypeVariable<?>[] rv = Repo.class.getTypeParameters();
        System.out.println("repo: " + r.get(rv[0]).getTypeName() + " " + r.get(rv[1]).getTypeName());
        Type items = Base.class.getField("items").getGenericType();
        TypeVariable<?> fromField = (TypeVariable<?>) ((ParameterizedType) items).getActualTypeArguments()[0];
        System.out.println("tv-equal: " + fromField.equals(t) + " " + (fromField.hashCode() == t.hashCode())
                + " decl=" + fromField.getGenericDeclaration() + " bounds=" + Arrays.toString(fromField.getBounds()));
        System.out.println("tv-same: " + (Base.class.getTypeParameters()[0] == t)
                + " array-same=" + (Base.class.getTypeParameters() == Base.class.getTypeParameters()));
        Field itemsField = Base.class.getField("items");
        System.out.println("field-same: " + (itemsField.getGenericType() == itemsField.getGenericType())
                + " other-field-equal=" + Base.class.getField("items").getGenericType().equals(items));
        System.out.println("super-same: " + (Sub.class.getGenericSuperclass() == Sub.class.getGenericSuperclass())
                + " equal=" + Sub.class.getGenericSuperclass().equals(Sub.class.getGenericSuperclass()));
        for (String f : new String[] {"items", "arr", "wild", "sink"}) {
            Type g = Base.class.getField(f).getGenericType();
            System.out.println("field-" + f + ": " + g + " | " + describe(g) + " | " + kind(g));
        }
        for (String f : new String[] {"nested", "owned", "arrays"}) {
            Type g = Holder.class.getField(f).getGenericType();
            System.out.println("holder-" + f + ": " + g.getTypeName() + " | " + describe(g) + " | " + kind(g));
        }
        Method pick = Base.class.getMethod("pick", List.class, Comparable.class);
        TypeVariable<Method> rvar = pick.getTypeParameters()[0];
        System.out.println("method-tv: " + rvar + " bounds=" + Arrays.toString(rvar.getBounds())
                + " decl=" + rvar.getGenericDeclaration().getName() + " ret-equal=" + pick.getGenericReturnType().equals(rvar));
        System.out.println("method-params: " + Arrays.toString(pick.getGenericParameterTypes()));
        System.out.println("generic-string: " + pick.toGenericString());
        System.out.println("ifaces: " + Arrays.toString(UserRepo.class.getGenericInterfaces()) + " "
                + kind(UserRepo.class.getGenericInterfaces()[0]));
        System.out.println("jdk-classes: " + Arrays.toString(java.util.ArrayList.class.getGenericInterfaces())
                + " super=" + java.util.ArrayList.class.getGenericSuperclass());
        System.out.println("hashmap-tv: " + Arrays.toString(HashMap.class.getTypeParameters())
                + " " + kind(HashMap.class.getTypeParameters()[0]));
        long total = 0;
        for (int i = 0; i < 3000; i++) {
            total += walk();
        }
        System.out.println("walk-total: " + total);
        System.out.println("sub-T-again: " + variables(Sub.class).get(Base.class.getTypeParameters()[0]));
    }
}
