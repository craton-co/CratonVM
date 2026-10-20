// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 125, lane L5 --
// docs/internal/fixed-bugs/interpreter-i125-L9-global-name-guess-filter-can-leave-a-site-uncached-FIXED-20261224.md.
//
// A child-first loader C (parent: the platform loader) defines `l5n125.Caller`
// and, asked for `javax.security.auth.x500.X500Principal`, delegates it to a
// SECOND user loader P that is not its parent (CratonVM does not record P as
// C's parent). P defines its own `X500Principal` (`static int v()`, a no-arg
// constructor, `int w()`). java.base's `X500Principal` is loaded first, so the
// loader-blind flat lookup answers java.base's class (G) while C's recorded
// answer is P's (Y).
//
// `Caller.run(n)` executes one `invokestatic X500Principal.v()` site and one
// `new X500Principal; invokevirtual w()` site n times. HotSpot asks C once
// for the name (a dictionary hit after that). Wave-124's
// `global_name_owner_is_a_guess` compared the recorded answer with the flat
// lookup's owner and kept answering "guess", while the receiver door's early
// return saw the recorded answer and asked nothing: the sites never cached.
//
// Run: javac -d . L5W125GuessFilterUnrecordedParent.java
//      java -cp . L5W125GuessFilterUnrecordedParent   (HotSpot)
//      CRATONVM_DBG=access cratonvm --jdk-only|--compatible -cp . L5W125GuessFilterUnrecordedParent 2>err.txt
// Pass: stdout = the expected block (the `asks` stay 1 from 1 to 1000
// iterations), and the count of `[ACCESS-DBG] OWNER` lines naming
// `javax/security/auth/x500/X500Principal` (`receiver_owner_unresolved`) in
// err.txt does not grow with the iterations: run once with the argument `1`
// and once with no argument (1000) and compare the two counts.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags and -Xint)
// iterations=1 sum=126 asks=1 loader=P
// iterations=1000 sum=126000 asks=1 loader=P
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.function.IntUnaryOperator;

public class L5W125GuessFilterUnrecordedParent {
    static final String NAME = "javax.security.auth.x500.X500Principal";
    static final ClassDesc PRINCIPAL = ClassDesc.of(NAME);
    static final ClassDesc CALLER = ClassDesc.of("l5n125.Caller");
    static final ClassDesc I = ConstantDescs.CD_int;
    static final MethodTypeDesc INT = MethodTypeDesc.of(I);
    static final MethodTypeDesc INT_INT = MethodTypeDesc.of(I, I);
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;

    static byte[] principal() {
        return ClassFile.of().build(PRINCIPAL, clb -> clb
                .withFlags(PUBLIC)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("v", INT, PUBLIC | STATIC, cb -> cb.bipush(100).ireturn())
                .withMethodBody("w", INT, PUBLIC, cb -> cb.bipush(26).ireturn()));
    }

    /// `applyAsInt(n)`: sum over n iterations of `X500Principal.v()` +
    /// `new X500Principal().w()`.
    static byte[] caller() {
        return ClassFile.of().build(CALLER, clb -> clb
                .withFlags(PUBLIC)
                .withInterfaceSymbols(ClassDesc.of("java.util.function.IntUnaryOperator"))
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("applyAsInt", INT_INT, PUBLIC, cb -> {
                    var loop = cb.newLabel();
                    var done = cb.newLabel();
                    // locals: 1 = n, 2 = i, 3 = sum
                    cb.iconst_0().istore(2).iconst_0().istore(3);
                    cb.labelBinding(loop);
                    cb.iload(2).iload(1).if_icmpge(done);
                    cb.iload(3).invokestatic(PRINCIPAL, "v", INT).iadd().istore(3);
                    cb.iload(3)
                            .new_(PRINCIPAL).dup()
                            .invokespecial(PRINCIPAL, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .invokevirtual(PRINCIPAL, "w", INT)
                            .iadd().istore(3);
                    cb.iinc(2, 1).goto_(loop);
                    cb.labelBinding(done);
                    cb.iload(3).ireturn();
                }));
    }

    /// Defines its own `X500Principal` on request; everything else from the
    /// platform loader.
    static final class P extends ClassLoader {
        P() {
            super("P", ClassLoader.getPlatformClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (name.equals(NAME)) {
                    byte[] b = principal();
                    return defineClass(name, b, 0, b.length);
                }
                return super.loadClass(name, resolve);
            }
        }
    }

    /// Child-first for `l5n125.Caller`; delegates `X500Principal` to `p`,
    /// which is not its parent; everything else to the platform loader.
    static final class C extends ClassLoader {
        final P p;
        int asks;

        C(P p) {
            super("C", ClassLoader.getPlatformClassLoader());
            this.p = p;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(NAME)) {
                asks++;
                return p.loadClass(name);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (name.equals("l5n125.Caller")) {
                    byte[] b = caller();
                    return defineClass(name, b, 0, b.length);
                }
                return super.loadClass(name, resolve);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 1000;
        Class.forName(NAME);
        P p = new P();
        C c = new C(p);
        IntUnaryOperator op =
                (IntUnaryOperator) c.loadClass("l5n125.Caller").getConstructor().newInstance();
        int first = op.applyAsInt(1);
        String owner = p.loadClass(NAME).getClassLoader() == p ? "P" : "other";
        System.out.println("iterations=1 sum=" + first + " asks=" + c.asks + " loader=" + owner);
        if (n > 1) {
            int sum = op.applyAsInt(n);
            System.out.println("iterations=" + n + " sum=" + sum + " asks=" + c.asks + " loader=" + owner);
        }
    }
}
