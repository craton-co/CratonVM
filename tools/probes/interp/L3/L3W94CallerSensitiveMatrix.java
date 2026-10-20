// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 94, lane L3 -- the probe matrix stage 3 of
// docs/internal/fixed-bugs/interpreter-i49-L3-proposal-one-caller-class-rule-over-the-walkers-capture-DONE-20261128.md
// waits on: the caller-sensitive doors whose caller this VM's reflective door
// natives decide (`lang_class.rs` `resolve_caller_class_id`: the name list
// `REFLECTION_INTERNAL_CLASSES`; with `CRATONVM_REFLECTION_CS_CALLER_RULE=1`,
// HotSpot's `JVM_GetCallerClass` rule over the walkers' capture), each asked
// by an application (unnamed-module) caller in up to five shapes:
//
//   direct   the caller calls the @CallerSensitive method;
//   reflect  the caller calls it through `Method.invoke` (HotSpot skips
//            `Method.invoke` and the accessor frames: the caller answers);
//   chain    the caller calls `Method.invoke` of `Method.invoke` of it;
//   lambda   through a method reference (`Field::get`, ...): the lambda proxy
//            (hidden, a nestmate of this class, same loader and module)
//            calls it, and IS the caller;
//   handle   through a `MethodHandle` from `findVirtual` / `findStatic`
//            (HotSpot binds the caller-sensitive method to an injected
//            invoker in the lookup class's loader and package).
//
// Rows (`<door>-<shape>`; each prints the answer, or the throwable's class
// and message -- an `InvocationTargetException` unwrapped -- with identity
// hashes (` @1a2b`) and hidden-class suffixes (`/0x1a2b` -> `/0x`) erased):
//   get-other-*     Field.get of L3W94Other's private field, no setAccessible:
//                   IllegalAccessException naming the caller;
//   get-own-*       Field.get of this class's private field: allowed (5);
//   open-util-*     setAccessible(true) on java.util.ArrayList.size: refused,
//                   InaccessibleObjectException (JEP 403);
//   try-util-*      trySetAccessible() on the same field: false;
//   open-own-*      setAccessible(true) on this class's private field: ok;
//   newinst-sha-*   Class.newInstance of sun.security.provider.SHA (public
//                   class, public no-arg constructor, package not exported to
//                   the unnamed module): IllegalAccessException naming the
//                   caller (the JDK's own setAccessible of the copied
//                   constructor is called by java.lang.Class and passes);
//   res-lang-*      Object.class.getResource("uniName.dat") (java.lang is not
//                   open to the unnamed module): null; res-class-direct, a
//                   `.class` resource (never encapsulated): found;
//   lookup-*        MethodHandles.lookup().lookupClass();
//   forname-*       Class.forName("q94.OnlyA"), a class only the custom loader
//                   `a` defines, asked by this class (application loader) or
//                   by L3W94Asker (defined by `a`);
//   bundle-*        ResourceBundle.getBundle("q94.Bundle"), the same split;
//   jdk-*           a JDK frame is the caller: Enum.valueOf (Class.
//                   getEnumConstantsShared's values.setAccessible(true)), an
//                   annotation's toString (AnnotationInvocationHandler's
//                   setAccessible of the member methods), Proxy of a
//                   package-private interface, Character.getName (java.lang's
//                   CharacterName reads uniName.dat);
//   hot-*           20,000 asks from a stream lambda (compiled callers):
//                   count of answers equal to the direct row's;
//   get-other-direct-again  the first row again (cold-VM gap).
//
// HOTSPOT_EXPECTED_BEGIN (derived from the JDK 25 sources; get-other-reflect/-chain corrected from the w95 HotSpot 25.0.3 run; rows marked (?) below are the least certain)
// get-other-direct: java.lang.IllegalAccessException: class L3W94CallerSensitiveMatrix cannot access a member of class L3W94Other with modifiers "private"
// get-other-reflect: java.lang.IllegalAccessException: class L3W94CallerSensitiveMatrix$$InjectedInvoker/0x cannot access a member of class L3W94Other with modifiers "private"
// get-other-chain: java.lang.IllegalAccessException: class L3W94CallerSensitiveMatrix$$InjectedInvoker/0x cannot access a member of class L3W94Other with modifiers "private"
// get-other-lambda: java.lang.IllegalAccessException: class L3W94CallerSensitiveMatrix$$Lambda/0x cannot access a member of class L3W94Other with modifiers "private"
// get-other-handle: java.lang.IllegalAccessException: class L3W94CallerSensitiveMatrix$$InjectedInvoker/0x cannot access a member of class L3W94Other with modifiers "private"
// get-own-direct: 5
// get-own-reflect: 5
// get-own-chain: 5
// get-own-lambda: 5
// get-own-handle: 5
// open-util-direct: java.lang.reflect.InaccessibleObjectException: Unable to make field private int java.util.ArrayList.size accessible: module java.base does not "opens java.util" to unnamed module
// open-util-reflect: java.lang.reflect.InaccessibleObjectException: Unable to make field private int java.util.ArrayList.size accessible: module java.base does not "opens java.util" to unnamed module
// open-util-chain: java.lang.reflect.InaccessibleObjectException: Unable to make field private int java.util.ArrayList.size accessible: module java.base does not "opens java.util" to unnamed module
// open-util-lambda: java.lang.reflect.InaccessibleObjectException: Unable to make field private int java.util.ArrayList.size accessible: module java.base does not "opens java.util" to unnamed module
// open-util-handle: java.lang.reflect.InaccessibleObjectException: Unable to make field private int java.util.ArrayList.size accessible: module java.base does not "opens java.util" to unnamed module
// try-util-direct: false
// try-util-reflect: false
// try-util-lambda: false
// open-own-direct: ok
// open-own-reflect: ok
// open-own-lambda: ok
// open-own-handle: ok
// newinst-sha-direct: java.lang.IllegalAccessException: class L3W94CallerSensitiveMatrix cannot access class sun.security.provider.SHA (in module java.base) because module java.base does not export sun.security.provider to unnamed module
// newinst-sha-reflect: java.lang.IllegalAccessException: class L3W94CallerSensitiveMatrix cannot access class sun.security.provider.SHA (in module java.base) because module java.base does not export sun.security.provider to unnamed module
// newinst-sha-lambda: java.lang.IllegalAccessException: class L3W94CallerSensitiveMatrix$$Lambda/0x cannot access class sun.security.provider.SHA (in module java.base) because module java.base does not export sun.security.provider to unnamed module
// res-lang-direct: null
// res-lang-reflect: null
// res-lang-lambda: null
// res-lang-handle: null
// res-lang-stream-direct: null
// res-class-direct: found
// lookup-direct: L3W94CallerSensitiveMatrix
// lookup-reflect: L3W94CallerSensitiveMatrix
// lookup-lambda: L3W94CallerSensitiveMatrix$$Lambda/0x
// forname-app-direct: java.lang.ClassNotFoundException: q94.OnlyA
// forname-app-reflect: java.lang.ClassNotFoundException: q94.OnlyA
// forname-asker-direct: q94.OnlyA
// forname-asker-reflect: q94.OnlyA
// forname-asker-lambda: q94.OnlyA
// forname-app-invokes-asker: q94.OnlyA
// bundle-app-direct: java.util.MissingResourceException
// bundle-asker-direct: v
// bundle-asker-reflect: v
// jdk-enum-valueof: RED
// jdk-annotation-tostring: @L3W94CallerSensitiveMatrix.Tag("x")
// jdk-proxy-package-private: proxied hi
// jdk-character-name: LATIN CAPITAL LETTER A
// hot-get-other: right=20000/20000
// hot-open-util: right=20000/20000
// hot-get-own: right=20000/20000
// get-other-direct-again: java.lang.IllegalAccessException: class L3W94CallerSensitiveMatrix cannot access a member of class L3W94Other with modifiers "private"
// HOTSPOT_EXPECTED_END
//
// Least certain expected lines (read from the JDK 25 sources, not measured):
// (?) get-other-handle / get-own-handle / open-own-handle: the injected
//     invoker's name and whether it is a nestmate of this class
//     (`MethodHandleImpl.BindCaller.makeInjectedInvoker`); get-own-handle and
//     open-own-handle may refuse if it is not;
// (?) get-other-lambda / newinst-sha-lambda / lookup-lambda: that the lambda
//     proxy (not an injected invoker) calls the method, as
//     `InnerClassLambdaMetafactory` does for an ordinary `invokevirtual` /
//     `invokestatic` implementation method;
// (?) jdk-annotation-tostring: `toStringImpl`'s canonical-name form.
// Rows a door-native change can move (stage 3): only the jdk-* rows and the
// rows whose caller is a non-hidden `java.lang.invoke` or `java.lang.Class`
// frame; every other row must print the same with the switch on and off.
//
// Positive control: `CRATONVM_DBG_CALLER=1 CRATONVM_REFLECTION_CS_CALLER_RULE=1`
// prints `[DBG_CALLER] reflective door (cs rule): <caller> (list: <list's>)`
// for each door ask a native answers (` DIFFERS` where the two differ).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W94CallerSensitiveMatrix`,
// and each cratonvm run again with `CRATONVM_REFLECTION_CS_CALLER_RULE=1`.
// Companion probes: L3W94CallerSensitiveOpens (--add-opens / --add-exports),
// L3W94CallerSensitiveNamed (a named-module caller in a layer).
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.AccessibleObject;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.net.URL;
import java.util.ArrayList;
import java.util.ResourceBundle;
import java.util.stream.IntStream;

