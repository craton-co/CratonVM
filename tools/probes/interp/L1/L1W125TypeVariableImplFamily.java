// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 125, lane L1 -- the flip-rule probe and bench for
// the `JdkTypeVariableImpl` compat family (alignment bit 98,
// `CRATONVM_COMPAT_JDK_TYPE_VARIABLE_IMPL`, default off): with the row on,
// `--compatible` runs the JDK's own bodies of
// `sun.reflect.generics.reflectiveObjects.TypeVariableImpl` (getName,
// getBounds, toString, getTypeName through Type's default, getAnnotatedBounds
// and the six annotation getters), as `--jdk-only` does. Page:
// docs/internal/proposals/interpreter/i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows-20261107.md.
//
// Covers: type variables of a class, a method, a constructor and an inner
// class, reached through getTypeParameters, a field's generic type, a generic
// return type, a parameterized type's arguments and a generic superclass;
// their class, name, toString, getTypeName, bounds (and that the bounds array
// is a copy), declaration, equals / hashCode across lookups; the annotation
// getters on an annotated and an unannotated variable (present, absent, by
// type, repeatable, null argument with the NPE's top frame); annotated bounds;
// getBounds / getTypeName called reflectively and through a Proxy that
// forwards by Method.invoke (the Spring SerializableTypeWrapper shape);
// string concatenation and a Map key. TIME lines: getName, getBounds,
// toString, getAnnotations on one variable.
//
// Must be SAME in all three arms (TIME lines filtered):
//   javac -d out L1W125TypeVariableImplFamily.java
//   java -cp out L1W125TypeVariableImplFamily
//   cratonvm --jdk-only -cp out L1W125TypeVariableImplFamily
//   cratonvm --compatible -cp out L1W125TypeVariableImplFamily
//   CRATONVM_COMPAT_JDK_TYPE_VARIABLE_IMPL=1 cratonvm --compatible -cp out L1W125TypeVariableImplFamily
//   (each also with --nojit for the interpreter's TIME rows)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint identical; TIME lines filtered)
// G params=[T, U, V] arrayClass=[Ljava.lang.reflect.TypeVariable;
// G.T class=sun.reflect.generics.reflectiveObjects.TypeVariableImpl
// G.T name=T toString=T typeName=T
// G.T bounds=[java.lang.Number, java.lang.Comparable<T>] arrayClass=[Ljava.lang.reflect.Type;
// G.T boundsAfterStore=[java.lang.Number, java.lang.Comparable<T>] copy=true
// G.T decl=class G
// G.T annotatedBounds=[java.lang.Number[], java.lang.Comparable<T>[]]
// G.T annotations=[] declared=[]
// G.T getAnnotation(Tp)=null getAnnotation(Tu)=null getDeclaredAnnotation(Tp)=null
// G.T byType(Rep)=[] declaredByType(Rep)=[] byType(Tu)=[] byTypeArrayClass=[LL1W125TypeVariableImplFamily$Tp;
// G.T isAnnotationPresent(Tp)=false
// G.T getAnnotation(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// G.T getDeclaredAnnotation(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// G.T getAnnotationsByType(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// G.T getDeclaredAnnotationsByType(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// G.U class=sun.reflect.generics.reflectiveObjects.TypeVariableImpl
// G.U name=U toString=U typeName=U
// G.U bounds=[java.lang.Object] arrayClass=[Ljava.lang.reflect.Type;
// G.U boundsAfterStore=[java.lang.Object] copy=true
// G.U decl=class G
// G.U annotatedBounds=[java.lang.Object[]]
// G.U annotations=[] declared=[]
// G.U getAnnotation(Tp)=null getAnnotation(Tu)=null getDeclaredAnnotation(Tp)=null
// G.U byType(Rep)=[] declaredByType(Rep)=[] byType(Tu)=[] byTypeArrayClass=[LL1W125TypeVariableImplFamily$Tp;
// G.U isAnnotationPresent(Tp)=false
// G.U getAnnotation(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// G.U getDeclaredAnnotation(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// G.U getAnnotationsByType(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// G.U getDeclaredAnnotationsByType(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// G.V class=sun.reflect.generics.reflectiveObjects.TypeVariableImpl
// G.V name=V toString=V typeName=V
// G.V bounds=[java.util.List<? super T>] arrayClass=[Ljava.lang.reflect.Type;
// G.V boundsAfterStore=[java.util.List<? super T>] copy=true
// G.V decl=class G
// G.V annotatedBounds=[java.util.List<? super T>[@Tu(b)]]
// G.V annotations=[@Tp(v), @Reps[@Rep(1), @Rep(2)]] declared=[@Tp(v), @Reps[@Rep(1), @Rep(2)]]
// G.V getAnnotation(Tp)=@Tp(v) getAnnotation(Tu)=null getDeclaredAnnotation(Tp)=@Tp(v)
// G.V byType(Rep)=[@Rep(1), @Rep(2)] declaredByType(Rep)=[@Rep(1), @Rep(2)] byType(Tu)=[] byTypeArrayClass=[LL1W125TypeVariableImplFamily$Tp;
// G.V isAnnotationPresent(Tp)=true
// G.V getAnnotation(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// G.V getDeclaredAnnotation(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// G.V getAnnotationsByType(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// G.V getDeclaredAnnotationsByType(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// pick.M class=sun.reflect.generics.reflectiveObjects.TypeVariableImpl
// pick.M name=M toString=M typeName=M
// pick.M bounds=[java.lang.Runnable] arrayClass=[Ljava.lang.reflect.Type;
// pick.M boundsAfterStore=[java.lang.Runnable] copy=true
// pick.M decl=method pick
// pick.M annotatedBounds=[java.lang.Runnable[]]
// pick.M annotations=[@Tp(m)] declared=[@Tp(m)]
// pick.M getAnnotation(Tp)=@Tp(m) getAnnotation(Tu)=null getDeclaredAnnotation(Tp)=@Tp(m)
// pick.M byType(Rep)=[] declaredByType(Rep)=[] byType(Tu)=[] byTypeArrayClass=[LL1W125TypeVariableImplFamily$Tp;
// pick.M isAnnotationPresent(Tp)=true
// pick.M getAnnotation(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// pick.M getDeclaredAnnotation(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// pick.M getAnnotationsByType(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// pick.M getDeclaredAnnotationsByType(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// pick.returnIsM=true param0IsM=true
// ctor.C class=sun.reflect.generics.reflectiveObjects.TypeVariableImpl
// ctor.C name=C toString=C typeName=C
// ctor.C bounds=[java.lang.Object] arrayClass=[Ljava.lang.reflect.Type;
// ctor.C boundsAfterStore=[java.lang.Object] copy=true
// ctor.C decl=ctor G/1
// ctor.C annotatedBounds=[java.lang.Object[]]
// ctor.C annotations=[] declared=[]
// ctor.C getAnnotation(Tp)=null getAnnotation(Tu)=null getDeclaredAnnotation(Tp)=null
// ctor.C byType(Rep)=[] declaredByType(Rep)=[] byType(Tu)=[] byTypeArrayClass=[LL1W125TypeVariableImplFamily$Tp;
// ctor.C isAnnotationPresent(Tp)=false
// ctor.C getAnnotation(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// ctor.C getDeclaredAnnotation(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// ctor.C getAnnotationsByType(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// ctor.C getDeclaredAnnotationsByType(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// Inner.I class=sun.reflect.generics.reflectiveObjects.TypeVariableImpl
// Inner.I name=I toString=I typeName=I
// Inner.I bounds=[T] arrayClass=[Ljava.lang.reflect.Type;
// Inner.I boundsAfterStore=[T] copy=true
// Inner.I decl=class Inner
// Inner.I annotatedBounds=[T[]]
// Inner.I annotations=[] declared=[]
// Inner.I getAnnotation(Tp)=null getAnnotation(Tu)=null getDeclaredAnnotation(Tp)=null
// Inner.I byType(Rep)=[] declaredByType(Rep)=[] byType(Tu)=[] byTypeArrayClass=[LL1W125TypeVariableImplFamily$Tp;
// Inner.I isAnnotationPresent(Tp)=false
// Inner.I getAnnotation(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// Inner.I getDeclaredAnnotation(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// Inner.I getAnnotationsByType(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// Inner.I getDeclaredAnnotationsByType(null) -> java.lang.NullPointerException: [null] top=java.util.Objects.requireNonNull
// Inner.outer type=T class=sun.reflect.generics.reflectiveObjects.TypeVariableImpl equalsG.T=true
// field t class=sun.reflect.generics.reflectiveObjects.TypeVariableImpl name=T
// field t vs G.T equals=true reverse=true hashEq=true hashIsDeclXorName=true
// G.T twice equals=true reverse=true hashEq=true hashIsDeclXorName=true
// G.T vs G.U equals=false reverse=false hashEq=false hashIsDeclXorName=true
// pick.M twice equals=true reverse=true hashEq=true hashIsDeclXorName=true
// G.T equals pick.M=false G.T equals String=false G.T equals null=false
// us arg=U class=sun.reflect.generics.reflectiveObjects.TypeVariableImpl equalsG.U=true
// map args=[T, V] toString=java.util.Map<T, V>
// Sub super=L1W125TypeVariableImplFamily$G<java.lang.Integer, S, java.util.ArrayList<java.lang.Number>> arg1 class=sun.reflect.generics.reflectiveObjects.TypeVariableImpl equalsSub.S=true decl=class Sub
// reflective getBounds class=[Ljava.lang.reflect.Type; value=[java.lang.Number, java.lang.Comparable<T>]
// reflective getTypeName=V
// reflective getName=M
// reflective toString=U
// reflective getAnnotatedBounds length=1
// proxy getBounds=[java.lang.Number, java.lang.Comparable<T>] getName=T typeName=T annotations=[]
// concat=<T,U,V>
// map get by field type=first by U=null
// list=[T, U, V] toString via valueOf=V
// sink>0=true
// HOTSPOT_EXPECTED_END

