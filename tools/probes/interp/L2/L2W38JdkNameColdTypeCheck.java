// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L2: the type-check half of
// docs/internal/fixed-bugs/interpreter-L5-a-user-loaders-class-under-a-jdk-name-is-resolved-to-the-jdks-class-FIXED-20261005.md,
// a variant of L2W37JdkNameColdSite. A child-first loader defines its OWN
// `javax.security.auth.x500.X500Principal` (`w()` = 303) on request;
// java.base's copy is loaded first. `l2h.User`'s `checkcast` / `instanceof`
// sites naming it are compiled before the loader has been asked for the name,
// and run only afterwards, on an instance of the loader's class.
//
// Wave 37 applied the loader-faithful rule (`loader_faithful_global_name_answer`)
// to the method-entry doors' `new` resolver, which also answers their
// type-check targets; the single-pass OSR door and the eager first-call door
// resolved their type-check targets with the flat `jit_known_class`, and baked
// java.base's class id. The holder-keyed run-time path of a deferred site
// (`helpers::jit_typecheck_holder_site_target`) also took the flat answer
// before asking the loader.
//
//   eager instanceof / eager checkcast
//       `probe(o)` = `o instanceof X500Principal ? 305 : 306` and
//       `cast(o)` = `((X500Principal) o).w()` (0 for null), each first called
//       reflectively with a non-principal (the eager door's compile under
//       CRATONVM_BG_COMPILE=0), then with an instance of the loader's class.
//   loop
//       `loop(60000)`: for i >= 40000 each iteration makes an instance and
//       adds 1 when `instanceof` holds and 1 when the cast's `w()` is 303;
//       the loop is OSR-compiled during the first 40000 iterations.
//
// Run: javac -d out L2W38JdkNameColdTypeCheck.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L2W38JdkNameColdTypeCheck
//      CRATONVM_BG_COMPILE=0 cratonvm --java-home <jdk25> -cp out L2W38JdkNameColdTypeCheck
//      CRATONVM_JIT_OSR_OPTIMIZING=0 cratonvm --java-home <jdk25> -cp out L2W38JdkNameColdTypeCheck
// (the last one routes `loop` to the single-pass OSR door).
//
// Expected HotSpot 25 output (default and -Xint):
//   eager instanceof: 305
//   eager checkcast: 303
//   loop: 40000
// Expected on CratonVM after wave 38: the same, in default mode (a bare
// invocation is --jdk-only). `--compatible` may differ by design: its
// interpreter does not ask a user loader for a JDK-global name either
// (L5W27JdkNamedOwnClass, `lazy jdk class`), so the loader's class is never
// defined there and the rows print what the JDK's class answers.
//
// Positive control: CRATONVM_DBG_ISOLATED_CNF=1 prints
// `[ISOLATED-CNF] jit global-name would-defer holder=l2h/User name=javax/security/auth/x500/X500Principal`
// lines for these compiles (the census of `jit_known_class`), and with
// CRATONVM_DBG_JITC=1 the rows' compiles are visible (`first-compile
// l2h/User.probe`, `OSR-compile l2h/User.loop`).
import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.HashMap;
import java.util.Map;

public class L2W38JdkNameColdTypeCheck {
    static final String PKG = "javax.security.auth.x500.";
    static final ClassDesc PRINCIPAL = ClassDesc.of(PKG + "X500Principal");
    static final ClassDesc USER = ClassDesc.of("l2h.User");
    static final ClassDesc OBJ = ConstantDescs.CD_Object;
    static final ClassDesc I = ConstantDescs.CD_int;
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;

