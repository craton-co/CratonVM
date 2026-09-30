// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 30 (orchestrator): two `invokeinterface`
// selection edges of
// docs/internal/fixed-bugs/interpreter-L4-invokespecial-and-invokeinterface-selection-diverge-from-hotspot-FIXED-20261001.md
// (items 3 and 4). javac refuses both shapes, so every class is generated
// with the java.lang.classfile API and defined by one loader.
//
//   pkg-private  `IpI { String m(); }`, `IpD implements IpI` declares `m`
//                package-private; `invokeinterface IpI.m` on an `IpD`
//                -> IllegalAccessError: 'java.lang.String IpD.m()'
//   protected    the same with `m` protected -> the same error
//   public       the control: `m` public -> "D"
//   private-recv `PvI` has a private instance method `p` and a static
//                `call(Object)` doing `invokeinterface PvI.p` on its argument;
//                called with a `PvX` that does not implement `PvI`
//                -> IncompatibleClassChangeError: Class PvX does not
//                   implement the requested interface PvI
//
// Before wave 30 CratonVM ran `IpD.m` for the first two rows (selection
// accepted any non-private method for an interface reference) and `PvI.p`
// for the last (a private target made the site special and skipped the
// receiver check).
//
// Run: javac -d out L4W30InterfaceSelectionEdges.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W30InterfaceSelectionEdges
//
// Expected HotSpot 25 output (default and -Xint):
//   pkg-private: java.lang.IllegalAccessError: 'java.lang.String IpD0.m()'
//   protected: java.lang.IllegalAccessError: 'java.lang.String IpD4.m()'
//   public: D
//   private-recv: java.lang.IncompatibleClassChangeError: Class PvX does not implement the requested interface PvI
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.util.HashMap;
import java.util.Map;

public class L4W30InterfaceSelectionEdges {
    static final MethodTypeDesc STR = MethodTypeDesc.of(ConstantDescs.CD_String);
    static final MethodTypeDesc OBJ_STR = MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_Object);

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(L4W30InterfaceSelectionEdges.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = bytes.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }
    }

    static byte[] iface(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT);
            cb.withMethod("m", STR, ClassFile.ACC_PUBLIC | ClassFile.ACC_ABSTRACT, mb -> { });
        });
    }

    static byte[] impl(String name, String iface, int mFlags) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(ClassDesc.of(iface));
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                            ConstantDescs.MTD_void).return_());
            cb.withMethodBody("m", STR, mFlags, code -> code.ldc("D").areturn());
        });
    }

    static byte[] caller(String name, String iface) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("call", OBJ_STR, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.aload(0).checkcast(ClassDesc.of(iface))
                            .invokeinterface(ClassDesc.of(iface), "m", STR).areturn());
        });
    }

    static byte[] privateIface() {
        ClassDesc pvi = ClassDesc.of("PvI");
        return ClassFile.of().build(pvi, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT);
            cb.withMethodBody("p", STR, ClassFile.ACC_PRIVATE, code -> code.ldc("p").areturn());
            cb.withMethodBody("call", OBJ_STR, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.aload(0).invokeinterface(pvi, "p", STR).areturn());
        });
    }

    static byte[] plain(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                            ConstantDescs.MTD_void).return_());
        });
    }

    static String describe(Throwable t) {
        Throwable e = t instanceof InvocationTargetException ? t.getCause() : t;
        return e.getMessage() == null ? e.getClass().getName() : e.getClass().getName() + ": " + e.getMessage();
    }

    static String selection(int flags) {
        String suffix = Integer.toString(flags);
        Loader l = new Loader();
        l.bytes.put("IpI" + suffix, iface("IpI" + suffix));
        l.bytes.put("IpD" + suffix, impl("IpD" + suffix, "IpI" + suffix, flags));
        l.bytes.put("IpCaller" + suffix, caller("IpCaller" + suffix, "IpI" + suffix));
        try {
            Object d = l.loadClass("IpD" + suffix).getConstructor().newInstance();
            return (String) l.loadClass("IpCaller" + suffix).getMethod("call", Object.class).invoke(null, d);
        } catch (Throwable t) {
            return describe(t);
        }
    }

    static String privateReceiver() {
        Loader l = new Loader();
        l.bytes.put("PvI", privateIface());
        l.bytes.put("PvX", plain("PvX"));
        try {
            Object x = l.loadClass("PvX").getConstructor().newInstance();
            return (String) l.loadClass("PvI").getMethod("call", Object.class).invoke(null, x);
        } catch (Throwable t) {
            return describe(t);
        }
    }

    public static void main(String[] args) {
        System.out.println("pkg-private: " + selection(0));
        System.out.println("protected: " + selection(ClassFile.ACC_PROTECTED));
        System.out.println("public: " + selection(ClassFile.ACC_PUBLIC));
        System.out.println("private-recv: " + privateReceiver());
    }
}