public class L3W94CallerSensitiveMatrix {
    static final int HOT = 20_000;

    private int own = 5;

    interface Call { Object run() throws Throwable; }
    interface FieldGet { Object get(Field f, Object o) throws Exception; }
    interface FieldOpen { void open(Field f, boolean flag); }
    interface FieldTry { boolean open(Field f); }
    interface NewInst { Object make(Class<?> c) throws Exception; }
    interface ResGet { URL get(Class<?> c, String name); }
    interface LookupGet { MethodHandles.Lookup get(); }

    enum Color { RED, GREEN }

    @Retention(RetentionPolicy.RUNTIME)
    @interface Tag { String value(); }

    @Tag("x")
    static final class Tagged { }

    interface PkgIface { String hi(); }

    static String norm(String s) {
        if (s == null) {
            return "null";
        }
        return s.replaceAll(" @[0-9a-f]+", "").replaceAll("/0x[0-9a-f]+", "/0x");
    }

    static Throwable unwrap(Throwable t) {
        while (t instanceof InvocationTargetException && t.getCause() != null) {
            t = t.getCause();
        }
        return t;
    }

    static String show(Throwable t) {
        Throwable c = unwrap(t);
        return c.getClass().getName() + ": " + norm(c.getMessage());
    }

    static String answer(Call call) {
        try {
            Object o = call.run();
            return norm(String.valueOf(o));
        } catch (Throwable t) {
            return show(t);
        }
    }

