// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 44, lane L4
// (`i42-L4-objectmethods-getters-the-native-linkage-still-hands-to-the-jdk`,
// row 4): `ldc` of a `CONSTANT_MethodHandle` naming a PROTECTED member of a
// class in another run-time package. From an UNRELATED class it is an
// `IllegalAccessError` (JVMS §5.4.3.5 / §5.4.4); from a SUBCLASS it links, and
// an instance member's handle has the subclass as its receiver type
// (`Lookup.restrictProtectedReceiver`).
//
// `lo.pc.LoProt` (public) has a protected instance field `f` ("x"), a
// protected static field `sf` (3), a protected static `ps()I` (7) and a
// protected instance `pv()I` (8). Each row generates a class whose static
// `make()` is `ldc <handle>` then `type()` and `invoke`, and runs it twice;
// `unrel-*` rows are unrelated classes of `lo.pd`, `sub-*` rows subclasses of
// `LoProt` in `lo.pd`. The module / loader part (` (` and what follows) is cut
// from each message.
//
// Run: javac -d out L4W44LdcProtectedHandle.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L4W44LdcProtectedHandle
//
// Expected HotSpot 25 output (default and -Xint, measured locally):
//   unrel-getter: java.lang.IllegalAccessError: member is protected: lo.pc.LoProt.f/java.lang.String/getField, from class lo.pd.LoUG <- java.lang.IllegalAccessException: member is protected: lo.pc.LoProt.f/java.lang.String/getField, from class lo.pd.LoUG | java.lang.IllegalAccessError: member is protected: lo.pc.LoProt.f/java.lang.String/getField, from class lo.pd.LoUG <- java.lang.IllegalAccessException: member is protected: lo.pc.LoProt.f/java.lang.String/getField, from class lo.pd.LoUG
//   unrel-static-getter: java.lang.IllegalAccessError: member is protected: lo.pc.LoProt.sf/int/getStatic, from class lo.pd.LoUSG <- java.lang.IllegalAccessException: member is protected: lo.pc.LoProt.sf/int/getStatic, from class lo.pd.LoUSG | java.lang.IllegalAccessError: member is protected: lo.pc.LoProt.sf/int/getStatic, from class lo.pd.LoUSG <- java.lang.IllegalAccessException: member is protected: lo.pc.LoProt.sf/int/getStatic, from class lo.pd.LoUSG
//   unrel-static: java.lang.IllegalAccessError: class lo.pd.LoUS tried to access protected method 'int lo.pc.LoProt.ps()' | java.lang.IllegalAccessError: class lo.pd.LoUS tried to access protected method 'int lo.pc.LoProt.ps()'
//   unrel-virtual: java.lang.IllegalAccessError: class lo.pd.LoUV tried to access protected method 'int lo.pc.LoProt.pv()' | java.lang.IllegalAccessError: class lo.pd.LoUV tried to access protected method 'int lo.pc.LoProt.pv()'
//   sub-getter: (LoSG)String x | (LoSG)String x
//   sub-static-getter: ()int 3 | ()int 3
//   sub-static: ()int 7 | ()int 7
//   sub-virtual: (LoSV)int 8 | (LoSV)int 8
//
// On the base `5248262b7` (`--jdk-only`) the four `unrel-*` rows link and
// print `(LoProt)String x`, `()int 3`, `()int 7`, `(LoProt)int 8`: the `ldc`
// check refused private and package-private members only. `sub-virtual`
// printed `(LoProt)int 8` there when `Lookup.findVirtual`'s native answers
// (no receiver restriction before wave 44).
//
// Positive control: CRATONVM_DBG_INDY_ALL=1 prints
//   `[indy-all] ldc method handle refused: member is protected: lo.pc.LoProt.f/java.lang.String/getField, from class lo.pd.LoUG`
// and `[indy-all] ldc method handle refused: class lo.pd.LoUV tried to access protected method 'int lo.pc.LoProt.pv()' ...`.
//
// `--compatible` (by design, as the other JVMS §5.4.4 member checks of
// `ldc`) does not refuse the `unrel-*` rows.
import java.lang.classfile.ClassFile;
import java.lang.constant.*;
import java.lang.reflect.InvocationTargetException;