    /// The loader's `X500Principal`: a public no-arg constructor and `int w()`.
    static byte[] principal() {
        return ClassFile.of().build(PRINCIPAL, clb -> clb
                .withFlags(PUBLIC)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(OBJ, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("w", MethodTypeDesc.of(I), PUBLIC, cb -> cb.sipush(303).ireturn()));
    }

    static byte[] user() {
        return ClassFile.of().build(USER, clb -> clb
                .withFlags(PUBLIC)
                .withMethodBody("make", MethodTypeDesc.of(OBJ), PUBLIC | STATIC,
                        cb -> cb.new_(PRINCIPAL).dup()
                                .invokespecial(PRINCIPAL, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                                .areturn())
                .withMethodBody("probe", MethodTypeDesc.of(I, OBJ), PUBLIC | STATIC, cb -> {
                    Label no = cb.newLabel();
                    cb.aload(0).instanceOf(PRINCIPAL).ifeq(no);
                    cb.sipush(305).ireturn();
                    cb.labelBinding(no);
                    cb.sipush(306).ireturn();
                })
                .withMethodBody("cast", MethodTypeDesc.of(I, OBJ), PUBLIC | STATIC, cb -> {
                    Label some = cb.newLabel();
                    cb.aload(0).ifnonnull(some);
                    cb.iconst_0().ireturn();
                    cb.labelBinding(some);
                    cb.aload(0).checkcast(PRINCIPAL)
                            .invokevirtual(PRINCIPAL, "w", MethodTypeDesc.of(I)).ireturn();
                })
                // int acc = 0;
                // for (int i = 0; i < n; i++) {
                //   if (i < 40000) { acc += 0 * (i & 1); continue; }   // warm, no count
                //   Object o = make();
                //   if (o instanceof X500Principal) acc++;
                //   if (((X500Principal) o).w() == 303) acc++;
                // }
                // return acc;
                .withMethodBody("loop", MethodTypeDesc.of(I, I), PUBLIC | STATIC, cb -> {
                    Label head = cb.newLabel();
                    Label body = cb.newLabel();
                    Label cold = cb.newLabel();
                    Label notInst = cb.newLabel();
                    Label next = cb.newLabel();
                    Label done = cb.newLabel();
                    cb.iconst_0().istore(1); // acc
                    cb.iconst_0().istore(2); // i
                    cb.labelBinding(head);
                    cb.iload(2).iload(0).if_icmpge(done);
                    cb.labelBinding(body);
                    cb.iload(2).ldc(40_000).if_icmpge(cold);
                    cb.iload(1).iload(2).iconst_1().iand().iconst_0().imul().iadd().istore(1);
                    cb.goto_(next);
                    cb.labelBinding(cold);
                    cb.invokestatic(USER, "make", MethodTypeDesc.of(OBJ)).astore(3);
                    cb.aload(3).instanceOf(PRINCIPAL).ifeq(notInst);
                    cb.iinc(1, 1);
                    cb.labelBinding(notInst);
                    Label skip = cb.newLabel();
                    cb.aload(3).checkcast(PRINCIPAL)
                            .invokevirtual(PRINCIPAL, "w", MethodTypeDesc.of(I))
                            .sipush(303).if_icmpne(skip);
                    cb.iinc(1, 1);
                    cb.labelBinding(skip);
                    cb.labelBinding(next);
                    cb.iinc(2, 1).goto_(head);
                    cb.labelBinding(done);
                    cb.iload(1).ireturn();
                }));
    }

    /// Child-first for its own names, parent = the platform loader.
    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(ClassLoader.getPlatformClassLoader());
            bytes.put(PKG + "X500Principal", principal());
            bytes.put("l2h.User", user());
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

    static String call(Method m, Object arg) {
        try {
            return String.valueOf(m.invoke(null, arg));
        } catch (InvocationTargetException e) {
            return "error " + e.getCause();
        } catch (ReflectiveOperationException e) {
            return "error " + e;
        }
    }

    public static void main(String[] args) throws Exception {
        Class.forName("javax.security.auth.x500.X500Principal");
        Class<?> user = new Loader().loadClass("l2h.User");
        Method probe = user.getMethod("probe", Object.class);
        Method cast = user.getMethod("cast", Object.class);
        Method make = user.getMethod("make");
        // The first calls: compiled here by the eager door under
        // CRATONVM_BG_COMPILE=0, before the loader was asked for the name.
        for (int i = 0; i < 3; i++) {
            call(probe, "warm");
            call(cast, null);
        }
        Object principal = make.invoke(null);
        System.out.println("eager instanceof: " + call(probe, principal));
        System.out.println("eager checkcast: " + call(cast, principal));
        // A second loader, so nothing has asked it for the name when `loop`
        // is compiled.
        Method loop = new Loader().loadClass("l2h.User").getMethod("loop", int.class);
        System.out.println("loop: " + call(loop, 60_000));
    }
}
