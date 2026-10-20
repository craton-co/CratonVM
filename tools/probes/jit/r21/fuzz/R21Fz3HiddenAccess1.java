// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 13, lane fuzz3: reflective and MethodHandle access to and
// from HIDDEN classes, cold and through hot loops.
//
// Risk targeted: wave 8's hidden runtime package (`P/0x..` is in P's package
// for the reflective access check; the StringConcatFactory inline strategy
// Generator failed without it, CRATONVM_REFLECT_HIDDEN_RUNTIME_PACKAGE), page
// `r21-hidden-class-own-private-ctor-access-refused` (a hidden class
// reflecting on its OWN private constructor), the MHN access texts (holder
// spelled like getName), waves 10-12 Lookup.in (isSamePackage /
// isSamePackageMember with a hidden lookup or target, the hidden-member ICCE),
// and `useNativeAccessorOnly`'s once-per-VM read. Arms: H (hidden), HN (hidden
// NESTMATE of this class). Facts:
//   * Class answers: isHidden, sanitized getName / getSimpleName,
//     getCanonicalName, getPackageName, getNestHost, Class.forName(getName());
//   * from this class: newInstance of the private and the package-private
//     constructors, Method.invoke of a private method, Field.getInt of a private
//     field, all without setAccessible (refusal texts sanitized);
//   * from inside the hidden class: the same on its own members, a findVirtual
//     on its own private method, its own lookupModes, a string concat, a lambda;
//   * Lookup modes: hidden full lookup, in(this class), this class's in(hidden),
//     dropLookupMode(PRIVATE).in(..), privateLookupIn(hidden, lookup()),
//     findVirtual / findGetter of the private members from this class's lookup;
// then 30k-iteration hot loops through the reflective and MH routes, and every
// fact again ("hot" pass).
// Hidden names are printed with `/0x<hex>` -> `/0x#` and `@<hex>` -> `@#`.
//
// Run:
//   javac -d out R21Fz3HiddenAccess1.java
//   java -Xmx256m -cp out R21Fz3HiddenAccess1                    # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz3HiddenAccess1                # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz3HiddenAccess1
//   cratonvm --nojit -Xmx256m -cp out R21Fz3HiddenAccess1
//   java -Djdk.reflect.useNativeAccessorOnly=true ... / cratonvm -Djdk.reflect.useNativeAccessorOnly=true ...
//   CRATONVM_REFLECT_HIDDEN_RUNTIME_PACKAGE=0 cratonvm ...    # kill-switch control: access rows DIFF
// HotSpot expectation: every line identical on every arm.

import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.concurrent.Callable;

public class R21Fz3HiddenAccess1 {
    static final String PAYLOAD = "R21Fz3HiddenAccess1Payload";
    static final int HOT = 30_000;

    static String clean(Object o) {
        if (o == null) {
            return "null";
        }
        return String.valueOf(o).replaceAll("/0x[0-9a-fA-F]+", "/0x#").replaceAll("@[0-9a-fA-F]+", "@#");
    }

    static String describe(Throwable e) {
        if (e instanceof InvocationTargetException) {
            return "ITE(" + describe(e.getCause()) + ")";
        }
        String r = e.getClass().getName() + ":" + clean(e.getMessage());
        Throwable c = e.getCause();
        return c == null ? r : r + " cause=" + c.getClass().getName() + ":" + clean(c.getMessage());
    }

    static Object inv(MethodHandle mh) throws Exception {
        try {
            return (Object) mh.invokeExact();
        } catch (Exception | Error e) {
            throw e;
        } catch (Throwable t) {
            throw new RuntimeException(t);
        }
    }

    static void fact(String arm, String id, Callable<Object> c) {
        String r;
        try {
            r = "= " + clean(c.call());
        } catch (Throwable e) {
            r = "! " + describe(e);
        }
        System.out.println("fact " + arm + " " + id + " " + r);
    }

