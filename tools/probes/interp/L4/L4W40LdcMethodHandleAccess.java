// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 40, lane L4 (the note of
// `i38-L4-objectmethods-native-linkage-ignores-its-getters`, What remains):
// `ldc` of a `CONSTANT_MethodHandle` naming ANOTHER class's private member is
// an `IllegalAccessError`, JVMS §5.4.3.5 / §5.4.4, recorded against the entry
// (the second execution rethrows it): for a field `MemberName`'s text with an
// `IllegalAccessException` cause, for a method `LinkResolver`'s text without
// one. CratonVM resolved the constant through `Lookup.find*`, whose native
// access gate admits every member for a full-power lookup, so the handle was
// minted and the private member read or called.
//
// Each row generates (java.lang.classfile) a class `Lo<Row>` whose static
// `make()` is `ldc <handle>; invoke` and runs it twice. `LoTarget` has a
// private field `b` ("x" in the instance `make` builds), a private static
// `ps()I` (7) and a private instance `pv()I` (8). `LoHost` / `LoHost$Mate` are
// nestmates (NestHost / NestMembers).
//
// Run: javac -d out L4W40LdcMethodHandleAccess.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L4W40LdcMethodHandleAccess
//
// Expected HotSpot 25 output (default and -Xint); the module / loader part
// (` (` and what follows) is cut from each message:
//   foreign-getter: java.lang.IllegalAccessError: member is private: LoTarget.b/java.lang.String/getField, from class LoForeignGetter / java.lang.IllegalAccessException | java.lang.IllegalAccessError: member is private: LoTarget.b/java.lang.String/getField, from class LoForeignGetter / java.lang.IllegalAccessException
//   foreign-static: java.lang.IllegalAccessError: class LoForeignStatic tried to access private method 'int LoTarget.ps()' | java.lang.IllegalAccessError: class LoForeignStatic tried to access private method 'int LoTarget.ps()'
//   foreign-virtual: java.lang.IllegalAccessError: class LoForeignVirtual tried to access private method 'int LoTarget.pv()' | java.lang.IllegalAccessError: class LoForeignVirtual tried to access private method 'int LoTarget.pv()'
//
// Positive control: CRATONVM_DBG_INDY_ALL=1 prints
//   `[indy-all] ldc method handle refused: member is private: LoTarget.b/java.lang.String/getField, from class LoForeignGetter`
//   own-getter: x | x
//   nestmate-getter: h | h
//
// `--compatible` (by design, as the other JVMS §5.4.4 member checks: the
// check is enforced under `--jdk-only` only) answers `x | x`, `7 | 7`,
// `8 | 8` on the three foreign rows.
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.NestHostAttribute;
import java.lang.classfile.attribute.NestMembersAttribute;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;

public class L4W40LdcMethodHandleAccess {
    static final ClassDesc OBJ = ConstantDescs.CD_Object;
    static final ClassDesc STR = ConstantDescs.CD_String;
    static final ClassDesc INT = ConstantDescs.CD_int;
    static final ClassDesc MH = ConstantDescs.CD_MethodHandle;
    static final ClassDesc TARGET = ClassDesc.of("LoTarget");
    static final ClassDesc HOST = ClassDesc.of("LoHost");
    static final ClassDesc MATE = ClassDesc.of("LoHost$Mate");

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W40LdcMethodHandleAccess.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    /** `LoTarget`: private `b`, private static `ps()I`, private `pv()I`, and its own `make()`. */
    static byte[] target() {
        return ClassFile.of().build(TARGET, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withField("b", STR, ClassFile.ACC_PRIVATE);
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(OBJ, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .aload(0).ldc("x").putfield(TARGET, "b", STR).return_());
            cb.withMethodBody("ps", MethodTypeDesc.of(INT), ClassFile.ACC_PRIVATE | ClassFile.ACC_STATIC,
                    code -> code.bipush(7).ireturn());
            cb.withMethodBody("pv", MethodTypeDesc.of(INT), ClassFile.ACC_PRIVATE,
                    code -> code.bipush(8).ireturn());
            DirectMethodHandleDesc own = MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.GETTER, TARGET, "b",
                    STR);
            cb.withMethodBody("make", MethodTypeDesc.of(OBJ), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.ldc(own).new_(TARGET).dup()
                            .invokespecial(TARGET, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .invokevirtual(MH, "invoke", MethodTypeDesc.of(OBJ, TARGET)).areturn());
        });
    }

    /** A plain class whose `make()` is `ldc handle` then its call. */
    static byte[] foreign(String name, DirectMethodHandleDesc handle) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(OBJ), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                code.ldc(handle);
                switch (handle.kind()) {
                    case STATIC -> code.invokevirtual(MH, "invoke", MethodTypeDesc.of(OBJ));
                    default -> code.new_(TARGET).dup()
                            .invokespecial(TARGET, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .invokevirtual(MH, "invoke", MethodTypeDesc.of(OBJ, TARGET));
                }
                code.areturn();
            });
        });
    }

    static byte[] host() {
        return ClassFile.of().build(HOST, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.with(NestMembersAttribute.ofSymbols(MATE));
            cb.withField("h", STR, ClassFile.ACC_PRIVATE);
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(OBJ, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .aload(0).ldc("h").putfield(HOST, "h", STR).return_());
        });
    }

    static byte[] mate() {
        DirectMethodHandleDesc getter = MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.GETTER, HOST, "h", STR);
        return ClassFile.of().build(MATE, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.with(NestHostAttribute.of(HOST));
            cb.withMethodBody("make", MethodTypeDesc.of(OBJ), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.ldc(getter).new_(HOST).dup()
                            .invokespecial(HOST, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .invokevirtual(MH, "invoke", MethodTypeDesc.of(OBJ, HOST)).areturn());
        });
    }

    static String shown(Throwable t) {
        String m = String.valueOf(t.getMessage());
        int cut = m.indexOf(" (");
        if (cut >= 0) {
            m = m.substring(0, cut);
        }
        Throwable c = t.getCause();
        return t.getClass().getName() + ": " + m + (c == null ? "" : " / " + c.getClass().getName());
    }

    static String twice(Class<?> c) {
        StringBuilder out = new StringBuilder();
        for (int i = 0; i < 2; i++) {
            if (i > 0) {
                out.append(" | ");
            }
            try {
                out.append(c.getMethod("make").invoke(null));
            } catch (InvocationTargetException e) {
                out.append(shown(e.getCause()));
            } catch (Throwable t) {
                out.append("call: ").append(t);
            }
        }
        return out.toString();
    }

    static String row(String name, DirectMethodHandleDesc handle) {
        try {
            return twice(LOADER.define(name, foreign(name, handle)));
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    public static void main(String[] args) throws Throwable {
        Class<?> target = LOADER.define("LoTarget", target());
        System.out.println("foreign-getter: " + row("LoForeignGetter",
                MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.GETTER, TARGET, "b", STR)));
        System.out.println("foreign-static: " + row("LoForeignStatic",
                MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, TARGET, "ps", MethodTypeDesc.of(INT))));
        System.out.println("foreign-virtual: " + row("LoForeignVirtual",
                MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.VIRTUAL, TARGET, "pv", MethodTypeDesc.of(INT))));
        System.out.println("own-getter: " + twice(target));
        LOADER.define("LoHost", host());
        System.out.println("nestmate-getter: " + twice(LOADER.define("LoHost$Mate", mate())));
    }
}