public class L4W44LdcProtectedHandle {
    static final ClassDesc OBJ = ConstantDescs.CD_Object, STR = ConstantDescs.CD_String, INT = ConstantDescs.CD_int,
            MH = ConstantDescs.CD_MethodHandle, MT = ConstantDescs.CD_MethodType;
    static final ClassDesc T = ClassDesc.of("lo.pc.LoProt");

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W44LdcProtectedHandle.class.getClassLoader());
        }

        Class<?> define(String n, byte[] b) {
            return defineClass(n, b, 0, b.length);
        }
    }

    static final Loader L = new Loader();

    static byte[] target() {
        return ClassFile.of().build(T, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withField("f", STR, ClassFile.ACC_PROTECTED);
            cb.withField("sf", INT, ClassFile.ACC_PROTECTED | ClassFile.ACC_STATIC);
            cb.withMethodBody("<init>", ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC, c -> c.aload(0)
                    .invokespecial(OBJ, "<init>", ConstantDescs.MTD_void).aload(0).ldc("x").putfield(T, "f", STR)
                    .return_());
            cb.withMethodBody("<clinit>", ConstantDescs.MTD_void, ClassFile.ACC_STATIC,
                    c -> c.iconst_3().putstatic(T, "sf", INT).return_());
            cb.withMethodBody("ps", MethodTypeDesc.of(INT), ClassFile.ACC_PROTECTED | ClassFile.ACC_STATIC,
                    c -> c.bipush(7).ireturn());
            cb.withMethodBody("pv", MethodTypeDesc.of(INT), ClassFile.ACC_PROTECTED, c -> c.bipush(8).ireturn());
        });
    }

    static byte[] caller(String name, boolean sub, DirectMethodHandleDesc h, boolean needsInstance) {
        ClassDesc self = ClassDesc.of(name);
        ClassDesc inst = sub ? self : T;
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(sub ? T : OBJ);
            cb.withMethodBody("<init>", ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC, c -> c.aload(0)
                    .invokespecial(sub ? T : OBJ, "<init>", ConstantDescs.MTD_void).return_());
            cb.withMethodBody("make", MethodTypeDesc.of(OBJ), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, c -> {
                c.ldc(h).astore(0);
                c.aload(0).invokevirtual(MH, "type", MethodTypeDesc.of(MT))
                        .invokevirtual(OBJ, "toString", MethodTypeDesc.of(STR)).astore(1);
                c.aload(0);
                if (needsInstance) {
                    c.new_(inst).dup().invokespecial(inst, "<init>", ConstantDescs.MTD_void)
                            .invokevirtual(MH, "invoke", MethodTypeDesc.of(OBJ, OBJ));
                } else {
                    c.invokevirtual(MH, "invoke", MethodTypeDesc.of(OBJ));
                }
                c.astore(2);
                c.iconst_2().anewarray(OBJ).dup().iconst_0().aload(1).aastore().dup().iconst_1().aload(2).aastore()
                        .areturn();
            });
        });
    }

    static String describe(Throwable t) {
        StringBuilder sb = new StringBuilder();
        for (Throwable c = t; c != null; c = c.getCause()) {
            if (c != t) {
                sb.append(" <- ");
            }
            sb.append(c.getClass().getName());
            if (c.getMessage() != null) {
                String m = c.getMessage();
                int i = m.indexOf(" (");
                sb.append(": ").append(i >= 0 ? m.substring(0, i) : m);
            }
        }
        return sb.toString();
    }

    static String row(String name, boolean sub, DirectMethodHandleDesc h, boolean inst) {
        try {
            Class<?> c = L.define(name, caller(name, sub, h, inst));
            StringBuilder out = new StringBuilder();
            for (int i = 0; i < 2; i++) {
                if (i > 0) {
                    out.append(" | ");
                }
                try {
                    Object[] r = (Object[]) c.getMethod("make").invoke(null);
                    out.append(r[0]).append(' ').append(r[1]);
                } catch (InvocationTargetException e) {
                    out.append(describe(e.getCause()));
                }
            }
            return out.toString();
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    public static void main(String[] a) {
        L.define("lo.pc.LoProt", target());
        var getter = MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.GETTER, T, "f", STR);
        var sgetter = MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.STATIC_GETTER, T, "sf", INT);
        var st = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, T, "ps", MethodTypeDesc.of(INT));
        var vi = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.VIRTUAL, T, "pv", MethodTypeDesc.of(INT));
        System.out.println("unrel-getter: " + row("lo.pd.LoUG", false, getter, true));
        System.out.println("unrel-static-getter: " + row("lo.pd.LoUSG", false, sgetter, false));
        System.out.println("unrel-static: " + row("lo.pd.LoUS", false, st, false));
        System.out.println("unrel-virtual: " + row("lo.pd.LoUV", false, vi, true));
        System.out.println("sub-getter: " + row("lo.pd.LoSG", true, getter, true));
        System.out.println("sub-static-getter: " + row("lo.pd.LoSSG", true, sgetter, false));
        System.out.println("sub-static: " + row("lo.pd.LoSS", true, st, false));
        System.out.println("sub-virtual: " + row("lo.pd.LoSV", true, vi, true));
    }
}
