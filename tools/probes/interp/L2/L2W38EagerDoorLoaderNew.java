// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L2: item 4 of the wave-37 compile-door
// review. The eager first-call door (`execute` in
// vm/src/runtime/interpreter.rs, reached with `CRATONVM_BG_COMPILE=0`)
// resolved a compiled method's `new` / `anewarray` classes and its trivial
// `<init>()V` targets with `SharedVm::load_class_concurrent`, which is
// loader-BLIND: it answers the built-in chain's class of that name. For a
// method of a user-defined loader's class that is another loader's class
// whenever the application class path has one of the same name.
//
// A child-first loader defines its OWN copies of the three nested classes
// below (the application loader has them too; `main` instantiates them first)
// and a class `l2e.User` whose static methods `new` / `anewarray` them. The
// loader's `CtorTarget.<init>` counts its calls in a static field; the class
// path's is the empty constructor the eager door elides. Each method is first
// called reflectively (the route to `execute`), so under
// `CRATONVM_BG_COMPILE=0` the eager door compiles it at that first call.
//
//   cold  the loader has not defined the targets yet when `User` is compiled
//         (only a site's run asks it); the compile must defer the site.
//   warm  the loader defined all three before `User` loaded; the compile
//         must bind the LOADER's classes.
//
// Each row counts the objects whose class (for the array: component class)
// the loader defined, out of 5 calls, and `made` is the loader's
// `CtorTarget.made`.
//
// Run: javac -d out L2W38EagerDoorLoaderNew.java
//      CRATONVM_BG_COMPILE=0 cratonvm --java-home <jdk25> -cp out L2W38EagerDoorLoaderNew
//      CRATONVM_BG_COMPILE=0 cratonvm --java-home <jdk25> --compatible -cp out L2W38EagerDoorLoaderNew
//      cratonvm --java-home <jdk25> [--nojit] -cp out L2W38EagerDoorLoaderNew
//
// Expected HotSpot 25 output (default and -Xint), every CratonVM mode alike:
//   cold new: own=5 other=0
//   cold anewarray: own=5 other=0
//   cold ctor: own=5 other=0 made=5
//   warm new: own=5 other=0
//   warm anewarray: own=5 other=0
//   warm ctor: own=5 other=0 made=5
//
// Expected on the wave-37 base under CRATONVM_BG_COMPILE=0, read from the
// code (not run in-lane): `other=5` on every row whose site the eager door
// resolved to the class path's class (and `made=0` on the ctor rows, whose
// call it elided as `Object.<init>`); the cold rows only if nothing asked the
// loader for the name before the compile.
//
// Positive control (wave 38): with CRATONVM_DBG_JITC=1 the cold rows' compile
// prints `[cratonvm-jitc] cp-class deferred holder=l2e/User name=L2W38EagerDoorLoaderNew$NewTarget ...`
// (the loader-faithful answer declined a class the old loader-blind lookup
// would have bound), and the `first-compile l2e/User.make()` line shows the
// eager door compiled the method.
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.Method;
import java.util.HashMap;
import java.util.Map;

public class L2W38EagerDoorLoaderNew {
    public static class NewTarget {
        public NewTarget() {
        }
    }

    public static class CtorTarget {
        public CtorTarget() {
        }
    }

    public static class ArrTarget {
        public ArrTarget() {
        }
    }

    static final String OUTER = "L2W38EagerDoorLoaderNew";
    static final String NEW_NAME = OUTER + "$NewTarget";
    static final String CTOR_NAME = OUTER + "$CtorTarget";
    static final String ARR_NAME = OUTER + "$ArrTarget";
    static final ClassDesc NEW_T = ClassDesc.of(NEW_NAME);
    static final ClassDesc CTOR_T = ClassDesc.of(CTOR_NAME);
    static final ClassDesc ARR_T = ClassDesc.of(ARR_NAME);
    static final ClassDesc USER = ClassDesc.of("l2e.User");
    static final MethodTypeDesc OBJ = MethodTypeDesc.of(ConstantDescs.CD_Object);
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;

