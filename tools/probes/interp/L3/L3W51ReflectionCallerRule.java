// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 51, lane L3 -- stage 2 of
// docs/internal/fixed-bugs/interpreter-i49-L3-proposal-one-caller-class-rule-over-the-walkers-capture-DONE-20261128.md:
// the JVM's `jdk.internal.reflect.Reflection.getCallerClass()`, observed
// through JDK 25's `@CallerSensitive` `Package.getPackage(String)`, whose
// bytecode asks it and answers the package as the CALLER's loader defines it
// (`null` when that loader defines none). (`Module.addReads` and its siblings
// ask it too, but check the caller only for a named module.)
//
// The asking code lives in `p.Twin`, defined by its own loader `a`, which
// alone defines package `p`: a wrong caller answers the application loader's
// `null`.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   twin-direct          -- `Twin.pkg()`: `Package.getPackage("p")`;
//   app-direct           -- the same call made by this class (application loader);
//   twin-reflective      -- `Twin.pkgReflective()`: `Package.getPackage` through
//                           `Method.invoke`, the reflective call made by `Twin`
//                           (HotSpot skips `Method.invoke` and the accessor);
//   twin-via-handle      -- `Twin.pkg()` called through a `MethodHandle`;
//   twin-in-stream       -- `Twin.pkg()` from a lambda of `Stream.map(...).toList()`,
//                           20,000 times (hot, compiled), count of right answers;
//   twin-in-thread       -- `Twin.pkg()` from a `Runnable` run by a new `Thread`;
//   twin-constructor     -- `Twin.<init>` asks, run by `Constructor.newInstance`
//                           (HotSpot does not skip the constructor accessor, and
//                           the constructor is the caller);
//   twin-clinit          -- `p.Init.<clinit>` asks, run by `Class.forName(.., true, a)`;
//   twin-direct-again    -- `twin-direct` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// twin-direct: p
// app-direct: null
// twin-reflective: p
// twin-via-handle: p
// twin-in-stream: right=20000/20000 last=p
// twin-in-thread: p
// twin-constructor: p
// twin-clinit: p
// twin-direct-again: p
// HOTSPOT_EXPECTED_END
//
// Since wave 51 (`CRATONVM_REFLECTION_CALLER_RULE`,
// `VmConfig::reflection_caller_class_rule`) the registered
// `Reflection.getCallerClass()` walks the walkers' capture by HotSpot's
// `JVM_GetCallerClass` rule. Positive control: `CRATONVM_DBG_CALLER=1` prints
// `[DBG_CALLER] Reflection.getCallerClass (security walk): <asker> -> <caller>`.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W51ReflectionCallerRule`.
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.Method;
import java.util.List;

public class L3W51ReflectionCallerRule {
    static final int N = 20_000;

    interface Call {
        Object run() throws Throwable;
    }

    static String show(Object o) {
        if (o instanceof Package p) {
            return p.getName();
        }
        return String.valueOf(o);
    }

    static String safe(Call call) {
        try {
            return show(call.run());
        } catch (Throwable t) {
            Throwable c = t instanceof java.lang.reflect.InvocationTargetException i ? i.getCause() : t;
            return c.getClass().getName();
        }
    }

    static void row(String name, Call call) {
        System.out.println(name + ": " + safe(call));
    }

    static final ClassDesc PACKAGE = ClassDesc.of("java.lang.Package");
    static final ClassDesc METHOD = ClassDesc.of("java.lang.reflect.Method");

