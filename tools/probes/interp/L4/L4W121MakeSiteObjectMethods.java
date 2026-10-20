// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 121, lane L4
// (`i121-L4-jdk-string-concat-factory-hidden-class-constructor-is-refused`;
// on w116 and w120 every row failed, `direct-mixed` included, so the cause is
// the JDK `StringConcatFactory`'s hidden-class constructor, not makeSite): the one-run
// diagnostic for the `ObjectMethods` exclusion in `link_generic_call_site`.
// With CRATONVM_INDY_GENERIC_VIA_MAKESITE=1 on the w119 binary,
// `L4W40BootstrapTypedVarargs` `accessor-mixed-tostring` failed with
// `BootstrapMethodError / RuntimeException: StringConcatException: Generator
// failed`. The direct route, which this VM used before, passes. The two routes
// differ in (a) the lookup (`MethodHandles.lookup()` vs
// `IMPL_LOOKUP.in(caller)`), (b) the getters array (this VM's packed
// `MethodHandle[]` vs `BootstrapMethodInvoker`'s `Arrays.copyOfRange`), and
// (c) the entry (a direct bytecode call vs `invokeExact` / `invokeBasic`
// through the LambdaForm chain). Each row below changes one of them. A
// failing row prints the throwable and its causes, up to five levels.
//
//   direct-mixed        ObjectMethods.bootstrap called from bytecode, the
//                       caller's own lookup, getters `a` (field) + `b()`
//                       (accessor METHOD): the control
//   iwa-mixed           the same through a varargs `MethodHandle` and
//                       `invokeWithArguments`, trailing getters collected into
//                       MethodHandle[] (entry (c) without the lookup)
//   iwa-array-mixed     the same through `asFixedArity()` with an explicit
//                       MethodHandle[] (no collector; what BMI's copyOfRange
//                       array meets)
//   impl-lookup-mixed   direct call, lookup `IMPL_LOOKUP.in(caller)` (lookup
//                       (a) alone); needs the --add-opens below
//   makesite-mixed      `CallSite.makeSite(bsm, ...)` by reflection: what the
//                       VM's makeSite route runs (a + b + c); needs --add-opens
//   makesite-fields     the same with two FIELD getters (does the mix matter?)
//
// Without --add-opens java.base/java.lang.invoke=ALL-UNNAMED the last three
// rows print `no-access: java.lang.reflect.InaccessibleObjectException` on
// both VMs; run with it.
//
// Run: javac -d out L4W121MakeSiteObjectMethods.java
//      cratonvm --java-home <jdk25> [--nojit] [--compatible] \
//          --add-opens java.base/java.lang.invoke=ALL-UNNAMED \
//          -cp out L4W121MakeSiteObjectMethods
//      (switch on or off: the reflective rows do not depend on it)
//
// Expected HotSpot 25.0.3 output (default and -Xint, with --add-opens):
//   direct-mixed: Mixed[a=1, b=x, c=true] | Mixed[a=1, b=x]
//   iwa-mixed: Mixed[a=1, b=x, c=true] | Mixed[a=1, b=x]
//   iwa-array-mixed: Mixed[a=1, b=x, c=true] | Mixed[a=1, b=x]
//   impl-lookup-mixed: Mixed[a=1, b=x, c=true] | Mixed[a=1, b=x]
//   makesite-mixed: Mixed[a=1, b=x, c=true] | Mixed[a=1, b=x]
//   makesite-fields: Mixed[a=1, b=x, c=true] | Mixed[a=1, b=x]
//
// (The first half of each row is the record's own javac `toString`, the
// second the linked site's answer for names "a;b".)
import java.lang.invoke.CallSite;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.TypeDescriptor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.runtime.ObjectMethods;

public class L4W121MakeSiteObjectMethods {
    record Mixed(int a, String b, boolean c) {}

    static final MethodType SITE = MethodType.methodType(String.class, Mixed.class);
    static final MethodType BSM_TYPE = MethodType.methodType(Object.class, MethodHandles.Lookup.class,
            String.class, TypeDescriptor.class, Class.class, String.class, MethodHandle[].class);

