// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L2: the COMPILED half of items 3 and 4
// of docs/internal/fixed-bugs/interpreter-L4-invokespecial-and-invokeinterface-selection-diverge-from-hotspot-FIXED-20261001.md,
// which wave 30 fixed in the interpreter and probed once
// (`tools/probes/interp/L4/L4W30InterfaceSelectionEdges.java`). Here each
// site's method is called 30 000 times, so it is compiled; the `-cold` rows
// pass a well-behaved receiver for the first 20 000 calls and the erring one
// after, so the erring receiver reaches only compiled code.
//
//   private-recv-hot   `PvI.call(Object)`: `invokeinterface PvI.p` (a PRIVATE
//                      interface method) on a `PvX` that does not implement
//                      PvI -> IncompatibleClassChangeError every time
//   private-recv-cold  `PvGood implements PvI` first ("p"), then `PvX`
//   pkg-private-hot    `invokeinterface IpI.m` on `IpD`, whose `m` is
//                      package-private -> IllegalAccessError every time
//   pkg-private-cold   `IpGood` (public m, "G") first, then `IpD`
//
// Filed as docs/internal/fixed-bugs/interpreter-L2-compiled-invokeinterface-selection-edges-FIXED-20261004.md.
//
// Run: javac -d out L2W37InterfaceSelectionHot.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L2W37InterfaceSelectionHot
//
// Expected HotSpot 25 output (default and -Xint):
//   private-recv-hot: ok=0 IncompatibleClassChangeError=30000 IllegalAccessError=0 other=0
//   private-recv-cold: ok=20000 IncompatibleClassChangeError=10000 IllegalAccessError=0 other=0
//   pkg-private-hot: ok=0 IncompatibleClassChangeError=0 IllegalAccessError=30000 other=0
//   pkg-private-cold: ok=20000 IncompatibleClassChangeError=0 IllegalAccessError=10000 other=0
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.HashMap;
import java.util.Map;

public class L2W37InterfaceSelectionHot {
    static final MethodTypeDesc STR = MethodTypeDesc.of(ConstantDescs.CD_String);
    static final MethodTypeDesc OBJ_STR = MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_Object);

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(L2W37InterfaceSelectionHot.class.getClassLoader());
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

    static byte[] impl(String name, String iface, int mFlags, String answer) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            if (iface != null) {
                cb.withInterfaceSymbols(ClassDesc.of(iface));
            }
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                            ConstantDescs.MTD_void).return_());
            if (answer != null) {
                cb.withMethodBody("m", STR, mFlags, code -> code.ldc(answer).areturn());
            }
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

    static byte[] privateIface(String name) {
        ClassDesc pvi = ClassDesc.of(name);
        return ClassFile.of().build(pvi, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT);
            cb.withMethodBody("p", STR, ClassFile.ACC_PRIVATE, code -> code.ldc("p").areturn());
            cb.withMethodBody("call", OBJ_STR, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.aload(0).invokeinterface(pvi, "p", STR).areturn());
        });
    }

    static String run(String label, MethodHandle call, Object good, Object bad, String want) throws Throwable {
        int ok = 0;
        int icce = 0;
        int iae = 0;
        int other = 0;
        int unexpected = 0;
        String firstUnexpected = null;
        for (int i = 0; i < 30_000; i++) {
            Object recv = good != null && i < 20_000 ? good : bad;
            try {
                String r = (String) call.invokeExact(recv);
                if (want.equals(r)) {
                    ok++;
                } else {
                    other++;
                }
            } catch (IllegalAccessError e) {
                iae++;
            } catch (IncompatibleClassChangeError e) {
                icce++;
            } catch (Throwable t) {
                // Not expected on HotSpot; named so a CratonVM divergence is
                // not a silent crash of the whole probe (wave 37 host run,
                // `--compatible`).
                unexpected++;
                if (firstUnexpected == null) {
                    firstUnexpected = t.toString();
                }
            }
        }
        return label + ": ok=" + ok + " IncompatibleClassChangeError=" + icce + " IllegalAccessError=" + iae
                + " other=" + other
                + (unexpected == 0 ? "" : " unexpected=" + unexpected + " first=" + firstUnexpected);
    }

    static MethodHandle callOf(Class<?> c) throws Exception {
        return MethodHandles.lookup().findStatic(c, "call", MethodType.methodType(String.class, Object.class));
    }

    static String privateRow(String label, boolean cold) throws Throwable {
        Loader l = new Loader();
        l.bytes.put("PvI", privateIface("PvI"));
        l.bytes.put("PvX", impl("PvX", null, 0, null));
        l.bytes.put("PvGood", impl("PvGood", "PvI", 0, null));
        Object bad = l.loadClass("PvX").getConstructor().newInstance();
        Object good = cold ? l.loadClass("PvGood").getConstructor().newInstance() : null;
        return run(label, callOf(l.loadClass("PvI")), good, bad, "p");
    }

    static String pkgRow(String label, boolean cold) throws Throwable {
        Loader l = new Loader();
        l.bytes.put("IpI", iface("IpI"));
        l.bytes.put("IpD", impl("IpD", "IpI", 0, "D"));
        l.bytes.put("IpGood", impl("IpGood", "IpI", ClassFile.ACC_PUBLIC, "G"));
        l.bytes.put("IpCaller", caller("IpCaller", "IpI"));
        Object bad = l.loadClass("IpD").getConstructor().newInstance();
        Object good = cold ? l.loadClass("IpGood").getConstructor().newInstance() : null;
        return run(label, callOf(l.loadClass("IpCaller")), good, bad, "G");
    }

    public static void main(String[] args) throws Throwable {
        System.out.println(privateRow("private-recv-hot", false));
        System.out.println(privateRow("private-recv-cold", true));
        System.out.println(pkgRow("pkg-private-hot", false));
        System.out.println(pkgRow("pkg-private-cold", true));
    }
}
