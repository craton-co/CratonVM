// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 53, lane L5 -- `ClassLoader.loadClass` asks its
// receiver (and the receiver's parents), never the loader of the class that
// called it. CratonVM's base `loadClass` native looked the name up in its flat
// store first, and that lookup, on a miss, drove the CALLING frame's defining
// loader (`NativeContextImpl::class_via_caller_loader`). For a loader asked from
// one of its own classes -- Spring Boot's `LaunchedClassLoader` asked by
// Spring's code, every `Class.forName` / `ClassUtils.isPresent` of a fat jar --
// that is the receiver again, so a name it does not have walked its whole
// `URLClassPath` TWICE (the `unflattened-app-class-path` start-up cost of
// `docs/internal/fixed-bugs/interpreter-L5-compatible-gates-that-still-differ-after-the-alignment-table-FIXED-20261007.md`).
// For a loader asked from ANOTHER loader's class, the caller's loader was asked
// for (and defined) a name HotSpot never asks it for.
//
// Every row is first (a cold VM) and again last, on fresh loaders; the
// counts are `URLStreamHandler.openConnection` calls for the named entry:
//
//   absent   `Caller` (defined by loader A) calls `A.loadClass(name)` of a name
//            A does not have: HotSpot looks once (`opens=1`).
//   present  the same for a name A has: one look, A's class.
//   sibling  `Caller` (defined by loader B) calls `C.loadClass(name)` of a name
//            only B has: HotSpot throws without ever asking B (`bOpens=0`).
//
// Run (no setup):
//   javac -d out L5W53LoadClassAsksReceiverOnly.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W53LoadClassAsksReceiverOnly
//
// Positive control: `CRATONVM_LOAD_CLASS_ASKS_RECEIVER_ONLY=0` restores the
// caller-loader drive (expected on the base: `absent-N=... opens=2` and
// `sibling-N=... bOpens=1`).
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   absent-1=java.lang.ClassNotFoundException opens=1
//   present-1=loader=true opens=1
//   sibling-1=java.lang.ClassNotFoundException bOpens=0 cOpens=1
//   absent-2=java.lang.ClassNotFoundException opens=1
//   present-2=loader=true opens=1
//   sibling-2=java.lang.ClassNotFoundException bOpens=0 cOpens=1

import java.io.ByteArrayInputStream;
import java.io.FileNotFoundException;
import java.io.InputStream;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.net.URL;
import java.net.URLClassLoader;
import java.net.URLConnection;
import java.net.URLStreamHandler;
import java.util.HashMap;
import java.util.Map;

public class L5W53LoadClassAsksReceiverOnly {
    static final class MemHandler extends URLStreamHandler {
        final Map<String, byte[]> files = new HashMap<>();
        final Map<String, Long> opens = new HashMap<>();

        @Override
        protected URLConnection openConnection(URL u) {
            String path = u.getPath();
            opens.merge(path, 1L, Long::sum);
            return new URLConnection(u) {
                @Override
                public void connect() {
                }

                @Override
                public InputStream getInputStream() throws java.io.IOException {
                    byte[] b = files.get(path);
                    if (b == null) {
                        throw new FileNotFoundException(path);
                    }
                    return new ByteArrayInputStream(b);
                }
            };
        }

        long opens(String entry) {
            return opens.getOrDefault("/root/" + entry, 0L);
        }
    }

    static byte[] empty(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withSuperclass(ConstantDescs.CD_Object));
    }

    // public class l5w53.Caller { public static Class ask(ClassLoader l, String n) { return l.loadClass(n); } }
    static byte[] caller() {
        ClassDesc cl = ClassDesc.of("java.lang.ClassLoader");
        ClassDesc cls = ConstantDescs.CD_Class;
        MethodTypeDesc ask = MethodTypeDesc.of(cls, cl, ConstantDescs.CD_String);
        MethodTypeDesc load = MethodTypeDesc.of(cls, ConstantDescs.CD_String);
        return ClassFile.of().build(ClassDesc.of("l5w53.Caller"), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withSuperclass(ConstantDescs.CD_Object)
                .withMethodBody("ask", ask, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code
                        .aload(0)
                        .aload(1)
                        .invokevirtual(cl, "loadClass", load)
                        .areturn()));
    }

    @SuppressWarnings("deprecation")
    static URL base(MemHandler h, String... classes) throws Exception {
        for (String c : classes) {
            byte[] bytes = c.equals("l5w53.Caller") ? caller() : empty(c);
            h.files.put("/root/" + c.replace('.', '/') + ".class", bytes);
        }
        return new URL(null, "l5w53mem:/root/", h);
    }

    /** `Caller.ask(target, name)` with `Caller` loaded by `callerLoader`. */
    static Object ask(ClassLoader callerLoader, ClassLoader target, String name) throws Exception {
        Method m = callerLoader.loadClass("l5w53.Caller").getMethod("ask", ClassLoader.class, String.class);
        try {
            return m.invoke(null, target, name);
        } catch (InvocationTargetException e) {
            return e.getCause();
        }
    }

    static String outcome(Object o, ClassLoader expected) {
        if (o instanceof Class<?> c) {
            return "loader=" + (c.getClassLoader() == expected);
        }
        return o == null ? "null" : o.getClass().getName();
    }

    static void absentAndPresent(int round) throws Exception {
        MemHandler h = new MemHandler();
        try (URLClassLoader a = new URLClassLoader(new URL[] {base(h, "l5w53.Caller", "l5w53.Gen")})) {
            Object absent = ask(a, a, "l5w53.Absent");
            System.out.println("absent-" + round + "=" + outcome(absent, a)
                    + " opens=" + h.opens("l5w53/Absent.class"));
            Object present = ask(a, a, "l5w53.Gen");
            System.out.println("present-" + round + "=" + outcome(present, a)
                    + " opens=" + h.opens("l5w53/Gen.class"));
        }
    }

    static void sibling(int round) throws Exception {
        MemHandler hb = new MemHandler();
        MemHandler hc = new MemHandler();
        try (URLClassLoader b = new URLClassLoader(new URL[] {base(hb, "l5w53.Caller", "l5w53.Only")});
                URLClassLoader c = new URLClassLoader(new URL[] {base(hc)})) {
            Object o = ask(b, c, "l5w53.Only");
            System.out.println("sibling-" + round + "=" + outcome(o, c)
                    + " bOpens=" + hb.opens("l5w53/Only.class")
                    + " cOpens=" + hc.opens("l5w53/Only.class"));
        }
    }

    public static void main(String[] args) throws Exception {
        for (int round = 1; round <= 2; round++) {
            absentAndPresent(round);
            sibling(round);
        }
    }
}
