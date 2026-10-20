// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 63, lane L4 -- method handles name their owner by
// class id, stage 2 (`CRATONVM_MH_OWNER_ID_AUTHORITY`, opt-in; proposal
// `i61-L4-proposal-method-handles-name-their-owner-by-class-id-20261025.md`).
//
// Two child-first loaders each define `Base` and `Sub extends Base` (`Sub`
// HIDES `Base.v`). Handles are looked up on loader A's `Base`. HotSpot casts
// the receiver to that exact class: loader B's `Base` -- same name, another
// class -- and B's `Sub` are refused with `ClassCastException: Cannot cast
// <name> to <name>`. Before stage 2 this model's `--compatible` door judged a
// named owner's receiver by NAME, so the `*-foreign` rows ran loader B's
// method / read and wrote loader B's field. The other rows are controls the
// switch must not move (the hidden field is the owner's, read by id now).
// The `*-again` rows repeat the refusals after a warm loop (the field-accessor
// link must not admit a namesake it was not made for).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// namesakes: true true
// virtual-own: 1
// virtual-sub: sub
// virtual-sub-get: 1
// getter-hidden: 1
// setter-hidden: 10/2
// getter-exact-sub: 10
// unreflect-getter: 1
// unreflect-setter: 7
// virtual-foreign: java.lang.ClassCastException: Cannot cast L4W63ForeignOwnerCast$Base to L4W63ForeignOwnerCast$Base
// virtual-foreign-sub: java.lang.ClassCastException: Cannot cast L4W63ForeignOwnerCast$Sub to L4W63ForeignOwnerCast$Base
// getter-foreign: java.lang.ClassCastException: Cannot cast L4W63ForeignOwnerCast$Base to L4W63ForeignOwnerCast$Base
// getter-foreign-sub: java.lang.ClassCastException: Cannot cast L4W63ForeignOwnerCast$Sub to L4W63ForeignOwnerCast$Base
// setter-foreign: java.lang.ClassCastException: Cannot cast L4W63ForeignOwnerCast$Base to L4W63ForeignOwnerCast$Base
// setter-foreign-left: 1
// unreflect-getter-foreign: java.lang.ClassCastException: Cannot cast L4W63ForeignOwnerCast$Base to L4W63ForeignOwnerCast$Base
// bindTo-foreign: java.lang.ClassCastException: Cannot cast L4W63ForeignOwnerCast$Base to L4W63ForeignOwnerCast$Base
// getter-bindTo-foreign: java.lang.ClassCastException: Cannot cast L4W63ForeignOwnerCast$Base to L4W63ForeignOwnerCast$Base
// warm: 4000
// virtual-foreign-again: java.lang.ClassCastException: Cannot cast L4W63ForeignOwnerCast$Base to L4W63ForeignOwnerCast$Base
// getter-foreign-again: java.lang.ClassCastException: Cannot cast L4W63ForeignOwnerCast$Base to L4W63ForeignOwnerCast$Base
// getter-exact-foreign: java.lang.ClassCastException: Cannot cast L4W63ForeignOwnerCast$Base to L4W63ForeignOwnerCast$Base
// setter-exact-foreign: java.lang.ClassCastException: Cannot cast L4W63ForeignOwnerCast$Base to L4W63ForeignOwnerCast$Base
// HOTSPOT_EXPECTED_END
//
// Switches: under `--compatible` the `*-foreign*` rows need
// `CRATONVM_MH_OWNER_ID_AUTHORITY=1` (and stage 1's
// `CRATONVM_MH_INSTANCE_OWNER_ID`, default on); without it they print loader
// B's values (`virtual-foreign: 1`, `getter-foreign: 1`, `bindTo-foreign:
// MethodHandle(...)`, ...) -- the positive control. Under `--jdk-only` the
// handles are the JDK's own `DirectMethodHandle`s and the switch reaches only
// this model's handles.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W63ForeignOwnerCast`.
import java.io.IOException;
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W63ForeignOwnerCast {
    public static class Base {
        public int v = 1;

        public int get() {
            return v;
        }

        public String who() {
            return "base";
        }
    }

    public static class Sub extends Base {
        public int v = 2;

        @Override
        public String who() {
            return "sub";
        }
    }

    static final String BASE = "L4W63ForeignOwnerCast$Base";
    static final String SUB = "L4W63ForeignOwnerCast$Sub";

    /** Defines `Base` and `Sub` itself (child-first); delegates the rest. */
    static final class Isolating extends ClassLoader {
        Isolating(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(BASE) && !name.equals(SUB)) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    byte[] bytes;
                    try (InputStream in = L4W63ForeignOwnerCast.class.getResourceAsStream(name + ".class")) {
                        if (in == null) {
                            throw new ClassNotFoundException(name);
                        }
                        bytes = in.readAllBytes();
                    } catch (IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                    c = defineClass(name, bytes, 0, bytes.length);
                }
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            }
        }
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        if (out.startsWith("MethodHandle(")) {
            out = "MethodHandle(...)";
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        ClassLoader app = L4W63ForeignOwnerCast.class.getClassLoader();
        Class<?> baseA = new Isolating(app).loadClass(BASE);
        Class<?> subA = Class.forName(SUB, true, baseA.getClassLoader());
        ClassLoader lb = new Isolating(app);
        Class<?> baseB = lb.loadClass(BASE);
        Class<?> subB = lb.loadClass(SUB);
        Object a = baseA.getConstructor().newInstance();
        Object sa = subA.getConstructor().newInstance();
        Object a2 = baseA.getConstructor().newInstance();
        Object b = baseB.getConstructor().newInstance();
        Object sb = subB.getConstructor().newInstance();

        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle get = lk.findVirtual(baseA, "get", MethodType.methodType(int.class));
        MethodHandle who = lk.findVirtual(baseA, "who", MethodType.methodType(String.class));
        MethodHandle getter = lk.findGetter(baseA, "v", int.class);
        MethodHandle setter = lk.findSetter(baseA, "v", int.class);
        MethodHandle ugetter = lk.unreflectGetter(baseA.getField("v"));
        MethodHandle usetter = lk.unreflectSetter(baseA.getField("v"));
        MethodHandle exactGetter = getter.asType(MethodType.methodType(int.class, Object.class));
        MethodHandle exactSetter = setter.asType(MethodType.methodType(void.class, Object.class, int.class));
        MethodHandle subGetter = lk.findGetter(subA, "v", int.class);
        MethodHandle bGetter = lk.findGetter(baseB, "v", int.class);

        row("namesakes", () -> baseA.getName().equals(baseB.getName()) + " " + (baseA != baseB));
        row("virtual-own", () -> (int) get.invoke(a));
        row("virtual-sub", () -> (String) who.invoke(sa));
        row("virtual-sub-get", () -> (int) get.invoke(sa));
        row("getter-hidden", () -> (int) getter.invoke(sa));
        row("setter-hidden", () -> {
            setter.invoke(sa, 10);
            return (int) getter.invoke(sa) + "/" + (int) subGetter.invoke(sa);
        });
        row("getter-exact-sub", () -> (int) exactGetter.invokeExact((Object) sa));
        row("unreflect-getter", () -> (int) ugetter.invoke(a));
        row("unreflect-setter", () -> {
            usetter.invoke(a2, 7);
            return (int) getter.invoke(a2);
        });
        row("virtual-foreign", () -> (int) get.invoke(b));
        row("virtual-foreign-sub", () -> (String) who.invoke(sb));
        row("getter-foreign", () -> (int) getter.invoke(b));
        row("getter-foreign-sub", () -> (int) getter.invoke(sb));
        row("setter-foreign", () -> {
            setter.invoke(b, 5);
            return "stored";
        });
        row("setter-foreign-left", () -> (int) bGetter.invoke(b));
        row("unreflect-getter-foreign", () -> (int) ugetter.invoke(b));
        row("bindTo-foreign", () -> get.bindTo(b));
        row("getter-bindTo-foreign", () -> getter.bindTo(b));
        long n = 0;
        for (int i = 0; i < 2000; i++) {
            n += (int) get.invoke(a);
            n += (int) exactGetter.invokeExact((Object) a);
            exactSetter.invokeExact((Object) a2, 7);
        }
        System.out.println("warm: " + n);
        row("virtual-foreign-again", () -> (int) get.invoke(b));
        row("getter-foreign-again", () -> (int) getter.invoke(b));
        row("getter-exact-foreign", () -> (int) exactGetter.invokeExact((Object) b));
        row("setter-exact-foreign", () -> {
            exactSetter.invokeExact((Object) b, 9);
            return "stored";
        });
    }
}
