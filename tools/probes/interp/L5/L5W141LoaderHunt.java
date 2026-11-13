// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i7 wave 141, lane L5 (hunt; page
// docs/feature-designs/real-jdk-mode.md): class-loading edges no
// earlier probe covers. Each row prints the answer or `ExceptionClass: message`.
//   d-*  ClassLoader.defineClass argument and supertype errors
//   k-*  parallel-capable registration and getClassLoadingLock
//   p-*  defined packages of a custom loader, array / primitive packages
//   l-*  Lookup.defineClass / defineHiddenClass refusals
//
// Run: javac -d out L5W141LoaderHunt.java
//      java -cp out L5W141LoaderHunt                                    (HotSpot)
//      cratonvm --real-jdk|--compatible [--nojit] -cp out L5W141LoaderHunt
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// d-neg-off: java.lang.ArrayIndexOutOfBoundsException: Array region -1..55 out of bounds for length 56
// d-past-end: java.lang.ArrayIndexOutOfBoundsException: Array region 0..57 out of bounds for length 56
// d-null-bytes: java.lang.NullPointerException
// d-super-final: java.lang.IncompatibleClassChangeError: class h.SubOfFinal cannot inherit from final class h.Final
// d-super-iface: java.lang.IncompatibleClassChangeError: class h.SuperIsIface has interface h.Iface as super class
// d-impl-class: java.lang.IncompatibleClassChangeError: class h.ImplClass can not implement h.Plain, because it is not an interface (h.Plain is in unnamed module of loader 'hunt' @HASH)
// d-self-super: java.lang.ClassCircularityError: h/SelfSuper
// d-missing-super: java.lang.NoClassDefFoundError: h/Missing cause=java.lang.ClassNotFoundException: h.Missing
// d-iface-super-not-object: java.lang.ClassFormatError: Interfaces must have java.lang.Object as superclass in class file h/BadIface
// d-ok: h.Ok loader=hunt
// k-plain-lock-is-loader: true
// k-plain-registered: false
// k-parallel-registered: true
// k-parallel-lock-is-loader: false
// k-parallel-lock-same-name: true
// k-parallel-lock-other-name: false
// k-sub-registered: false
// k-sub-lock-is-loader: true
// p-defined: h spec=null impl=null sealed=false
// p-defined-count: 1
// p-define-twice: java.lang.IllegalArgumentException: h
// p-parent-sees: null
// p-array: null
// p-array-name: java.lang
// p-primitive: null
// p-primitive-name: java.lang
// p-platform-java-lang: null
// l-other-package: java.lang.IllegalArgumentException: other/Gen not in same package as lookup class
// l-no-full-privilege: java.lang.IllegalAccessException: L5W141LoaderHunt/package does not have full privilege access
// l-public-lookup: java.lang.IllegalAccessException: Lookup does not have PACKAGE access
// l-hidden-null: java.lang.NullPointerException
// l-hidden-ok: hidden=true package=
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.invoke.MethodHandles;
import java.util.HashMap;
import java.util.Map;

public class L5W141LoaderHunt {
    static final class Hunt extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Hunt() {
            super("hunt", L5W141LoaderHunt.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = bytes.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }

        Class<?> def(String name, byte[] b, int off, int len) {
            return defineClass(name, b, off, len);
        }

        Package defPkg(String name) {
            return definePackage(name, null, null, null, null, null, null, null);
        }
    }

    static final class Parallel extends ClassLoader {
        static final boolean REGISTERED = ClassLoader.registerAsParallelCapable();

        Parallel() {
            super(null);
        }

        Object lock(String n) {
            return getClassLoadingLock(n);
        }
    }

    static final class SubOfParallel extends ClassLoader {
        SubOfParallel() {
            super(null);
        }

        Object lock(String n) {
            return getClassLoadingLock(n);
        }
    }

    static final class Plain extends ClassLoader {
        Plain() {
            super(null);
        }

        Object lock(String n) {
            return getClassLoadingLock(n);
        }
    }