    static void row(String label, Call call) {
        System.out.println(label + ": " + answer(call));
    }

    static void rowClassOnly(String label, Call call) {
        String out;
        try {
            out = norm(String.valueOf(call.run()));
        } catch (Throwable t) {
            out = unwrap(t).getClass().getName();
        }
        System.out.println(label + ": " + out);
    }

    static final Method INVOKE;
    static {
        try {
            INVOKE = Method.class.getMethod("invoke", Object.class, Object[].class);
        } catch (NoSuchMethodException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    /** `m.invoke(target, args)` made by `Method.invoke` itself (the chain shape). */
    static Object chain(Method m, Object target, Object... args) throws Throwable {
        return INVOKE.invoke(m, target, args);
    }

    static String found(URL u) {
        return u == null ? "null" : "found";
    }

    // ---- the custom loader `a`: L3W94Asker and its nested types child-first,
    // ---- q94.OnlyA and q94.Bundle renamed from this file's classes.

    static byte[] own(String binaryName) throws IOException {
        try (InputStream in = ClassLoader.getSystemResourceAsStream(binaryName.replace('.', '/') + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `bytes` with every constant-pool UTF8 `from` replaced by `to`. */
    static byte[] rename(byte[] bytes, String from, String to) throws IOException {
        DataInputStream in = new DataInputStream(new ByteArrayInputStream(bytes));
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(buf);
        out.writeInt(in.readInt());
        out.writeShort(in.readUnsignedShort());
        out.writeShort(in.readUnsignedShort());
        int count = in.readUnsignedShort();
        out.writeShort(count);
        for (int i = 1; i < count; i++) {
            int tag = in.readUnsignedByte();
            out.writeByte(tag);
            switch (tag) {
                case 1 -> out.writeUTF(in.readUTF().replace(from, to));
                case 3, 4 -> out.writeInt(in.readInt());
                case 5, 6 -> { out.writeLong(in.readLong()); i++; }
                case 7, 8, 16, 19, 20 -> out.writeShort(in.readUnsignedShort());
                case 9, 10, 11, 12, 17, 18 -> out.writeInt(in.readInt());
                case 15 -> { out.writeByte(in.readUnsignedByte()); out.writeShort(in.readUnsignedShort()); }
                default -> throw new IOException("tag " + tag);
            }
        }
        in.transferTo(out);
        out.flush();
        return buf.toByteArray();
    }

    static final class ALoader extends ClassLoader {
        ALoader(ClassLoader parent) {
            super("a", parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                try {
                    byte[] b;
                    if (name.startsWith("L3W94Asker")) {
                        b = own(name);
                    } else if (name.equals("q94.OnlyA")) {
                        b = rename(own("L3W94OnlyA"), "L3W94OnlyA", "q94/OnlyA");
                    } else if (name.equals("q94.Bundle")) {
                        b = rename(own("L3W94Bundle"), "L3W94Bundle", "q94/Bundle");
                    } else {
                        return super.loadClass(name, resolve);
                    }
                    return defineClass(name, b, 0, b.length);
                } catch (IOException e) {
                    throw new ClassNotFoundException(name, e);
                }
            }
        }
    }

    static int hot(Call call, String expected) {
        return (int) IntStream.range(0, HOT).filter(i -> answer(call).equals(expected)).count();
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        Field other = L3W94Other.class.getDeclaredField("secret");
        Field ownF = L3W94CallerSensitiveMatrix.class.getDeclaredField("own");
        Field size = ArrayList.class.getDeclaredField("size");
        Method fieldGet = Field.class.getMethod("get", Object.class);
        Method fieldOpen = Field.class.getMethod("setAccessible", boolean.class);
        Method fieldTry = AccessibleObject.class.getMethod("trySetAccessible");
        Method classNew = Class.class.getMethod("newInstance");
        Method classRes = Class.class.getMethod("getResource", String.class);
        MethodHandle getH = lookup.findVirtual(Field.class, "get", MethodType.methodType(Object.class, Object.class));
        MethodHandle openH = lookup.findVirtual(Field.class, "setAccessible", MethodType.methodType(void.class, boolean.class));
        MethodHandle resH = lookup.findVirtual(Class.class, "getResource", MethodType.methodType(URL.class, String.class));
        FieldGet getL = Field::get;
        FieldOpen openL = Field::setAccessible;
        FieldTry tryL = Field::trySetAccessible;
        @SuppressWarnings("deprecation")
        NewInst newL = Class::newInstance;
        ResGet resL = Class::getResource;
        LookupGet lookupL = MethodHandles::lookup;
        Object o = new L3W94Other();
        L3W94CallerSensitiveMatrix self = new L3W94CallerSensitiveMatrix();
        Class<?> sha = Class.forName("sun.security.provider.SHA", false, null);

        row("get-other-direct", () -> other.get(o));
        row("get-other-reflect", () -> fieldGet.invoke(other, o));
        row("get-other-chain", () -> chain(fieldGet, other, o));
        row("get-other-lambda", () -> getL.get(other, o));
        row("get-other-handle", () -> getH.invoke(other, o));
        row("get-own-direct", () -> ownF.get(self));
        row("get-own-reflect", () -> fieldGet.invoke(ownF, self));
        row("get-own-chain", () -> chain(fieldGet, ownF, self));
        row("get-own-lambda", () -> getL.get(ownF, self));
        row("get-own-handle", () -> getH.invoke(ownF, self));

        row("open-util-direct", () -> { size.setAccessible(true); return "ok"; });
        row("open-util-reflect", () -> { fieldOpen.invoke(size, true); return "ok"; });
        row("open-util-chain", () -> { chain(fieldOpen, size, true); return "ok"; });
        row("open-util-lambda", () -> { openL.open(size, true); return "ok"; });
        row("open-util-handle", () -> { openH.invoke(size, true); return "ok"; });
        row("try-util-direct", () -> size.trySetAccessible());
        row("try-util-reflect", () -> fieldTry.invoke(size));
        row("try-util-lambda", () -> tryL.open(size));
        // Each open-own row opens a fresh copy of the field.
        row("open-own-direct", () -> { L3W94CallerSensitiveMatrix.class.getDeclaredField("own").setAccessible(true); return "ok"; });
        row("open-own-reflect", () -> { fieldOpen.invoke(L3W94CallerSensitiveMatrix.class.getDeclaredField("own"), true); return "ok"; });
        row("open-own-lambda", () -> { openL.open(L3W94CallerSensitiveMatrix.class.getDeclaredField("own"), true); return "ok"; });
        row("open-own-handle", () -> { openH.invoke(L3W94CallerSensitiveMatrix.class.getDeclaredField("own"), true); return "ok"; });

        row("newinst-sha-direct", () -> { @SuppressWarnings("deprecation") Object x = sha.newInstance(); return x.getClass().getName(); });
        row("newinst-sha-reflect", () -> classNew.invoke(sha).getClass().getName());
        row("newinst-sha-lambda", () -> newL.make(sha).getClass().getName());

        row("res-lang-direct", () -> found(Object.class.getResource("uniName.dat")));
        row("res-lang-reflect", () -> found((URL) classRes.invoke(Object.class, "uniName.dat")));
        row("res-lang-lambda", () -> found(resL.get(Object.class, "uniName.dat")));
        row("res-lang-handle", () -> found((URL) resH.invoke(Object.class, "uniName.dat")));
        row("res-lang-stream-direct", () -> {
            try (InputStream in = Object.class.getResourceAsStream("uniName.dat")) {
                return in == null ? "null" : "found";
            }
        });
        row("res-class-direct", () -> found(Object.class.getResource("Object.class")));

        row("lookup-direct", () -> MethodHandles.lookup().lookupClass().getName());
        row("lookup-reflect", () -> ((MethodHandles.Lookup) MethodHandles.class.getMethod("lookup").invoke(null)).lookupClass().getName());
        row("lookup-lambda", () -> lookupL.get().lookupClass().getName());

        ClassLoader a = new ALoader(L3W94CallerSensitiveMatrix.class.getClassLoader());
        Class<?> asker = a.loadClass("L3W94Asker");
        Method askForName = asker.getMethod("forName", String.class);
        row("forname-app-direct", () -> Class.forName("q94.OnlyA").getName());
        row("forname-app-reflect", () -> ((Class<?>) Class.class.getMethod("forName", String.class).invoke(null, "q94.OnlyA")).getName());
        row("forname-asker-direct", () -> askForName.invoke(null, "q94.OnlyA"));
        row("forname-asker-reflect", () -> asker.getMethod("forNameReflective", String.class).invoke(null, "q94.OnlyA"));
        row("forname-asker-lambda", () -> asker.getMethod("forNameLambda", String.class).invoke(null, "q94.OnlyA"));
        row("forname-app-invokes-asker", () -> chain(askForName, null, "q94.OnlyA"));
        rowClassOnly("bundle-app-direct", () -> ResourceBundle.getBundle("q94.Bundle").getString("k"));
        row("bundle-asker-direct", () -> asker.getMethod("bundle", String.class).invoke(null, "q94.Bundle"));
        row("bundle-asker-reflect", () -> asker.getMethod("bundleReflective", String.class).invoke(null, "q94.Bundle"));

        row("jdk-enum-valueof", () -> Enum.valueOf(Color.class, "RED"));
        row("jdk-annotation-tostring", () -> Tagged.class.getAnnotation(Tag.class).toString());
        row("jdk-proxy-package-private", () -> {
            PkgIface p = (PkgIface) Proxy.newProxyInstance(PkgIface.class.getClassLoader(), new Class<?>[] {PkgIface.class},
                    (proxy, m, margs) -> "proxied " + m.getName());
            return p.hi();
        });
        row("jdk-character-name", () -> Character.getName('A'));

        String otherText = answer(() -> other.get(o));
        String openText = answer(() -> { size.setAccessible(true); return "ok"; });
        row("hot-get-other", () -> "right=" + hot(() -> other.get(o), otherText) + "/" + HOT);
        row("hot-open-util", () -> "right=" + hot(() -> { size.setAccessible(true); return "ok"; }, openText) + "/" + HOT);
        row("hot-get-own", () -> "right=" + hot(() -> ownF.get(self), "5") + "/" + HOT);

        row("get-other-direct-again", () -> other.get(o));
    }
}

/** A class of the same package and loader, not a nestmate. */
class L3W94Other {
    private int secret = 7;
}

/** Renamed `q94.OnlyA` and defined by the loader `a` only. */
class L3W94OnlyA {
}

/** Renamed `q94.Bundle` and defined by the loader `a` only. */
class L3W94Bundle extends java.util.ListResourceBundle {
    public L3W94Bundle() {
    }

    @Override
    protected Object[][] getContents() {
        return new Object[][] {{"k", "v"}};
    }
}

/** Defined by the loader `a` (child-first): its caller-sensitive asks answer `a`. */
class L3W94Asker {
    interface ForName { Class<?> find(String name) throws ClassNotFoundException; }

    static String safe(java.util.concurrent.Callable<Object> c) {
        try {
            Object o = c.call();
            return o instanceof Class<?> k ? k.getName() : String.valueOf(o);
        } catch (Throwable t) {
            Throwable x = t;
            while (x instanceof java.lang.reflect.InvocationTargetException && x.getCause() != null) {
                x = x.getCause();
            }
            return x.getClass().getName() + ": " + x.getMessage();
        }
    }

    public static String forName(String name) {
        return safe(() -> Class.forName(name));
    }

    public static String forNameReflective(String name) {
        return safe(() -> Class.class.getMethod("forName", String.class).invoke(null, name));
    }

    public static String forNameLambda(String name) {
        ForName f = Class::forName;
        return safe(() -> f.find(name));
    }

    public static String bundle(String name) {
        return safe(() -> java.util.ResourceBundle.getBundle(name).getString("k"));
    }

    public static String bundleReflective(String name) {
        return safe(() -> ((java.util.ResourceBundle) java.util.ResourceBundle.class
                .getMethod("getBundle", String.class).invoke(null, name)).getString("k"));
    }
}
