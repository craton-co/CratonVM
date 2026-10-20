// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 32 (orchestrator): `invokespecial` selection as
// HotSpot's `LinkResolver::runtime_resolve_special_method`, and an
// `invokeinterface` of `Object.clone()` through an interface
// (docs/internal/fixed-bugs/interpreter-L4-invokespecial-and-invokeinterface-selection-diverge-from-hotspot-FIXED-20261001.md,
// items 1 and 5). javac refuses every call shape here, so each caller (and
// the two classes javac will not compile, `ScB` and `SsB`) is generated with
// the java.lang.classfile API and defined by one loader.
//
//   abstract-super  `SaCaller extends SaB extends SaA`; `SaB` re-declares
//                   `m` abstract over `SaA.m`; `invokespecial SaB.m`
//                   -> AbstractMethodError naming SaB.m
//   iface-super     `SiCaller implements SiI extends SiJ`; `SiI` re-declares
//                   the default `m` abstract; `invokespecial SiI.m`
//                   -> AbstractMethodError naming SiI.m
//   conflict-super  `ScCaller extends ScB implements ScK1, ScK2` (two
//                   unrelated defaults); `invokespecial ScB.m`
//                   -> IncompatibleClassChangeError: Conflicting default methods
//   static-between  `SsCaller extends SsB extends SsA`; `SsB` declares a
//                   STATIC `m`; `invokespecial SsA.m` -> "A" (the static is
//                   skipped)
//   iface-clone     `invokeinterface IoI.clone()` on an `IoX implements IoI,
//                   Cloneable` -> NoSuchMethodError (Object.clone is
//                   protected, so an interface reference does not reach it)
//   iface-hashCode  the control: `invokeinterface IoI.hashCode()` -> "ok"
//
// Before wave 32 CratonVM ran `SaA.m`, `SiJ.m`, `ScK1.m`, the static `SsB.m`
// and `Object.clone` for the first five rows. Rows 1-4 are enforced under
// `--jdk-only` (the default); `--compatible` keeps its old answers. Row 5 is
// fixed in both modes.
//
// Run: javac -d out L4W32SpecialSelection.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W32SpecialSelection
//
// Expected HotSpot 25 output (default and -Xint):
//   abstract-super: java.lang.AbstractMethodError: 'java.lang.String L4W32SpecialSelection$SaB.m()'
//   iface-super: java.lang.AbstractMethodError: 'java.lang.String L4W32SpecialSelection$SiI.m()'
//   conflict-super: java.lang.IncompatibleClassChangeError: Conflicting default methods: L4W32SpecialSelection$ScK1.m L4W32SpecialSelection$ScK2.m
//   static-between: A
//   iface-clone: java.lang.NoSuchMethodError: 'java.lang.Object L4W32SpecialSelection$IoI.clone()'
//   iface-hashCode: ok
import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.util.function.Consumer;

public class L4W32SpecialSelection {
    public static class SaA {
        public String m() {
            return "A";
        }
    }

    public abstract static class SaB extends SaA {
        @Override
        public abstract String m();
    }

    public interface SiJ {
        default String m() {
            return "J";
        }
    }

    public interface SiI extends SiJ {
        @Override
        String m();
    }

    public interface ScK1 {
        default String m() {
            return "K1";
        }
    }

    public interface ScK2 {
        default String m() {
            return "K2";
        }
    }

    public static class SsA {
        public String m() {
            return "A";
        }
    }

    public interface IoI {
    }

    public static class IoX implements IoI, Cloneable {
    }