import java.lang.annotation.ElementType;
import java.lang.annotation.Repeatable;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.annotation.Annotation;
import java.lang.reflect.AnnotatedType;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.ParameterizedType;
import java.lang.reflect.Proxy;
import java.lang.reflect.Type;
import java.lang.reflect.TypeVariable;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L1W125TypeVariableImplFamily {

    @Target(ElementType.TYPE_PARAMETER)
    @Retention(RetentionPolicy.RUNTIME)
    @interface Tp {
        String value() default "tp";
    }

    @Target(ElementType.TYPE_USE)
    @Retention(RetentionPolicy.RUNTIME)
    @interface Tu {
        String value() default "tu";
    }

    @Target({ElementType.TYPE_PARAMETER, ElementType.TYPE_USE})
    @Retention(RetentionPolicy.RUNTIME)
    @Repeatable(Reps.class)
    @interface Rep {
        int value();
    }

    @Target({ElementType.TYPE_PARAMETER, ElementType.TYPE_USE})
    @Retention(RetentionPolicy.RUNTIME)
    @interface Reps {
        Rep[] value();
    }

    static class G<T extends Number & Comparable<T>, U, @Tp("v") @Rep(1) @Rep(2) V extends @Tu("b") List<? super T>> {
        T t;
        List<U> us;
        Map<T, V> map;

        <@Tp("m") M extends Runnable> M pick(M m) {
            return m;
        }

        <C> G(C c) {
        }

        class Inner<I extends T> {
            I i;
            T outer;
        }
    }

    static class Sub<S> extends G<Integer, S, ArrayList<Number>> {
        Sub() {
            super("x");
        }
    }

    static String typeName(Type t) {
        return t == null ? "null" : t.getTypeName();
    }

    static String names(Type[] ts) {
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < ts.length; i++) {
            if (i > 0) {
                sb.append(", ");
            }
            sb.append(typeName(ts[i]));
        }
        return sb.append("]").toString();
    }

    static String annos(Annotation[] as) {
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < as.length; i++) {
            if (i > 0) {
                sb.append(", ");
            }
            sb.append(anno(as[i]));
        }
        return sb.append("]").toString();
    }

    static String anno(Annotation a) {
        if (a == null) {
            return "null";
        }
        if (a instanceof Tp tp) {
            return "@Tp(" + tp.value() + ")";
        }
        if (a instanceof Tu tu) {
            return "@Tu(" + tu.value() + ")";
        }
        if (a instanceof Rep r) {
            return "@Rep(" + r.value() + ")";
        }
        if (a instanceof Reps rs) {
            return "@Reps" + annos(rs.value());
        }
        return "@" + a.annotationType().getSimpleName();
    }

    static String decl(Object d) {
        if (d instanceof Class<?> c) {
            return "class " + c.getSimpleName();
        }
        if (d instanceof Method m) {
            return "method " + m.getName();
        }
        if (d instanceof java.lang.reflect.Constructor<?> c) {
            return "ctor " + c.getDeclaringClass().getSimpleName() + "/" + c.getParameterCount();
        }
        return String.valueOf(d);
    }

    static void describe(String label, TypeVariable<?> tv) {
        System.out.println(label + " class=" + tv.getClass().getName());
        System.out.println(label + " name=" + tv.getName() + " toString=" + tv + " typeName=" + tv.getTypeName());
        Type[] b1 = tv.getBounds();
        System.out.println(label + " bounds=" + names(b1) + " arrayClass=" + b1.getClass().getName());
        if (b1.length > 0) {
            b1[0] = String.class;
        }
        System.out.println(label + " boundsAfterStore=" + names(tv.getBounds()) + " copy=" + (b1 != tv.getBounds()));
        System.out.println(label + " decl=" + decl(tv.getGenericDeclaration()));
        AnnotatedType[] ab = tv.getAnnotatedBounds();
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < ab.length; i++) {
            if (i > 0) {
                sb.append(", ");
            }
            sb.append(typeName(ab[i].getType())).append(annos(ab[i].getAnnotations()));
        }
        System.out.println(label + " annotatedBounds=" + sb.append("]"));
        System.out.println(label + " annotations=" + annos(tv.getAnnotations())
                + " declared=" + annos(tv.getDeclaredAnnotations()));
        System.out.println(label + " getAnnotation(Tp)=" + anno(tv.getAnnotation(Tp.class))
                + " getAnnotation(Tu)=" + anno(tv.getAnnotation(Tu.class))
                + " getDeclaredAnnotation(Tp)=" + anno(tv.getDeclaredAnnotation(Tp.class)));
        System.out.println(label + " byType(Rep)=" + annos(tv.getAnnotationsByType(Rep.class))
                + " declaredByType(Rep)=" + annos(tv.getDeclaredAnnotationsByType(Rep.class))
                + " byType(Tu)=" + annos(tv.getAnnotationsByType(Tu.class))
                + " byTypeArrayClass=" + tv.getAnnotationsByType(Tp.class).getClass().getName());
        System.out.println(label + " isAnnotationPresent(Tp)=" + tv.isAnnotationPresent(Tp.class));
        nullArg(label + " getAnnotation(null)", () -> tv.getAnnotation(null));
        nullArg(label + " getDeclaredAnnotation(null)", () -> tv.getDeclaredAnnotation(null));
        nullArg(label + " getAnnotationsByType(null)", () -> tv.getAnnotationsByType(null));
        nullArg(label + " getDeclaredAnnotationsByType(null)", () -> tv.getDeclaredAnnotationsByType(null));
    }

    interface Call {
        Object run() throws Throwable;
    }

    static void nullArg(String label, Call c) {
        try {
            System.out.println(label + " -> " + c.run());
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            String top = st.length == 0 ? "none" : st[0].getClassName() + "." + st[0].getMethodName();
            System.out.println(label + " -> " + t.getClass().getName() + ": [" + t.getMessage() + "] top=" + top);
        }
    }

    static void sameness(String label, TypeVariable<?> a, TypeVariable<?> b) {
        System.out.println(label + " equals=" + a.equals(b) + " reverse=" + b.equals(a)
                + " hashEq=" + (a.hashCode() == b.hashCode())
                + " hashIsDeclXorName=" + (a.hashCode() == (a.getGenericDeclaration().hashCode() ^ a.getName().hashCode())));
    }

    public static void main(String[] args) throws Throwable {
        TypeVariable<?>[] gs = G.class.getTypeParameters();
        System.out.println("G params=" + names(gs) + " arrayClass=" + gs.getClass().getName());
        describe("G.T", gs[0]);
        describe("G.U", gs[1]);
        describe("G.V", gs[2]);

        Method pick = G.class.getDeclaredMethod("pick", Runnable.class);
        TypeVariable<Method> m = pick.getTypeParameters()[0];
        describe("pick.M", m);
        System.out.println("pick.returnIsM=" + pick.getGenericReturnType().equals(m)
                + " param0IsM=" + pick.getGenericParameterTypes()[0].equals(m));

        java.lang.reflect.Constructor<?> ctor = G.class.getDeclaredConstructor(Object.class);
        describe("ctor.C", ctor.getTypeParameters()[0]);

        TypeVariable<?>[] inner = G.Inner.class.getTypeParameters();
        describe("Inner.I", inner[0]);
        Type outerT = G.Inner.class.getDeclaredField("outer").getGenericType();
        System.out.println("Inner.outer type=" + typeName(outerT) + " class=" + outerT.getClass().getName()
                + " equalsG.T=" + outerT.equals(gs[0]));

        Type ft = G.class.getDeclaredField("t").getGenericType();
        System.out.println("field t class=" + ft.getClass().getName() + " name=" + typeName(ft));
        sameness("field t vs G.T", (TypeVariable<?>) ft, gs[0]);
        sameness("G.T twice", G.class.getTypeParameters()[0], gs[0]);
        sameness("G.T vs G.U", gs[0], gs[1]);
        sameness("pick.M twice", pick.getTypeParameters()[0], m);
        System.out.println("G.T equals pick.M=" + gs[0].equals(m) + " G.T equals String=" + gs[0].equals("T")
                + " G.T equals null=" + gs[0].equals(null));

        ParameterizedType us = (ParameterizedType) G.class.getDeclaredField("us").getGenericType();
        Type uArg = us.getActualTypeArguments()[0];
        System.out.println("us arg=" + typeName(uArg) + " class=" + uArg.getClass().getName()
                + " equalsG.U=" + uArg.equals(gs[1]));
        ParameterizedType map = (ParameterizedType) G.class.getDeclaredField("map").getGenericType();
        System.out.println("map args=" + names(map.getActualTypeArguments()) + " toString=" + map);

        ParameterizedType sup = (ParameterizedType) Sub.class.getGenericSuperclass();
        Type sArg = sup.getActualTypeArguments()[1];
        System.out.println("Sub super=" + sup + " arg1 class=" + sArg.getClass().getName()
                + " equalsSub.S=" + sArg.equals(Sub.class.getTypeParameters()[0])
                + " decl=" + decl(((TypeVariable<?>) sArg).getGenericDeclaration()));

        // Reflective calls, the SerializableTypeWrapper shape.
        Method getBounds = TypeVariable.class.getMethod("getBounds");
        Object rb = getBounds.invoke(gs[0]);
        System.out.println("reflective getBounds class=" + rb.getClass().getName() + " value=" + names((Type[]) rb));
        Method getTypeName = Type.class.getMethod("getTypeName");
        System.out.println("reflective getTypeName=" + getTypeName.invoke(gs[2]));
        Method getName = TypeVariable.class.getMethod("getName");
        System.out.println("reflective getName=" + getName.invoke(m));
        Method toStr = Object.class.getMethod("toString");
        System.out.println("reflective toString=" + toStr.invoke(gs[1]));
        Method annBounds = TypeVariable.class.getMethod("getAnnotatedBounds");
        Object rab = annBounds.invoke(gs[2]);
        System.out.println("reflective getAnnotatedBounds length=" + ((AnnotatedType[]) rab).length);
        final TypeVariable<?> target = gs[0];
        InvocationHandler h = (proxy, method, a) -> method.invoke(target, a);
        TypeVariable<?> wrapped = (TypeVariable<?>) Proxy.newProxyInstance(
                L1W125TypeVariableImplFamily.class.getClassLoader(), new Class<?>[] {TypeVariable.class}, h);
        System.out.println("proxy getBounds=" + names(wrapped.getBounds()) + " getName=" + wrapped.getName()
                + " typeName=" + wrapped.getTypeName() + " annotations=" + annos(wrapped.getAnnotations()));

        System.out.println("concat=" + ("<" + gs[0] + "," + gs[1] + "," + gs[2] + ">"));
        Map<Type, String> keyed = new HashMap<>();
        keyed.put(gs[0], "first");
        System.out.println("map get by field type=" + keyed.get(ft) + " by U=" + keyed.get(gs[1]));
        System.out.println("list=" + Arrays.asList(gs) + " toString via valueOf=" + String.valueOf(gs[2]));

        // TIME rows (bench): row off against row on, P-cores, medians of interleaved runs.
        TypeVariable<?> hot = gs[0];
        long sink = 0;
        for (int rep = 0; rep < 3; rep++) {
            int n = 200_000;
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                sink += hot.getName().length();
            }
            System.out.println("TIME getName rep=" + rep + " ns/op=" + (System.nanoTime() - t0) / n);
            t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                sink += hot.getBounds().length;
            }
            System.out.println("TIME getBounds rep=" + rep + " ns/op=" + (System.nanoTime() - t0) / n);
            t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                sink += hot.toString().length();
            }
            System.out.println("TIME toString rep=" + rep + " ns/op=" + (System.nanoTime() - t0) / n);
            int na = 20_000;
            t0 = System.nanoTime();
            for (int i = 0; i < na; i++) {
                sink += gs[2].getAnnotations().length;
            }
            System.out.println("TIME getAnnotations rep=" + rep + " ns/op=" + (System.nanoTime() - t0) / na);
        }
        System.out.println("sink>0=" + (sink > 0));
    }
}
