// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 120, lane L5 -- the checks `Lookup.defineClass`
// / `defineHiddenClass` make in Java before they reach the VM
// (`i44-L5-proposal-lookup-define-runs-the-jdks-own-checks`, "Wave 120"; switch
// `CRATONVM_LOOKUP_DEFINE_JDK_BODY`, default off: the VM's `Lookup` natives
// stand in front of the JDK body unless it is set). Every line must be
// HotSpot's in both modes, with the switch off (the natives' re-implemented
// checks) and on (the JDK's own, down to `ClassLoader.defineClass0`).
//
// Run: javac -d . L5W120LookupDefineChecks.java
//      java -cp . L5W120LookupDefineChecks                                         (HotSpot)
//      cratonvm --jdk-only -cp . L5W120LookupDefineChecks
//      CRATONVM_LOOKUP_DEFINE_JDK_BODY=1 cratonvm --jdk-only -cp . L5W120LookupDefineChecks
//      (and both with --compatible)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// same-package ok Gen1 loader-is-app true
// other-package java.lang.IllegalArgumentException: other/Gen2 not in same package as lookup class
// no-package-access java.lang.IllegalAccessException: Lookup does not have PACKAGE access
// bad-magic java.lang.ClassFormatError
// module-info java.lang.IllegalArgumentException
// duplicate java.lang.LinkageError
// hidden ok hidden=true nest-host L5W120LookupDefineChecks
// child-super ok super-loader-is-child true iface-loader-is-child true
// HOTSPOT_EXPECTED_END

import java.io.InputStream;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.util.function.Supplier;

public class L5W120LookupDefineChecks {
    public static byte[] plain(String name, String superName, String iface) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER)
                    .withSuperclass(ClassDesc.of(superName));
            if (iface != null) {
                cb.withInterfaceSymbols(ClassDesc.of(iface));
            }
            cb.withMethodBody("<init>", MethodTypeDesc.of(ConstantDescs.CD_void), ClassFile.ACC_PUBLIC, c -> c
                    .aload(0)
                    .invokespecial(ClassDesc.of(superName), "<init>", MethodTypeDesc.of(ConstantDescs.CD_void))
                    .return_());
        });
    }

    public static String err(Throwable t) {
        return t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
    }

    /** The class of a child-first loader that defines through its own lookup. */
    public static class ChildBase {
        public ChildBase() {
        }
    }

    public interface ChildIface {
    }

    public static class ChildDefiner implements Supplier<String> {
        public String get() {
            try {
                MethodHandles.Lookup lk = MethodHandles.lookup();
                byte[] b = plain("L5W120LookupDefineChecks$ChildGen", "L5W120LookupDefineChecks$ChildBase",
                        "L5W120LookupDefineChecks$ChildIface");
                Class<?> c = lk.defineClass(b);
                ClassLoader mine = ChildDefiner.class.getClassLoader();
                return "child-super ok super-loader-is-child " + (c.getSuperclass().getClassLoader() == mine)
                        + " iface-loader-is-child " + (c.getInterfaces()[0].getClassLoader() == mine);
            } catch (Throwable t) {
                return "child-super " + err(t);
            }
        }
    }

    static final class ChildFirst extends ClassLoader {
        ChildFirst() {
            super(L5W120LookupDefineChecks.class.getClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (name.startsWith("L5W120LookupDefineChecks$Child") && !name.endsWith("ChildGen")) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        try (InputStream in = getParent().getResourceAsStream(name + ".class")) {
                            byte[] b = in.readAllBytes();
                            c = defineClass(name, b, 0, b.length);
                        } catch (java.io.IOException e) {
                            throw new ClassNotFoundException(name, e);
                        }
                    }
                    return c;
                }
                return super.loadClass(name, resolve);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        try {
            Class<?> c = lk.defineClass(plain("Gen1", "java.lang.Object", null));
            System.out.println("same-package ok " + c.getName() + " loader-is-app "
                    + (c.getClassLoader() == L5W120LookupDefineChecks.class.getClassLoader()));
        } catch (Throwable t) {
            System.out.println("same-package " + err(t));
        }
        try {
            lk.defineClass(plain("other.Gen2", "java.lang.Object", null));
            System.out.println("other-package ok");
        } catch (Throwable t) {
            System.out.println("other-package " + err(t));
        }
        try {
            lk.dropLookupMode(MethodHandles.Lookup.PACKAGE).defineClass(plain("Gen3", "java.lang.Object", null));
            System.out.println("no-package-access ok");
        } catch (Throwable t) {
            System.out.println("no-package-access " + err(t));
        }
        try {
            byte[] b = plain("Gen4", "java.lang.Object", null);
            b[0] = 0;
            lk.defineClass(b);
            System.out.println("bad-magic ok");
        } catch (Throwable t) {
            System.out.println("bad-magic " + t.getClass().getName());
        }
        try {
            byte[] b = ClassFile.of().buildModule(java.lang.classfile.attribute.ModuleAttribute.of(
                    java.lang.constant.ModuleDesc.of("m.x"), mb -> { }));
            lk.defineClass(b);
            System.out.println("module-info ok");
        } catch (Throwable t) {
            System.out.println("module-info " + t.getClass().getName());
        }
        try {
            lk.defineClass(plain("Gen1", "java.lang.Object", null));
            System.out.println("duplicate ok");
        } catch (Throwable t) {
            System.out.println("duplicate " + t.getClass().getName());
        }
        try {
            MethodHandles.Lookup h = lk.defineHiddenClass(plain("Gen5", "java.lang.Object", null), true,
                    MethodHandles.Lookup.ClassOption.NESTMATE);
            System.out.println("hidden ok hidden=" + h.lookupClass().isHidden() + " nest-host "
                    + h.lookupClass().getNestHost().getName());
        } catch (Throwable t) {
            System.out.println("hidden " + err(t));
        }
        @SuppressWarnings("unchecked")
        Supplier<String> d = (Supplier<String>) new ChildFirst()
                .loadClass("L5W120LookupDefineChecks$ChildDefiner").getConstructor().newInstance();
        System.out.println(d.get());
    }
}