    static final MethodTypeDesc STR = MethodTypeDesc.of(ConstantDescs.CD_String);
    static final MethodTypeDesc OBJ = MethodTypeDesc.of(ConstantDescs.CD_Object);
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);

    static ClassDesc cd(Class<?> c) {
        return ClassDesc.of(c.getName());
    }

    static byte[] build(String name, Consumer<ClassBuilder> body) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            body.accept(cb);
        });
    }

    /** A public no-arg constructor calling `sup`'s. */
    static void ctor(ClassBuilder cb, ClassDesc sup) {
        cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                code -> code.aload(0).invokespecial(sup, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void).return_());
    }

    /** `public String m() { return tag; }` */
    static void m(ClassBuilder cb, String tag) {
        cb.withMethodBody("m", STR, ClassFile.ACC_PUBLIC, code -> code.ldc(tag).areturn());
    }

    /** `public String call() { return <invokespecial owner.m()>; }` */
    static void call(ClassBuilder cb, ClassDesc owner, boolean isInterface) {
        cb.withMethodBody("call", STR, ClassFile.ACC_PUBLIC,
                code -> code.aload(0).invokespecial(owner, "m", STR, isInterface).areturn());
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W32SpecialSelection.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static String callOn(String name, byte[] bytes) {
        try {
            Class<?> c = LOADER.define(name, bytes);
            Object o = c.getConstructor().newInstance();
            return (String) c.getMethod("call").invoke(o);
        } catch (Throwable t) {
            return describe(t);
        }
    }

    static String describe(Throwable t) {
        Throwable e = t instanceof InvocationTargetException ? t.getCause() : t;
        return e.getMessage() == null ? e.getClass().getName() : e.getClass().getName() + ": " + e.getMessage();
    }

    static String abstractSuper() {
        ClassDesc sab = cd(SaB.class);
        return callOn("SaCaller", build("SaCaller", cb -> {
            cb.withSuperclass(sab);
            ctor(cb, sab);
            m(cb, "C");
            call(cb, sab, false);
        }));
    }

    static String ifaceSuper() {
        ClassDesc sii = cd(SiI.class);
        return callOn("SiCaller", build("SiCaller", cb -> {
            cb.withSuperclass(ConstantDescs.CD_Object);
            cb.withInterfaceSymbols(sii);
            ctor(cb, ConstantDescs.CD_Object);
            m(cb, "C");
            call(cb, sii, true);
        }));
    }

    static String conflictSuper() {
        try {
            LOADER.define("ScB", ClassFile.of().build(ClassDesc.of("ScB"), cb -> {
                cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER | ClassFile.ACC_ABSTRACT);
                cb.withSuperclass(ConstantDescs.CD_Object);
                cb.withInterfaceSymbols(cd(ScK1.class), cd(ScK2.class));
                ctor(cb, ConstantDescs.CD_Object);
            }));
        } catch (Throwable t) {
            return "ScB: " + describe(t);
        }
        ClassDesc scb = ClassDesc.of("ScB");
        return callOn("ScCaller", build("ScCaller", cb -> {
            cb.withSuperclass(scb);
            ctor(cb, scb);
            call(cb, scb, false);
        }));
    }

    static String staticBetween() {
        ClassDesc ssa = cd(SsA.class);
        try {
            LOADER.define("SsB", build("SsB", cb -> {
                cb.withSuperclass(ssa);
                ctor(cb, ssa);
                cb.withMethodBody("m", STR, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                        code -> code.ldc("B").areturn());
            }));
        } catch (Throwable t) {
            return "SsB: " + describe(t);
        }
        ClassDesc ssb = ClassDesc.of("SsB");
        return callOn("SsCaller", build("SsCaller", cb -> {
            cb.withSuperclass(ssb);
            ctor(cb, ssb);
            call(cb, ssa, false);
        }));
    }

    static String ifaceObjectMember(String name, String method, MethodTypeDesc type) {
        ClassDesc ioi = cd(IoI.class);
        try {
            Class<?> c = LOADER.define(name, build(name, cb -> {
                cb.withSuperclass(ConstantDescs.CD_Object);
                ctor(cb, ConstantDescs.CD_Object);
                cb.withMethodBody("run", MethodTypeDesc.of(ConstantDescs.CD_Object, ioi),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                            code.aload(0).invokeinterface(ioi, method, type);
                            if (type.returnType().isPrimitive()) {
                                code.pop().ldc("ok");
                            }
                            code.areturn();
                        });
            }));
            Object r = c.getMethod("run", IoI.class).invoke(null, new IoX());
            return r instanceof String s ? s : "returned " + (r == null ? "null" : r.getClass().getSimpleName());
        } catch (Throwable t) {
            return describe(t);
        }
    }

    public static void main(String[] args) {
        System.out.println("abstract-super: " + abstractSuper());
        System.out.println("iface-super: " + ifaceSuper());
        System.out.println("conflict-super: " + conflictSuper());
        System.out.println("static-between: " + staticBetween());
        System.out.println("iface-clone: " + ifaceObjectMember("IoCloneCaller", "clone", OBJ));
        System.out.println("iface-hashCode: " + ifaceObjectMember("IoHashCaller", "hashCode", INT));
    }
}
