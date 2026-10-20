// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L4 (w100 hand-back follow-up): the
// `IllegalAccessError` text of a JPMS export refusal whose accessor is in an
// UNNAMED module. HotSpot (`Reflection::verify_class_access_msg`) names the
// accessor's module `unnamed module @0x<identity hash>` on both sides:
//
//   class C (in unnamed module @0x..) cannot access class jdk.internal.misc.Unsafe
//   (in module java.base) because module java.base does not export
//   jdk.internal.misc to unnamed module @0x..
//
// CratonVM's class manager writes `unnamed module` without the hash; wave 100
// rewrites it in one place (`vm/src/vm/vm_object.rs`
// `access_denial_with_unnamed_module_hash`) for every site that raises it:
// constant-pool class resolution (`ldc`, `checkcast`, `anewarray`: `constants.rs`
// `raise_class_access_denial`; `new`: `opcodes.rs`), the method-handle site
// link (`invoke.rs`) and the doors' copy (`vm_exec.rs`
// `class_access_denial_for_name`, reached here by the reflective row).
//
// The callers are generated with `java.lang.classfile` (javac refuses to
// reference a non-exported package): one defined by `Lookup.defineClass` in
// the application loader, one by a user loader named "child". Each row is
// asked twice (a failed resolution is recorded and raised again with the same
// error). Hashes print as `@<hash>`.
//
// Run: javac L4W100UnnamedModuleHashTexts.java && java L4W100UnnamedModuleHashTexts
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources)
// app-ldc-1: java.lang.IllegalAccessError: class L4W100UnnamedModuleHashTextsApp (in unnamed module @<hash>) cannot access class jdk.internal.misc.Unsafe (in module java.base) because module java.base does not export jdk.internal.misc to unnamed module @<hash>
// app-ldc-2: java.lang.IllegalAccessError: class L4W100UnnamedModuleHashTextsApp (in unnamed module @<hash>) cannot access class jdk.internal.misc.Unsafe (in module java.base) because module java.base does not export jdk.internal.misc to unnamed module @<hash>
// app-checkcast-1: java.lang.IllegalAccessError: class L4W100UnnamedModuleHashTextsApp (in unnamed module @<hash>) cannot access class jdk.internal.misc.Unsafe (in module java.base) because module java.base does not export jdk.internal.misc to unnamed module @<hash>
// app-anewarray-1: java.lang.IllegalAccessError: class L4W100UnnamedModuleHashTextsApp (in unnamed module @<hash>) cannot access class jdk.internal.misc.Unsafe (in module java.base) because module java.base does not export jdk.internal.misc to unnamed module @<hash>
// app-site-1: java.lang.IllegalAccessError: class L4W100UnnamedModuleHashTextsApp (in unnamed module @<hash>) cannot access class jdk.internal.misc.Unsafe (in module java.base) because module java.base does not export jdk.internal.misc to unnamed module @<hash>
// app-site-reflective: java.lang.IllegalAccessError: class L4W100UnnamedModuleHashTextsApp (in unnamed module @<hash>) cannot access class jdk.internal.misc.Unsafe (in module java.base) because module java.base does not export jdk.internal.misc to unnamed module @<hash>
// child-ldc-1: java.lang.IllegalAccessError: class L4W100UnnamedModuleHashTextsChild (in unnamed module @<hash>) cannot access class jdk.internal.misc.Unsafe (in module java.base) because module java.base does not export jdk.internal.misc to unnamed module @<hash>
// child-ldc-2: java.lang.IllegalAccessError: class L4W100UnnamedModuleHashTextsChild (in unnamed module @<hash>) cannot access class jdk.internal.misc.Unsafe (in module java.base) because module java.base does not export jdk.internal.misc to unnamed module @<hash>
// child-site-1: java.lang.IllegalAccessError: class L4W100UnnamedModuleHashTextsChild (in unnamed module @<hash>) cannot access class jdk.internal.misc.Unsafe (in module java.base) because module java.base does not export jdk.internal.misc to unnamed module @<hash>
// same-module-hash app true
// same-module-hash child true
// HOTSPOT_EXPECTED_END
import static java.lang.classfile.ClassFile.ACC_PUBLIC;
import static java.lang.classfile.ClassFile.ACC_STATIC;

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;