    /** `p.Twin`: `pkg()` and `pkgReflective()`. */
    static byte[] twin() {
        return ClassFile.of().build(ClassDesc.of("p.Twin"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("pkg", MethodTypeDesc.of(ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.ldc("p")
                            .invokestatic(PACKAGE, "getPackage", MethodTypeDesc.of(PACKAGE, ConstantDescs.CD_String))
                            .areturn());
            cb.withMethodBody("pkgReflective", MethodTypeDesc.of(ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.ldc(PACKAGE)
                            .ldc("getPackage")
                            .iconst_1()
                            .anewarray(ConstantDescs.CD_Class)
                            .dup()
                            .iconst_0()
                            .ldc(ConstantDescs.CD_String)
                            .aastore()
                            .invokevirtual(ConstantDescs.CD_Class, "getMethod",
                                    MethodTypeDesc.of(METHOD, ConstantDescs.CD_String, ConstantDescs.CD_Class.arrayType()))
                            .aconst_null()
                            .iconst_1()
                            .anewarray(ConstantDescs.CD_Object)
                            .dup()
                            .iconst_0()
                            .ldc("p")
                            .aastore()
                            .invokevirtual(METHOD, "invoke",
                                    MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_Object,
                                            ConstantDescs.CD_Object.arrayType()))
                            .areturn());
            // public Object seen; public Twin() { seen = Package.getPackage("p"); }
            cb.withField("seen", ConstantDescs.CD_Object, ClassFile.ACC_PUBLIC);
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .aload(0)
                            .ldc("p")
                            .invokestatic(PACKAGE, "getPackage", MethodTypeDesc.of(PACKAGE, ConstantDescs.CD_String))
                            .putfield(ClassDesc.of("p.Twin"), "seen", ConstantDescs.CD_Object)
                            .return_());
        });
    }

    /** `p.Init`: `public static Object SEEN = Package.getPackage("p");`. */
    static byte[] init() {
        ClassDesc self = ClassDesc.of("p.Init");
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withField("SEEN", ConstantDescs.CD_Object, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC);
            cb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_STATIC,
                    code -> code.ldc("p")
                            .invokestatic(PACKAGE, "getPackage", MethodTypeDesc.of(PACKAGE, ConstantDescs.CD_String))
                            .putstatic(self, "SEEN", ConstantDescs.CD_Object)
                            .return_());
        });
    }

    public static void main(String[] args) throws Throwable {
        byte[] tb = twin();
        byte[] ib = init();
        ClassLoader a = new ClassLoader("a", L3W51ReflectionCallerRule.class.getClassLoader()) {
            @Override
            protected Class<?> findClass(String n) throws ClassNotFoundException {
                byte[] b = n.equals("p.Twin") ? tb : n.equals("p.Init") ? ib : null;
                if (b == null) {
                    throw new ClassNotFoundException(n);
                }
                return defineClass(n, b, 0, b.length);
            }
        };
        Class<?> twin = Class.forName("p.Twin", true, a);
        Method pkg = twin.getMethod("pkg");
        Method pkgReflective = twin.getMethod("pkgReflective");

        row("twin-direct", () -> pkg.invoke(null));
        row("app-direct", () -> Package.getPackage("p"));
        row("twin-reflective", () -> pkgReflective.invoke(null));
        var pkgHandle = java.lang.invoke.MethodHandles.publicLookup().unreflect(pkg);
        row("twin-via-handle", () -> pkgHandle.invoke());
        List<Integer> one = List.of(1);
        int right = 0;
        String last = null;
        for (int i = 0; i < N; i++) {
            last = one.stream().map(x -> safe(() -> pkg.invoke(null))).toList().get(0);
            if ("p".equals(last)) {
                right++;
            }
        }
        System.out.println("twin-in-stream: right=" + right + "/" + N + " last=" + last);
        row("twin-in-thread", () -> {
            String[] out = new String[1];
            Thread t = new Thread(() -> out[0] = safe(() -> pkg.invoke(null)));
            t.start();
            t.join();
            return out[0];
        });
        row("twin-constructor", () -> twin.getField("seen").get(twin.getConstructor().newInstance()));
        row("twin-clinit", () -> Class.forName("p.Init", true, a).getField("SEEN").get(null));
        row("twin-direct-again", () -> pkg.invoke(null));
    }
}