    interface Linker {
        Object link(MethodHandle[] getters) throws Throwable;
    }

    static String describe(Throwable t) {
        StringBuilder sb = new StringBuilder();
        for (int depth = 0; t != null && depth < 5; depth++, t = t.getCause()) {
            if (depth > 0) {
                sb.append(" <- ");
            }
            sb.append(t.getClass().getName()).append(": ").append(t.getMessage());
        }
        return sb.toString();
    }

    static MethodHandle[] mixedGetters() throws ReflectiveOperationException {
        MethodHandles.Lookup l = MethodHandles.lookup();
        return new MethodHandle[] {
            l.findGetter(Mixed.class, "a", int.class),
            l.findVirtual(Mixed.class, "b", MethodType.methodType(String.class)),
        };
    }

    static MethodHandle[] fieldGetters() throws ReflectiveOperationException {
        MethodHandles.Lookup l = MethodHandles.lookup();
        return new MethodHandle[] {
            l.findGetter(Mixed.class, "a", int.class),
            l.findGetter(Mixed.class, "b", String.class),
        };
    }

    static String row(Linker linker, boolean fields) {
        Mixed m = new Mixed(1, "x", true);
        try {
            Object linked = linker.link(fields ? fieldGetters() : mixedGetters());
            MethodHandle target = ((CallSite) linked).getTarget();
            return m + " | " + (String) target.invokeExact(m);
        } catch (InvocationTargetException e) {
            return describe(e.getCause());
        } catch (RuntimeException e) {
            if (e.getClass().getName().equals("java.lang.reflect.InaccessibleObjectException")) {
                return "no-access: " + e.getClass().getName();
            }
            return describe(e);
        } catch (Throwable t) {
            return describe(t);
        }
    }

    static MethodHandle bsm() throws ReflectiveOperationException {
        return MethodHandles.lookup().findStatic(ObjectMethods.class, "bootstrap", BSM_TYPE);
    }

    static MethodHandles.Lookup implLookupIn(Class<?> caller) throws ReflectiveOperationException {
        Field f = MethodHandles.Lookup.class.getDeclaredField("IMPL_LOOKUP");
        f.setAccessible(true);
        return ((MethodHandles.Lookup) f.get(null)).in(caller);
    }

    static Object makeSite(MethodHandle[] getters) throws Throwable {
        Method mk = CallSite.class.getDeclaredMethod("makeSite", MethodHandle.class, String.class,
                MethodType.class, Object.class, Class.class);
        mk.setAccessible(true);
        Object[] info = new Object[2 + getters.length];
        info[0] = Mixed.class;
        info[1] = "a;b";
        System.arraycopy(getters, 0, info, 2, getters.length);
        return mk.invoke(null, bsm(), "toString", SITE, info, L4W121MakeSiteObjectMethods.class);
    }

    public static void main(String[] args) {
        System.out.println("direct-mixed: " + row(g -> ObjectMethods.bootstrap(
                MethodHandles.lookup(), "toString", SITE, Mixed.class, "a;b", g[0], g[1]), false));
        System.out.println("iwa-mixed: " + row(g -> bsm().invokeWithArguments(
                MethodHandles.lookup(), "toString", SITE, Mixed.class, "a;b", g[0], g[1]), false));
        System.out.println("iwa-array-mixed: " + row(g -> bsm().asFixedArity().invokeWithArguments(
                MethodHandles.lookup(), "toString", SITE, Mixed.class, "a;b", g), false));
        System.out.println("impl-lookup-mixed: " + row(g -> ObjectMethods.bootstrap(
                implLookupIn(L4W121MakeSiteObjectMethods.class), "toString", SITE, Mixed.class, "a;b", g),
                false));
        System.out.println("makesite-mixed: " + row(L4W121MakeSiteObjectMethods::makeSite, false));
        System.out.println("makesite-fields: " + row(L4W121MakeSiteObjectMethods::makeSite, true));
    }
}
