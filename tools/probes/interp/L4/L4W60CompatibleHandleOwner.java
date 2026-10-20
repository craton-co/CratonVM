// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L4 -- proposal
// `interpreter-L4-proposal-compatible-handles-record-their-owner-FIXED-20261024`: under
// `--compatible` a static method / field handle and a static VarHandle of a
// class a BUILT-IN loader defined recorded no owner `ClassId`, so every call
// resolved the class by NAME. Two classes named `Tw` exist here: the
// application loader's (`A`, defined through `Lookup.defineClass`) and a
// user-defined loader's (`U`). The handles are minted against the
// application's `Tw` and invoked from inside the user loader's `Tw`, whose
// loader resolves the name `Tw` to itself. HotSpot (and `--jdk-only`, which
// always recorded the owner) answers the application class every time.
//
// Positive control: `CRATONVM_MH_COMPATIBLE_RECORD_OWNER=0 cratonvm
// --compatible` restores the by-name owner; compare.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// static-first: A
// getter: A
// setter: A2
// varhandle: A2
// varhandle-set: A3
// user-untouched: U
// app-direct: A3
// static-again: A
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W60CompatibleHandleOwner`.
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class L4W60CompatibleHandleOwner {
    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W60CompatibleHandleOwner.class.getClassLoader());
        }

        Class<?> define(byte[] b) {
            return defineClass("Tw", b, 0, b.length);
        }
    }

    static final ClassDesc SELF = ClassDesc.of("Tw");
    static final ClassDesc MH = ClassDesc.of("java.lang.invoke.MethodHandle");
    static final ClassDesc VH = ClassDesc.of("java.lang.invoke.VarHandle");

    // public class Tw {
    //   public static String V = "<tag>";
    //   public static String id() { return "<tag>"; }
    //   public static String get(MethodHandle h) { return (String) h.invokeExact(); }
    //   public static void set(MethodHandle h, String v) { h.invokeExact(v); }
    //   public static String vget(VarHandle h) { return (String) h.get(); }
    //   public static void vset(VarHandle h, String v) { h.set(v); }
    // }
    static byte[] tw(String tag) {
        return ClassFile.of().build(SELF, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withField("V", ConstantDescs.CD_String, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC);
            cb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_STATIC,
                    code -> code.ldc(tag).putstatic(SELF, "V", ConstantDescs.CD_String).return_());
            cb.withMethodBody("id", MethodTypeDesc.of(ConstantDescs.CD_String),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.ldc(tag).areturn());
            MethodTypeDesc str = MethodTypeDesc.of(ConstantDescs.CD_String);
            MethodTypeDesc strV = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String);
            int ps = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
            cb.withMethodBody("get", MethodTypeDesc.of(ConstantDescs.CD_String, MH), ps,
                    code -> code.aload(0).invokevirtual(MH, "invokeExact", str).areturn());
            cb.withMethodBody("set", MethodTypeDesc.of(ConstantDescs.CD_void, MH, ConstantDescs.CD_String), ps,
                    code -> code.aload(0).aload(1).invokevirtual(MH, "invokeExact", strV).return_());
            cb.withMethodBody("vget", MethodTypeDesc.of(ConstantDescs.CD_String, VH), ps,
                    code -> code.aload(0).invokevirtual(VH, "get", str).areturn());
            cb.withMethodBody("vset", MethodTypeDesc.of(ConstantDescs.CD_void, VH, ConstantDescs.CD_String), ps,
                    code -> code.aload(0).aload(1).invokevirtual(VH, "set", strV).return_());
        });
    }

    static Object row(MethodHandle via, Object... args) {
        try {
            return via.invokeWithArguments(args);
        } catch (Throwable t) {
            return t.getClass().getName() + " " + t.getMessage();
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        Class<?> app = lookup.defineClass(tw("A"));
        Class<?> user = new Loader().define(tw("U"));
        MethodHandle uGet = lookup.findStatic(user, "get", MethodType.methodType(String.class, MethodHandle.class));
        MethodHandle uSet = lookup.findStatic(user, "set",
                MethodType.methodType(void.class, MethodHandle.class, String.class));
        MethodHandle uVget = lookup.findStatic(user, "vget", MethodType.methodType(String.class, VarHandle.class));
        MethodHandle uVset = lookup.findStatic(user, "vset",
                MethodType.methodType(void.class, VarHandle.class, String.class));

        MethodHandle id = lookup.findStatic(app, "id", MethodType.methodType(String.class));
        MethodHandle get = lookup.findStaticGetter(app, "V", String.class);
        MethodHandle set = lookup.findStaticSetter(app, "V", String.class);
        VarHandle vh = lookup.findStaticVarHandle(app, "V", String.class);

        System.out.println("static-first: " + row(uGet, id));
        System.out.println("getter: " + row(uGet, get));
        row(uSet, set, "A2");
        System.out.println("setter: " + row(uGet, get));
        System.out.println("varhandle: " + row(uVget, vh));
        row(uVset, vh, "A3");
        System.out.println("varhandle-set: " + row(uVget, vh));
        System.out.println("user-untouched: " + user.getField("V").get(null));
        System.out.println("app-direct: " + app.getField("V").get(null));
        System.out.println("static-again: " + row(uGet, id));
    }
}