    static void facts(String pass, String arm, Lookup hl) {
        Class<?> h = hl.lookupClass();
        String a = pass + " " + arm;
        Lookup me = MethodHandles.lookup();
        fact(a, "isHidden", h::isHidden);
        fact(a, "name", h::getName);
        fact(a, "simpleName", h::getSimpleName);
        fact(a, "canonical", h::getCanonicalName);
        fact(a, "package", h::getPackageName);
        fact(a, "nestHost", () -> h.getNestHost().getName());
        fact(a, "forName", () -> Class.forName(h.getName()));
        fact(a, "forNameNoInit", () -> Class.forName(h.getName(), false, h.getClassLoader()));
        fact(a, "outer.privCtor", () -> h.getDeclaredConstructor().newInstance().toString());
        fact(a, "outer.pkgCtor", () -> h.getDeclaredConstructor(int.class).newInstance(11).toString());
        fact(a, "outer.privMethod", () -> {
            Object o = h.getDeclaredConstructor(int.class).newInstance(3);
            Method m = h.getDeclaredMethod("priv", int.class);
            return m.invoke(o, 4);
        });
        fact(a, "outer.privField", () -> {
            Object o = h.getDeclaredConstructor(int.class).newInstance(5);
            Field f = h.getDeclaredField("secret");
            return f.getInt(o);
        });
        fact(a, "outer.privStaticField", () -> {
            Field f = h.getDeclaredField("sCount");
            return f.getInt(null);
        });
        fact(a, "outer.setAccessibleFinal", () -> {
            Field f = h.getDeclaredField("TAG");
            f.setAccessible(true);
            f.set(null, "changed");
            return f.get(null);
        });
        for (String self : new String[] {"selfCtor", "selfMethod", "selfField", "selfMh", "selfModes", "concat",
                "lambda", "selfNestHost"}) {
            fact(a, "inside." + self, () -> {
                return inv(hl.findStatic(h, self, MethodType.methodType(Object.class)));
            });
        }
        fact(a, "modes.full", hl::lookupModes);
        fact(a, "modes.in(outer)", () -> hl.in(R21Fz3HiddenAccess1.class).lookupModes());
        fact(a, "modes.outer.in(h)", () -> me.in(h).lookupModes());
        fact(a, "modes.dropPrivate.in(outer)",
                () -> hl.dropLookupMode(Lookup.PRIVATE).in(R21Fz3HiddenAccess1.class).lookupModes());
        fact(a, "modes.in(Object)", () -> hl.in(Object.class).lookupModes());
        fact(a, "modes.in(mate)", () -> hl.in(R21Fz3HiddenAccess1Mate.class).lookupModes());
        fact(a, "modes.privateLookupIn", () -> MethodHandles.privateLookupIn(h, me).lookupModes());
        fact(a, "modes.in(h).in(outer)", () -> me.in(h).in(R21Fz3HiddenAccess1.class).lookupModes());
        fact(a, "lookup.toString", hl::toString);
        fact(a, "outer.findVirtualPriv", () -> me.findVirtual(h, "priv", MethodType.methodType(int.class, int.class)));
        fact(a, "outer.findGetterPriv", () -> me.findGetter(h, "secret", int.class));
        fact(a, "outer.findStaticPkg", () -> me.findStatic(h, "concat", MethodType.methodType(Object.class)));
        fact(a, "outer.findConstructorPriv", () -> me.findConstructor(h, MethodType.methodType(void.class)));
        fact(a, "outer.findStaticPkg.call", () -> {
            return inv(me.findStatic(h, "concat", MethodType.methodType(Object.class)));
        });
        fact(a, "mate.reflectPkg", () -> R21Fz3HiddenAccess1Mate.reflectPkg(h));
        fact(a, "mate.reflectPriv", () -> R21Fz3HiddenAccess1Mate.reflectPriv(h));
    }

    static long hot(Lookup hl) throws Throwable {
        Class<?> h = hl.lookupClass();
        Constructor<?> pkg = h.getDeclaredConstructor(int.class);
        pkg.setAccessible(true);
        Method priv = h.getDeclaredMethod("priv", int.class);
        priv.setAccessible(true);
        Field secret = h.getDeclaredField("secret");
        secret.setAccessible(true);
        MethodHandle concat = hl.findStatic(h, "concat", MethodType.methodType(Object.class));
        MethodHandle loop = hl.findStatic(h, "loop", MethodType.methodType(int.class, int.class));
        MethodHandle selfCtor = hl.findStatic(h, "selfCtor", MethodType.methodType(Object.class));
        long acc = 0;
        for (int i = 0; i < HOT; i++) {
            Object o = pkg.newInstance(i & 31);
            acc = acc * 31 + (int) priv.invoke(o, i & 7);
            acc = acc * 31 + secret.getInt(o);
            if ((i & 15) == 0) {
                acc = acc * 31 + ((String) (Object) concat.invokeExact()).length();
                acc = acc * 31 + ((Object) selfCtor.invokeExact()).toString().length();
            }
        }
        acc = acc * 31 + (int) loop.invokeExact(HOT);
        return acc;
    }

