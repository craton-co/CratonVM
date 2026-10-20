// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L2: a call site whose method reference
// does not resolve AT ALL (separate compilation removed the method), warmed
// until the JIT compiles the caller. JVMS 5.4.3.3 / 5.4.3.4: resolution of the
// reference fails before any selection, so HotSpot's NoSuchMethodError names
// the class the SITE named, on every call, whatever the receiver -- even a
// receiver whose own class declares a method of that name and descriptor.
//
// The compiled dispatch doors call by name on the RECEIVER's class; their
// terminal miss named the receiver ('int UGS2.gone()'), and the S111r12 rescue
// that used to resend through the site's class stopped matching when the
// NoSuchMethodError texts became HotSpot's (is_dispatch_no_such_method_miss).
// Wave 100 answers it from the class store in handle_jit_dispatch_error
// (jit_site_unresolved_reference_failure).
//
//   UGB  class (declares nothing);  UGS extends UGB (declares nothing)
//   UGS2 extends UGS (declares nothing);  UGS3 extends UGS: int gone() = 3
//   UGI  interface (declares nothing)
//   UGC1 implements UGI (declares nothing);  UGC2 implements UGI: int gone() = 4
//   UGD  the callers:
//     v1  invokevirtual   UGS.gone  on a UGS2
//     v2  invokevirtual   UGS.gone  on a UGS3   (receiver declares gone)
//     vp  invokevirtual   UGS.gone  on UGS2/UGS3
//     i1  invokeinterface UGI.gone  on a UGC1
//     i2  invokeinterface UGI.gone  on a UGC2   (receiver declares gone)
//
// Each row prints the distinct answers of 20000 calls. v2 and i2 are the
// rows of i100-L2-unresolvable-reference-runs-the-receivers-method: a by-name
// walk from the receiver FINDS gone(). Fixed in wave 100 (follow-up) in the
// interpreter's resolution (`resolve_method_metadata_noting`) and on the JIT
// success path (`virtual_dispatch_target_cached` flags the site, the doors
// raise it through `jit_selection_failure`). Kill switch:
// CRATONVM_UNRESOLVABLE_METHOD_REF_THROWS=0 (v2 then prints [3], i2 [4]).
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] L2W100UnresolvedSiteWarm
// Expected (HotSpot 25):
//   v1: [java.lang.NoSuchMethodError: 'int UGS.gone()']
//   v2: [java.lang.NoSuchMethodError: 'int UGS.gone()']
//   vp: [java.lang.NoSuchMethodError: 'int UGS.gone()']
//   i1: [java.lang.NoSuchMethodError: 'int UGI.gone()']
//   i2: [java.lang.NoSuchMethodError: 'int UGI.gone()']

import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.Set;
import java.util.TreeSet;
import java.util.function.Consumer;

public class L2W100UnresolvedSiteWarm {
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
    static final int PUB = ClassFile.ACC_PUBLIC;
    static final int PUB_STATIC = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;

    static ClassDesc cd(String n) { return ClassDesc.of(n); }

    static Class<?> define(String name, int flags, ClassDesc sup, ClassDesc[] ifaces,
                           Consumer<ClassBuilder> body) throws IllegalAccessException {
        byte[] bytes = ClassFile.of().build(cd(name), cb -> {
            cb.withFlags(flags);
            if (sup != null) cb.withSuperclass(sup);
            if (ifaces.length > 0) cb.withInterfaceSymbols(ifaces);
            body.accept(cb);
        });
        return MethodHandles.lookup().defineClass(bytes);
    }

    static void ctor(ClassBuilder cb, ClassDesc sup) {
        cb.withMethodBody("<init>", VOID, PUB,
            code -> code.aload(0).invokespecial(sup, "<init>", VOID).return_());
    }

    static void gone(ClassBuilder cb, int value) {
        cb.withMethodBody("gone", INT, PUB, code -> code.ldc(value).ireturn());
    }

    static String run(Method m, Object arg) {
        try {
            return String.valueOf(m.invoke(null, arg));
        } catch (InvocationTargetException e) {
            Throwable c = e.getCause();
            return c.getClass().getName() + ": " + c.getMessage();
        } catch (Throwable t) {
            return "probe failure " + t;
        }
    }

    public static void main(String[] args) throws Throwable {
        int iface = ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT;
        int cls = ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER;
        ClassDesc obj = ConstantDescs.CD_Object;
        ClassDesc[] none = {};
        ClassDesc ugb = cd("UGB"), ugs = cd("UGS"), ugi = cd("UGI");

        define("UGB", cls, obj, none, cb -> ctor(cb, obj));
        define("UGS", cls, ugb, none, cb -> ctor(cb, ugb));
        Class<?> s2 = define("UGS2", cls, ugs, none, cb -> ctor(cb, ugs));
        Class<?> s3 = define("UGS3", cls, ugs, none, cb -> { ctor(cb, ugs); gone(cb, 3); });
        define("UGI", iface, null, none, cb -> { });
        Class<?> c1 = define("UGC1", cls, obj, new ClassDesc[] {ugi}, cb -> ctor(cb, obj));
        Class<?> c2 = define("UGC2", cls, obj, new ClassDesc[] {ugi},
            cb -> { ctor(cb, obj); gone(cb, 4); });
        Class<?> ugd = define("UGD", cls, obj, none, cb -> {
            for (String v : new String[] {"v1", "v2", "vp"}) {
                cb.withMethodBody(v, MethodTypeDesc.of(ConstantDescs.CD_int, ugs), PUB_STATIC,
                    code -> code.aload(0).invokevirtual(ugs, "gone", INT).ireturn());
            }
            for (String i : new String[] {"i1", "i2"}) {
                cb.withMethodBody(i, MethodTypeDesc.of(ConstantDescs.CD_int, ugi), PUB_STATIC,
                    code -> code.aload(0).invokeinterface(ugi, "gone", INT).ireturn());
            }
        });
        Object o2 = s2.getConstructor().newInstance();
        Object o3 = s3.getConstructor().newInstance();
        Object k1 = c1.getConstructor().newInstance();
        Object k2 = c2.getConstructor().newInstance();
        String[] rows = {"v1", "v2", "vp", "i1", "i2"};
        Object[][] recv = {{o2}, {o3}, {o2, o3}, {k1}, {k2}};
        for (int i = 0; i < rows.length; i++) {
            Method m = null;
            for (Method cand : ugd.getMethods()) if (cand.getName().equals(rows[i])) m = cand;
            Set<String> seen = new TreeSet<>();
            for (int k = 0; k < 20000; k++) seen.add(run(m, recv[i][k % recv[i].length]));
            System.out.println(rows[i] + ": " + seen);
        }
    }
}
