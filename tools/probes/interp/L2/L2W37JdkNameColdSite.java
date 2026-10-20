// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L2: the JIT half of
// docs/internal/fixed-bugs/interpreter-L5-a-user-loaders-class-under-a-jdk-name-is-resolved-to-the-jdks-class-FIXED-20261005.md.
// A child-first loader (it overrides `loadClass`) defines its OWN
// `javax.security.auth.x500.X500Principal` and `X500PrivateCredential` on
// request; java.base's copies are loaded first. `l2g.User.applyAsInt(k)` is
// hot, and its sites naming those classes run only for k >= 20000, after the
// method has been compiled. The interpreter asks the loader for such a name
// (`drive_loader_for_global_name`, wave 28); the JIT's compile-time answer
// (`class_resolved_without_loading` -> `find_class_by_name_for_class`) is
// java.base's class for a name the loader has not answered yet.
//
//   cold new          `new X500Principal(); invokevirtual w()` -> the
//                     loader's own w() = 303
//   cold invokestatic `X500PrivateCredential.v()` -> the loader's own v() = 301
//
// Run: javac -d out L2W37JdkNameColdSite.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L2W37JdkNameColdSite
//
// Deferred-site census (every compile-time answer the loader-faithful rule
// would change; since wave 37's second pass the rule is APPLIED at the
// `new`, statically bound call and constant-pool splice doors and at the two
// run-time fast paths, `jit_resolve_cp_class` and `jit_static_owner_override`):
//   CRATONVM_DBG_ISOLATED_CNF=1 cratonvm --java-home <jdk25> -cp out L2W37JdkNameColdSite 2>&1 \
//     | grep '\[ISOLATED-CNF\] jit global-name would-defer' | sort -u
// must print at least one line naming `holder=l2g/User` and one of the two
// names (`why=transparency-unknown` or `why=not-transparent`) when the JIT
// compiles `applyAsInt` before k reaches 20000: that is the positive control
// of the counter.
//
// Expected HotSpot 25 output (default and -Xint):
//   cold new: 303=5000 other=0 errors=0
//   cold invokestatic: 301=5000 other=0 errors=0
//
// Wave 37 host run before the fix (default and --compatible):
//   cold new: 303=0 other=0 errors=5000
//   cold invokestatic: 301=0 other=0 errors=5000
// (a failing row now names its first exception: ` first=...`). Expected after
// the fix: HotSpot's lines in default mode. `--compatible` keeps erring by
// design: its interpreter does not ask a user loader for a JDK-global name
// either (`L5W27JdkNamedOwnClass`, `lazy jdk class`).
import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.HashMap;
import java.util.Map;
import java.util.function.IntUnaryOperator;

public class L2W37JdkNameColdSite {
    static final String PKG = "javax.security.auth.x500.";
    static final ClassDesc CRED = ClassDesc.of(PKG + "X500PrivateCredential");
    static final ClassDesc PRINCIPAL = ClassDesc.of(PKG + "X500Principal");
    static final ClassDesc USER = ClassDesc.of("l2g.User");
    static final ClassDesc I = ConstantDescs.CD_int;
    static final MethodTypeDesc INT = MethodTypeDesc.of(I);
    static final MethodTypeDesc INT_INT = MethodTypeDesc.of(I, I);
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;

    /// A public class with a public no-arg constructor, `static int v()` and
    /// `int w()`.
    static byte[] owned(ClassDesc self, int v, int w) {
        return ClassFile.of().build(self, clb -> clb
                .withFlags(PUBLIC)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("v", INT, PUBLIC | STATIC, cb -> cb.sipush(v).ireturn())
                .withMethodBody("w", INT, PUBLIC, cb -> cb.sipush(w).ireturn()));
    }

    /// `applyAsInt(k)`: k < 20000 -> k & 1; otherwise (k & 1) == 0 -> `new
    /// X500Principal().w()`, else `X500PrivateCredential.v()`.
    static byte[] user() {
        return ClassFile.of().build(USER, clb -> clb
                .withFlags(PUBLIC)
                .withInterfaceSymbols(ClassDesc.of("java.util.function.IntUnaryOperator"))
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("applyAsInt", INT_INT, PUBLIC, cb -> {
                    Label cold = cb.newLabel();
                    Label stat = cb.newLabel();
                    cb.iload(1).sipush(20_000).if_icmpge(cold);
                    cb.iload(1).iconst_1().iand().ireturn();
                    cb.labelBinding(cold);
                    cb.iload(1).iconst_1().iand().ifne(stat);
                    cb.new_(PRINCIPAL).dup()
                            .invokespecial(PRINCIPAL, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .invokevirtual(PRINCIPAL, "w", INT).ireturn();
                    cb.labelBinding(stat);
                    cb.invokestatic(CRED, "v", INT).ireturn();
                }));
    }

    /// Child-first for its own names, parent = the platform loader.
    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(ClassLoader.getPlatformClassLoader());
            bytes.put(PKG + "X500PrivateCredential", owned(CRED, 301, 302));
            bytes.put(PKG + "X500Principal", owned(PRINCIPAL, 304, 303));
            bytes.put("l2g.User", user());
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

    public static void main(String[] args) throws Exception {
        Class.forName("javax.security.auth.x500.X500PrivateCredential");
        Class.forName("javax.security.auth.x500.X500Principal");
        IntUnaryOperator op = (IntUnaryOperator) new Loader().loadClass("l2g.User")
                .getConstructor().newInstance();
        int[] hit = new int[2];
        int[] other = new int[2];
        int[] errors = new int[2];
        int[] want = {303, 301};
        String[] first = {"", ""};
        for (int k = 0; k < 30_000; k++) {
            try {
                int r = op.applyAsInt(k);
                if (k >= 20_000) {
                    int row = k & 1;
                    if (r == want[row]) {
                        hit[row]++;
                    } else {
                        other[row]++;
                    }
                }
            } catch (Throwable t) {
                if (errors[k & 1]++ == 0) {
                    first[k & 1] = " first=" + t;
                }
            }
        }
        System.out.println("cold new: 303=" + hit[0] + " other=" + other[0] + " errors=" + errors[0] + first[0]);
        System.out.println("cold invokestatic: 301=" + hit[1] + " other=" + other[1] + " errors=" + errors[1] + first[1]);
    }
}