    static byte[] cls(String name, String sup, int flags, String... ifaces) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(flags);
            if (sup != null) {
                cb.withSuperclass(ClassDesc.of(sup));
            }
            for (String i : ifaces) {
                cb.withInterfaceSymbols(ClassDesc.of(i));
            }
        });
    }

    static final int PUB = ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER;
    static final int IFACE = ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT;

    static String err(Throwable t) {
        String s = t.getClass().getName();
        if (t instanceof NullPointerException) {
            return s;
        }
        String m = String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@HASH");
        String r = s + ": " + m;
        if (t instanceof NoClassDefFoundError && t.getCause() != null) {
            r += " cause=" + t.getCause();
        }
        return r;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String label, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = err(t);
        }
        System.out.println(label + ": " + out);
    }

    static String loaderName(Class<?> c) {
        ClassLoader l = c.getClassLoader();
        return l == null ? "null" : l.getName();
    }

    public static void main(String[] args) throws Throwable {
        Hunt h = new Hunt();
        h.bytes.put("h.Final", cls("h.Final", null, PUB | ClassFile.ACC_FINAL));
        h.bytes.put("h.Iface", cls("h.Iface", null, IFACE));
        h.bytes.put("h.Plain", cls("h.Plain", null, PUB));
        h.bytes.put("h.SubOfFinal", cls("h.SubOfFinal", "h.Final", PUB));
        h.bytes.put("h.SuperIsIface", cls("h.SuperIsIface", "h.Iface", PUB));
        h.bytes.put("h.ImplClass", cls("h.ImplClass", null, PUB, "h.Plain"));
        h.bytes.put("h.SelfSuper", cls("h.SelfSuper", "h.SelfSuper", PUB));
        h.bytes.put("h.MissingSuper", cls("h.MissingSuper", "h.Missing", PUB));
        h.bytes.put("h.BadIface", cls("h.BadIface", "h.Plain", IFACE));
        byte[] ok = cls("h.Ok", null, PUB);

        row("d-neg-off", () -> h.def("h.Ok", ok, -1, ok.length));
        row("d-past-end", () -> h.def("h.Ok", ok, 0, ok.length + 1));
        row("d-null-bytes", () -> h.def("h.Ok", null, 0, 1));
        row("d-super-final", () -> h.loadClass("h.SubOfFinal"));
        row("d-super-iface", () -> h.loadClass("h.SuperIsIface"));
        row("d-impl-class", () -> h.loadClass("h.ImplClass"));
        row("d-self-super", () -> h.loadClass("h.SelfSuper"));
        row("d-missing-super", () -> h.loadClass("h.MissingSuper"));
        row("d-iface-super-not-object", () -> h.loadClass("h.BadIface"));
        row("d-ok", () -> {
            Class<?> c = h.def("h.Ok", ok, 0, ok.length);
            return c.getName() + " loader=" + loaderName(c);
        });

        Plain plain = new Plain();
        Parallel par = new Parallel();
        SubOfParallel sub = new SubOfParallel();
        row("k-plain-lock-is-loader", () -> plain.lock("a.B") == plain);
        row("k-plain-registered", plain::isRegisteredAsParallelCapable);
        row("k-parallel-registered", () -> Parallel.REGISTERED && par.isRegisteredAsParallelCapable());
        row("k-parallel-lock-is-loader", () -> par.lock("a.B") == par);
        row("k-parallel-lock-same-name", () -> par.lock("a.B") == par.lock("a.B"));
        row("k-parallel-lock-other-name", () -> par.lock("a.B") == par.lock("a.C"));
        row("k-sub-registered", sub::isRegisteredAsParallelCapable);
        row("k-sub-lock-is-loader", () -> sub.lock("a.B") == sub);

        row("p-defined", () -> {
            Package p = h.getDefinedPackage("h");
            return p.getName() + " spec=" + p.getSpecificationVersion() + " impl="
                    + p.getImplementationVersion() + " sealed=" + p.isSealed();
        });
        row("p-defined-count", () -> h.getDefinedPackages().length);
        row("p-define-twice", () -> h.defPkg("h"));
        row("p-parent-sees", () -> ClassLoader.getSystemClassLoader().getDefinedPackage("h"));
        row("p-array", () -> int[].class.getPackage());
        row("p-array-name", () -> String[].class.getPackageName());
        row("p-primitive", () -> int.class.getPackage());
        row("p-primitive-name", () -> int.class.getPackageName());
        row("p-platform-java-lang", () -> ClassLoader.getPlatformClassLoader().getDefinedPackage("java.lang"));

        MethodHandles.Lookup lk = MethodHandles.lookup();
        row("l-other-package", () -> lk.defineHiddenClass(cls("other.Gen", null, PUB), true));
        row("l-no-full-privilege", () -> lk.dropLookupMode(MethodHandles.Lookup.PRIVATE)
                .defineHiddenClass(cls("Gen3", null, PUB), true));
        row("l-public-lookup", () -> MethodHandles.publicLookup().defineClass(cls("Gen4", null, PUB)));
        row("l-hidden-null", () -> lk.defineHiddenClass(null, true));
        row("l-hidden-ok", () -> {
            Class<?> c = lk.defineHiddenClass(cls("Gen5", null, PUB), true).lookupClass();
            return "hidden=" + c.isHidden() + " package=" + c.getPackageName();
        });
    }
}
