// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 23, lane review-w23: ORDINARY classes whose last name
// segment is literally `0x<hex>` (`fz9/0x1F`, `fz9/0x2A`: legal class-file
// names, no javac output), against a hidden class, across the message texts
// that name a class. Wave 22 (`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`) decides
// the `/0x` tail from the class's own hidden flag at the sites that hold the
// class; the context-free renderers (the RuntimeError funnel's
// ArrayStoreException / ClassCastException texts, NoSuchMethodError texts)
// stay judged by the name's shape.
//
// Risk targeted (docs/known-issues/jit/r21-review-w23-findings-20261019.md,
// items 5 and 6): which texts still print `fz9/0x2A` where HotSpot prints
// `fz9.0x2A` (Klass::external_name(): '/' -> '.', the hidden form only for a
// hidden class or an array whose BOTTOM class is hidden), and that the
// "Could not initialize class" text, now split by the class's flag, still
// keeps a hidden class's tail.
//
// Run:
//   javac -d out R21Fz9ReviewHexName1.java
//   java -cp out R21Fz9ReviewHexName1                     # HotSpot 25
//   cratonvm -cp out R21Fz9ReviewHexName1                 # --jdk-only
//   cratonvm --compatible -cp out R21Fz9ReviewHexName1
//
// HotSpot expectation (by reading Klass::external_name,
// InterpreterRuntime::throw_ArrayStoreException,
// SharedRuntime::generate_class_cast_message, Class.cast and
// InstanceKlass::initialize_impl's "Could not initialize class " + external
// name; not yet run). A hidden name's address is printed as /0x*:
//   name=fz9.0x2A simple="0x2A" type=fz9.0x2A desc=Lfz9/0x2A; str=class fz9.0x2A
//   array=[Lfz9.0x2A; arraySimple="0x2A[]"
//   checkcast=class fz9.0x2A cannot be cast to class java.lang.String
//   cast=Cannot cast fz9.0x2A to java.lang.String
//   aastore=java.lang.ArrayStoreException: fz9.0x2A
//   init1=java.lang.ExceptionInInitializerError cause=java.lang.RuntimeException: boom
//   init2=java.lang.NoClassDefFoundError: Could not initialize class fz9.0x1F cause=java.lang.ExceptionInInitializerError
//   hidden name=R21Fz9ReviewHexName1$Hid/0x*
//   hidden init1=java.lang.ExceptionInInitializerError cause=java.lang.RuntimeException: boom
//   hidden init2=java.lang.NoClassDefFoundError: Could not initialize class R21Fz9ReviewHexName1$Hid/0x* cause=java.lang.ExceptionInInitializerError
//   hidden aastore=java.lang.ArrayStoreException: R21Fz9ReviewHexName1$Hid2/0x*
// Expected on CratonVM: init2 and the hidden rows match (wave 22's split);
// name / array likely match (the mirror name); checkcast and aastore are the
// context-free renderers and likely print fz9/0x2A on both modes (item 5).
// Wave 25, lane hexname (pending orchestrator build): every row is expected
// to match in both modes. checkcast and aastore were fixed by wave 24, and
// getName / simple / type / str / array / cast / desc (--compatible) by
// CRATONVM_CLASS_NAME_HIDDEN_BY_CLASS. Per-row list:
// docs/known-issues/jit/r21-fuzz9-probes-20261019.md item 7.

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Array;
import java.util.HashMap;
import java.util.Map;
import java.util.function.Supplier;

public class R21Fz9ReviewHexName1 {
    static final String H = "R21Fz9ReviewHexName1";
    static final Map<String, byte[]> BYTES = new HashMap<>();

    static final class L extends ClassLoader {
        L() {
            super("fz9hex", ClassLoader.getPlatformClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = BYTES.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }
    }

    static final ClassDesc RTE = ClassDesc.of("java.lang.RuntimeException");