    public static void main(String[] args) throws Throwable {
        byte[] bytes;
        try (InputStream in = R21Fz3HiddenAccess1.class.getResourceAsStream(PAYLOAD + ".class")) {
            bytes = in.readAllBytes();
        }
        Lookup l = MethodHandles.lookup();
        Lookup h = l.defineHiddenClass(bytes, true);
        Lookup hn = l.defineHiddenClass(bytes, true, Lookup.ClassOption.NESTMATE);
        facts("cold", "H", h);
        facts("cold", "HN", hn);
        System.out.println("hot H " + hot(h));
        System.out.println("hot HN " + hot(hn));
        System.out.println("hot H2 " + hot(h));
        facts("hot", "H", h);
        facts("hot", "HN", hn);
    }
}

/** Another top-level class of the package: reflects on the hidden class from a third class. */
class R21Fz3HiddenAccess1Mate {
    static Object reflectPkg(Class<?> h) throws Exception {
        return h.getDeclaredConstructor(int.class).newInstance(21).toString();
    }

    static Object reflectPriv(Class<?> h) throws Exception {
        return h.getDeclaredConstructor().newInstance().toString();
    }
}

/** The payload: never loaded by name; defined hidden from its bytes. */
class R21Fz3HiddenAccess1Payload {
    static final String TAG = "tag";
    private static int sCount;
    private int secret = 7;

    private R21Fz3HiddenAccess1Payload() {
        sCount++;
    }

    R21Fz3HiddenAccess1Payload(int s) {
        secret = s;
    }

    private int priv(int x) {
        return x + secret;
    }

    @Override
    public String toString() {
        return "P(" + secret + ")";
    }

    static Object selfCtor() throws Exception {
        Constructor<R21Fz3HiddenAccess1Payload> c = R21Fz3HiddenAccess1Payload.class.getDeclaredConstructor();
        return c.newInstance();
    }

    static Object selfMethod() throws Exception {
        Method m = R21Fz3HiddenAccess1Payload.class.getDeclaredMethod("priv", int.class);
        return m.invoke(new R21Fz3HiddenAccess1Payload(2), 5);
    }

    static Object selfField() throws Exception {
        Field f = R21Fz3HiddenAccess1Payload.class.getDeclaredField("secret");
        return f.getInt(new R21Fz3HiddenAccess1Payload(9));
    }

    static Object selfMh() throws Throwable {
        MethodHandle mh = MethodHandles.lookup().findVirtual(R21Fz3HiddenAccess1Payload.class, "priv",
                MethodType.methodType(int.class, int.class))
                .asType(MethodType.methodType(Object.class, Object.class, int.class));
        // Object-typed call-site descriptor: a descriptor naming this class by name
        // cannot be satisfied by the hidden class (VerifyError on HotSpot).
        Object self = new R21Fz3HiddenAccess1Payload(1);
        return (Object) mh.invokeExact(self, 6);
    }

    static Object selfModes() {
        return MethodHandles.lookup().lookupModes();
    }

    static Object concat() {
        int i = 4;
        long l = 1L << 40;
        char ch = 'z';
        return "c" + i + ":" + l + ch + 2.5 + true + (Object) null;
    }

    static Object lambda() {
        int k = 3;
        java.util.function.IntUnaryOperator f = x -> x * k + 1;
        java.util.function.Supplier<String> s = () -> "s" + k;
        return f.applyAsInt(5) + s.get();
    }

    static Object selfNestHost() {
        return R21Fz3HiddenAccess1Payload.class.getNestHost() == R21Fz3HiddenAccess1Payload.class;
    }

    static int loop(int n) throws Exception {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += ((String) concat()).length();
            if ((i & 63) == 0) {
                acc += selfCtor().toString().length() + (int) selfMethod() + (int) selfField();
            }
        }
        return acc;
    }
}
