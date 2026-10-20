// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L4 (the `ldc` note of
// `i38-L4-objectmethods-native-linkage-ignores-its-getters`): `ldc` of a
// `CONSTANT_MethodHandle` naming a PACKAGE-PRIVATE member of a class in
// another run-time package is an `IllegalAccessError` (JVMS §5.4.3.5 /
// §5.4.4), recorded against the entry. Wave 40 refused only PRIVATE members
// under `--jdk-only`; a package-private one was minted and read or called.
//
// `lo.pa.LoPkg` (public) has a package-private instance field `f` ("x"), a
// package-private static field `sf` (0), a package-private static `ps()I` (7)
// and instance `pv()I` (8). Each row generates a class whose static `make()`
// is `ldc <handle>; invoke` and runs it twice; `foreign-*` rows live in
// `lo.pb`, `same-*` rows in `lo.pa` (admitted). The module / loader part
// (` (` and what follows) is cut from each message.
//
// Run: javac -d out L4W42LdcPackagePrivateHandle.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L4W42LdcPackagePrivateHandle
//
// Expected HotSpot 25 output (default and -Xint, measured locally):
//   foreign-getter: java.lang.IllegalAccessError: member is private to package: lo.pa.LoPkg.f/java.lang.String/getField, from class lo.pb.LoFG <- java.lang.IllegalAccessException: member is private to package: lo.pa.LoPkg.f/java.lang.String/getField, from class lo.pb.LoFG | java.lang.IllegalAccessError: member is private to package: lo.pa.LoPkg.f/java.lang.String/getField, from class lo.pb.LoFG <- java.lang.IllegalAccessException: member is private to package: lo.pa.LoPkg.f/java.lang.String/getField, from class lo.pb.LoFG
//   foreign-static-getter: java.lang.IllegalAccessError: member is private to package: lo.pa.LoPkg.sf/int/getStatic, from class lo.pb.LoFSG <- java.lang.IllegalAccessException: member is private to package: lo.pa.LoPkg.sf/int/getStatic, from class lo.pb.LoFSG | java.lang.IllegalAccessError: member is private to package: lo.pa.LoPkg.sf/int/getStatic, from class lo.pb.LoFSG <- java.lang.IllegalAccessException: member is private to package: lo.pa.LoPkg.sf/int/getStatic, from class lo.pb.LoFSG
//   foreign-static: java.lang.IllegalAccessError: class lo.pb.LoFS tried to access method 'int lo.pa.LoPkg.ps()' | java.lang.IllegalAccessError: class lo.pb.LoFS tried to access method 'int lo.pa.LoPkg.ps()'
//   foreign-virtual: java.lang.IllegalAccessError: class lo.pb.LoFV tried to access method 'int lo.pa.LoPkg.pv()' | java.lang.IllegalAccessError: class lo.pb.LoFV tried to access method 'int lo.pa.LoPkg.pv()'
//   same-getter: x | x
//   same-static: 7 | 7
//   same-virtual: 8 | 8
//
// Positive control: CRATONVM_DBG_INDY_ALL=1 prints
//   `[indy-all] ldc method handle refused: member is private to package: lo.pa.LoPkg.f/java.lang.String/getField, from class lo.pb.LoFG`
// and, for `foreign-static`, `[indy-all] ldc method handle refused: class lo.pb.LoFS tried to access method 'int lo.pa.LoPkg.ps()' ...`.
//
// `--compatible` (by design, as the other JVMS §5.4.4 member checks) does
// not check the member and, not measured, should answer
// `x | x`, `0 | 0`, `7 | 7`, `8 | 8` on the four foreign rows.
import java.lang.classfile.ClassFile;
import java.lang.constant.*;
import java.lang.reflect.InvocationTargetException;

public class L4W42LdcPackagePrivateHandle {
    static final ClassDesc OBJ = ConstantDescs.CD_Object, STR = ConstantDescs.CD_String, INT = ConstantDescs.CD_int, MH = ConstantDescs.CD_MethodHandle;
    static final ClassDesc T = ClassDesc.of("lo.pa.LoPkg");
    static final class Loader extends ClassLoader {
        Loader() { super(L4W42LdcPackagePrivateHandle.class.getClassLoader()); }
        Class<?> define(String n, byte[] b) { return defineClass(n, b, 0, b.length); }
    }
    static final Loader L = new Loader();
    static byte[] target() {
        return ClassFile.of().build(T, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withField("f", STR, 0);
            cb.withField("sf", INT, ClassFile.ACC_STATIC);
            cb.withMethodBody("<init>", ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC, c -> c.aload(0)
                .invokespecial(OBJ, "<init>", ConstantDescs.MTD_void).aload(0).ldc("x").putfield(T, "f", STR).return_());
            cb.withMethodBody("ps", MethodTypeDesc.of(INT), ClassFile.ACC_STATIC, c -> c.bipush(7).ireturn());
            cb.withMethodBody("pv", MethodTypeDesc.of(INT), 0, c -> c.bipush(8).ireturn());
        });
    }
    static byte[] caller(String name, DirectMethodHandleDesc h, boolean needsInstance) {
        ClassDesc self = ClassDesc.of(name);
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(OBJ), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, c -> {
                c.ldc(h);
                if (needsInstance) {
                    c.new_(T).dup().invokespecial(T, "<init>", ConstantDescs.MTD_void)
                     .invokevirtual(MH, "invoke", MethodTypeDesc.of(OBJ, OBJ)).areturn();
                } else {
                    c.invokevirtual(MH, "invoke", MethodTypeDesc.of(OBJ)).areturn();
                }
            });
        });
    }
    static String describe(Throwable t) {
        StringBuilder sb = new StringBuilder();
        for (Throwable c = t; c != null; c = c.getCause()) {
            if (c != t) sb.append(" <- ");
            sb.append(c.getClass().getName());
            if (c.getMessage() != null) { String m = c.getMessage(); int i = m.indexOf(" ("); sb.append(": ").append(i >= 0 ? m.substring(0, i) : m); }
        }
        return sb.toString();
    }
    static String row(String name, DirectMethodHandleDesc h, boolean inst) {
        try {
            Class<?> c = L.define(name, caller(name, h, inst));
            StringBuilder out = new StringBuilder();
            for (int i = 0; i < 2; i++) {
                if (i > 0) out.append(" | ");
                try { out.append(c.getMethod("make").invoke(null)); }
                catch (InvocationTargetException e) { out.append(describe(e.getCause())); }
            }
            return out.toString();
        } catch (Throwable t) { return "setup: " + t; }
    }
    public static void main(String[] a) {
        L.define("lo.pa.LoPkg", target());
        var getter = MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.GETTER, T, "f", STR);
        var sgetter = MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.STATIC_GETTER, T, "sf", INT);
        var st = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, T, "ps", MethodTypeDesc.of(INT));
        var vi = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.VIRTUAL, T, "pv", MethodTypeDesc.of(INT));
        System.out.println("foreign-getter: " + row("lo.pb.LoFG", getter, true));
        System.out.println("foreign-static-getter: " + row("lo.pb.LoFSG", sgetter, false));
        System.out.println("foreign-static: " + row("lo.pb.LoFS", st, false));
        System.out.println("foreign-virtual: " + row("lo.pb.LoFV", vi, true));
        System.out.println("same-getter: " + row("lo.pa.LoSG", getter, true));
        System.out.println("same-static: " + row("lo.pa.LoSS", st, false));
        System.out.println("same-virtual: " + row("lo.pa.LoSV", vi, true));
    }
}