public class L4W100UnnamedModuleHashTexts {
    static final ClassDesc UNSAFE = ClassDesc.of("jdk.internal.misc.Unsafe");
    static final ClassDesc MH = ClassDesc.of("java.lang.invoke.MethodHandle");

    static byte[] caller(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ACC_PUBLIC);
            cb.withSuperclass(ConstantDescs.CD_Object);
            MethodTypeDesc toObject = MethodTypeDesc.of(ConstantDescs.CD_Object);
            cb.withMethodBody("ldc", toObject, ACC_PUBLIC | ACC_STATIC,
                    c -> c.ldc(UNSAFE).areturn());
            cb.withMethodBody("cast", toObject, ACC_PUBLIC | ACC_STATIC,
                    // A non-null operand: HotSpot's `checkcast` of null skips
                    // the resolution (and so the refusal).
                    c -> c.ldc("x").checkcast(UNSAFE).areturn());
            cb.withMethodBody("array", toObject, ACC_PUBLIC | ACC_STATIC,
                    c -> c.iconst_0().anewarray(UNSAFE).areturn());
            cb.withMethodBody("site", MethodTypeDesc.of(ConstantDescs.CD_void, MH),
                    ACC_PUBLIC | ACC_STATIC,
                    c -> c.aload(0).aconst_null()
                            .invokevirtual(MH, "invokeExact",
                                    MethodTypeDesc.of(ConstantDescs.CD_void, UNSAFE))
                            .return_());
        });
    }

    static final class Child extends ClassLoader {
        Child() {
            super("child", L4W100UnnamedModuleHashTexts.class.getClassLoader());
        }

        Class<?> define(byte[] b) {
            return defineClass(null, b, 0, b.length);
        }
    }

    interface Row {
        Object run() throws Throwable;
    }

    static String lastText = "";

    static void row(String name, Row r) {
        String out;
        try {
            out = "no-throw " + r.run();
        } catch (Throwable t) {
            if (t instanceof InvocationTargetException ite && ite.getCause() != null) {
                t = ite.getCause();
            }
            lastText = String.valueOf(t.getMessage());
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out.replaceAll("@(0x)?[0-9a-f]+", "@<hash>"));
    }

    /** Both `@` hashes of the last message name the accessor's own module. */
    static boolean sameModuleHash(Class<?> c) {
        String hex = "@0x" + Integer.toHexString(System.identityHashCode(c.getModule()));
        int first = lastText.indexOf(hex);
        return first >= 0 && lastText.indexOf(hex, first + 1) > first;
    }

    public static void main(String[] args) throws Throwable {
        Class<?> app = MethodHandles.lookup().defineClass(caller("L4W100UnnamedModuleHashTextsApp"));
        Class<?> child = new Child().define(caller("L4W100UnnamedModuleHashTextsChild"));
        MethodType toObject = MethodType.methodType(Object.class);
        MethodType site = MethodType.methodType(void.class, MethodHandle.class);
        MethodHandles.Lookup pub = MethodHandles.publicLookup();
        MethodHandle target = MethodHandles.empty(MethodType.methodType(void.class, Object.class));

        MethodHandle appLdc = pub.findStatic(app, "ldc", toObject);
        MethodHandle appCast = pub.findStatic(app, "cast", toObject);
        MethodHandle appArray = pub.findStatic(app, "array", toObject);
        MethodHandle appSite = pub.findStatic(app, "site", site);
        row("app-ldc-1", () -> appLdc.invoke());
        row("app-ldc-2", () -> appLdc.invoke());
        row("app-checkcast-1", () -> appCast.invoke());
        row("app-anewarray-1", () -> appArray.invoke());
        row("app-site-1", () -> {
            appSite.invoke(target);
            return "ran";
        });
        row("app-site-reflective", () -> app.getMethod("site", MethodHandle.class).invoke(null, target));
        boolean appSame = sameModuleHash(app);

        MethodHandle childLdc = pub.findStatic(child, "ldc", toObject);
        MethodHandle childSite = pub.findStatic(child, "site", site);
        row("child-ldc-1", () -> childLdc.invoke());
        row("child-ldc-2", () -> childLdc.invoke());
        row("child-site-1", () -> {
            childSite.invoke(target);
            return "ran";
        });
        boolean childSame = sameModuleHash(child);
        System.out.println("same-module-hash app " + appSame);
        System.out.println("same-module-hash child " + childSame);
    }
}