    /** A public class with a public no-arg constructor; `throwing` adds a <clinit> that throws. */
    static byte[] cls(String name, boolean throwing) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    ConstantDescs.MTD_void)
                            .return_());
            if (throwing) {
                cb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void,
                        ClassFile.ACC_STATIC,
                        code -> code.new_(RTE).dup().ldc("boom")
                                .invokespecial(RTE, ConstantDescs.INIT_NAME,
                                        MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String))
                                .athrow());
            }
        });
    }

    static String hide(String s) {
        return s == null ? "null" : s.replaceAll("/0x[0-9a-fA-F]+", "/0x*");
    }

    static String q(String s) {
        return s == null ? "null" : "\"" + s + "\"";
    }

    /** The answer or the throwable; the cause's message only for an EIIE (the NCDFE's cause text varies). */
    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (Throwable t) {
            String m = t.getMessage();
            if (t instanceof ClassCastException && m != null && m.indexOf(" (") > 0) {
                m = m.substring(0, m.indexOf(" ("));
            }
            Throwable c = t.getCause();
            String cause = c == null ? ""
                    : t instanceof ExceptionInInitializerError
                            ? " cause=" + c.getClass().getName() + ": " + c.getMessage()
                            : " cause=" + c.getClass().getName();
            return t.getClass().getName() + (m == null ? "" : ": " + m) + cause;
        }
    }

    static String checkcast(Object o) {
        String s = (String) o;
        return s;
    }

    static String aastore(Object o) {
        Object[] a = new String[1];
        a[0] = o;
        return "stored";
    }

    public static void main(String[] args) throws Throwable {
        BYTES.put("fz9.0x2A", cls("fz9.0x2A", false));
        BYTES.put("fz9.0x1F", cls("fz9.0x1F", true));
        L l = new L();
        Class<?> c = Class.forName("fz9.0x2A", false, l);
        Object o = c.getDeclaredConstructor().newInstance();
        System.out.println("name=" + safe(c::getName) + " simple=" + safe(() -> q(c.getSimpleName()))
                + " type=" + safe(c::getTypeName) + " desc=" + safe(c::descriptorString)
                + " str=" + safe(c::toString));
        Class<?> arr = Array.newInstance(c, 0).getClass();
        System.out.println("array=" + safe(arr::getName) + " arraySimple=" + safe(() -> q(arr.getSimpleName())));
        System.out.println("checkcast=" + safe(() -> checkcast(o)));
        System.out.println("cast=" + safe(() -> String.class.cast(o)));
        System.out.println("aastore=" + safe(() -> aastore(o)));
        System.out.println("init1=" + safe(() -> forName("fz9.0x1F", l)));
        System.out.println("init2=" + safe(() -> forName("fz9.0x1F", l)));

        MethodHandles.Lookup hl = MethodHandles.lookup().defineHiddenClass(cls(H + "$Hid", true), false);
        Class<?> hid = hl.lookupClass();
        System.out.println("hidden name=" + hide(safe(hid::getName)));
        System.out.println("hidden init1=" + hide(safe(() -> init(hl, hid))));
        System.out.println("hidden init2=" + hide(safe(() -> init(hl, hid))));
        MethodHandles.Lookup hl2 = MethodHandles.lookup().defineHiddenClass(cls(H + "$Hid2", false), true);
        Object ho = hl2.findConstructor(hl2.lookupClass(),
                java.lang.invoke.MethodType.methodType(void.class)).invoke();
        System.out.println("hidden aastore=" + hide(safe(() -> aastore(ho))));
    }

    static String forName(String name, ClassLoader l) {
        try {
            return Class.forName(name, true, l).getName();
        } catch (ClassNotFoundException e) {
            throw new IllegalStateException(e);
        }
    }

    static String init(MethodHandles.Lookup hl, Class<?> hid) {
        try {
            hl.ensureInitialized(hid);
            return "initialized";
        } catch (IllegalAccessException e) {
            throw new IllegalStateException(e);
        }
    }
}