    /// A public class whose `<init>()V` is `aload_0; invokespecial
    /// Object.<init>; return`.
    static byte[] empty(ClassDesc self) {
        return ClassFile.of().build(self, clb -> clb
                .withFlags(PUBLIC)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_()));
    }

    /// A public class whose `<init>()V` also does `made++`.
    static byte[] counting(ClassDesc self) {
        return ClassFile.of().build(self, clb -> clb
                .withFlags(PUBLIC)
                .withField("made", ConstantDescs.CD_int, PUBLIC | STATIC)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .getstatic(self, "made", ConstantDescs.CD_int)
                                .iconst_1()
                                .iadd()
                                .putstatic(self, "made", ConstantDescs.CD_int)
                                .return_()));
    }

    /// `make()`: `new NewTarget()`; `makeArr()`: `new ArrTarget[1]`;
    /// `makeCtor()`: `new CtorTarget()`.
    static byte[] user() {
        return ClassFile.of().build(USER, clb -> clb
                .withFlags(PUBLIC)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("make", OBJ, PUBLIC | STATIC,
                        cb -> cb.new_(NEW_T).dup()
                                .invokespecial(NEW_T, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                                .areturn())
                .withMethodBody("makeArr", OBJ, PUBLIC | STATIC,
                        cb -> cb.iconst_1().anewarray(ARR_T).areturn())
                .withMethodBody("makeCtor", OBJ, PUBLIC | STATIC,
                        cb -> cb.new_(CTOR_T).dup()
                                .invokespecial(CTOR_T, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                                .areturn()));
    }

    /// Child-first for its own four names; everything else goes to the
    /// application loader.
    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(L2W38EagerDoorLoaderNew.class.getClassLoader());
            bytes.put(NEW_NAME, empty(NEW_T));
            bytes.put(ARR_NAME, empty(ARR_T));
            bytes.put(CTOR_NAME, counting(CTOR_T));
            bytes.put("l2e.User", user());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                byte[] b = bytes.get(name);
                if (b == null) {
                    return super.loadClass(name, resolve);
                }
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static final int CALLS = 5;

    static void run(String label, Loader l, boolean warm) throws Exception {
        if (warm) {
            l.loadClass(NEW_NAME);
            l.loadClass(ARR_NAME);
            l.loadClass(CTOR_NAME);
        }
        Class<?> user = l.loadClass("l2e.User");
        Method make = user.getMethod("make");
        Method makeArr = user.getMethod("makeArr");
        Method makeCtor = user.getMethod("makeCtor");
        int own = 0;
        int other = 0;
        for (int i = 0; i < CALLS; i++) {
            if (make.invoke(null).getClass().getClassLoader() == l) {
                own++;
            } else {
                other++;
            }
        }
        System.out.println(label + " new: own=" + own + " other=" + other);
        own = 0;
        other = 0;
        for (int i = 0; i < CALLS; i++) {
            if (makeArr.invoke(null).getClass().getComponentType().getClassLoader() == l) {
                own++;
            } else {
                other++;
            }
        }
        System.out.println(label + " anewarray: own=" + own + " other=" + other);
        own = 0;
        other = 0;
        for (int i = 0; i < CALLS; i++) {
            if (makeCtor.invoke(null).getClass().getClassLoader() == l) {
                own++;
            } else {
                other++;
            }
        }
        int made = l.loadClass(CTOR_NAME).getField("made").getInt(null);
        System.out.println(label + " ctor: own=" + own + " other=" + other + " made=" + made);
    }

    public static void main(String[] args) throws Exception {
        // The class path's copies, loaded and initialized first.
        new NewTarget();
        new CtorTarget();
        new ArrTarget();
        ArrTarget[] warmArray = new ArrTarget[1];
        if (warmArray.length != 1) {
            throw new AssertionError();
        }
        run("cold", new Loader(), false);
        run("warm", new Loader(), true);
    }
}
