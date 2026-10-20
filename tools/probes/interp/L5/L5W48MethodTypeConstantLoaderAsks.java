// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L5: which of a `CONSTANT_MethodType`'s
// types the VM asks the resolving class's loader for
// (docs/internal/fixed-bugs/interpreter-L5-proposal-method-types-resolve-their-types-in-the-vm-FIXED-20261012.md).
//
// HotSpot's `SystemDictionary::find_method_handle_type` walks the descriptor
// in order (parameters, then the return type) with the NULL loader while
// every reference type so far is "always visible" (a public class of
// `java.lang` or `java.lang.invoke`; a primitive or a primitive array keeps
// the walk going); from the first other type on, it resolves every remaining
// reference type through the class's loader. CratonVM called the JDK's
// `MethodType.fromMethodDescriptorString(desc, loader)`, whose
// `Class.forName(name, false, loader)` asks the loader for EVERY reference
// type.
//
// A user loader defines the generated class `Gen` (the `java.lang.classfile`
// API writes its `ldc MethodType` constants) and logs each `loadClass` name
// it is asked for while one `ldc` runs. Rows (the names asked, in order):
//   lang      -- (Ljava/lang/String;Ljava/lang/Runnable;)V: none;
//   invoke    -- (Ljava/lang/invoke/MethodHandle;[Ljava/lang/Object;I)V: none;
//   util      -- (Ljava/util/concurrent/Exchanger;)V: Exchanger;
//   util-last -- (Ljava/lang/String;Ljava/util/concurrent/Phaser;)Ljava/lang/Thread;:
//                Phaser, then Thread (the return type, after the walk left
//                the null loader), not String;
//   util-first-- (Ljava/util/concurrent/Semaphore;Ljava/lang/Runtime;)V:
//                Semaphore, Runtime.
// Each row names classes no earlier row named: the VM does not ask a loader
// again for a name it initiated (its dictionary answers), so a repeated name
// would be missing from a later row on HotSpot.
//   own       -- (LL5W48MethodTypeConstantLoaderAsks$Own;)V: the class
//                itself (not always visible), answered by the loader.
//
// HotSpot 25 prints (measured, JDK 25.0.3; the same with -Xint):
//     lang []
//     invoke []
//     util [java.util.concurrent.Exchanger]
//     util-last [java.util.concurrent.Phaser, java.lang.Thread]
//     util-first [java.util.concurrent.Semaphore, java.lang.Runtime]
//     own [L5W48MethodTypeConstantLoaderAsks$Own]
// CratonVM before wave 48 (from the code): every reference type is asked,
// `lang [java.lang.String, java.lang.Runnable]`, `invoke
// [java.lang.invoke.MethodHandle, java.lang.Object]` (the array's element).
import java.io.InputStream;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Function;

public class L5W48MethodTypeConstantLoaderAsks {
    /** Named by the `own` row; the logging loader defines its own copy. */
    public static class Own {
    }

    static final String[] ROWS = {
        "lang", "(Ljava/lang/String;Ljava/lang/Runnable;)V",
        "invoke", "(Ljava/lang/invoke/MethodHandle;[Ljava/lang/Object;I)V",
        "util", "(Ljava/util/concurrent/Exchanger;)V",
        "util-last", "(Ljava/lang/String;Ljava/util/concurrent/Phaser;)Ljava/lang/Thread;",
        "util-first", "(Ljava/util/concurrent/Semaphore;Ljava/lang/Runtime;)V",
        "own", "(LL5W48MethodTypeConstantLoaderAsks$Own;)V",
    };

    static final class Logging extends ClassLoader {
        final List<String> asked = new ArrayList<>();
        volatile boolean logging;
        final byte[] gen;

        Logging(byte[] gen) {
            super(L5W48MethodTypeConstantLoaderAsks.class.getClassLoader());
            this.gen = gen;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (logging) {
                    asked.add(name);
                }
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (name.equals("Gen")) {
                    return defineClass(name, gen, 0, gen.length);
                }
                if (name.endsWith("$Own")) {
                    try (InputStream in = L5W48MethodTypeConstantLoaderAsks.class
                            .getResourceAsStream("L5W48MethodTypeConstantLoaderAsks$Own.class")) {
                        byte[] b = in.readAllBytes();
                        return defineClass(name, b, 0, b.length);
                    } catch (java.io.IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return super.loadClass(name, resolve);
            }
        }
    }

    /** `Gen implements Function<Object,Object>`; `apply(i)` runs the `ldc` of row `i`. */
    static byte[] gen() {
        ClassDesc me = ClassDesc.of("Gen");
        ClassDesc fn = ClassDesc.of("java.util.function.Function");
        return ClassFile.of().build(me, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(fn);
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    ConstantDescs.MTD_void)
                            .return_());
            for (int i = 0; i < ROWS.length / 2; i++) {
                MethodTypeDesc mt = MethodTypeDesc.ofDescriptor(ROWS[2 * i + 1]);
                cb.withMethodBody("row" + i, MethodTypeDesc.of(ConstantDescs.CD_Object),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                        code -> code.ldc(mt).areturn());
            }
            cb.withMethodBody("apply", MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC, code -> {
                        for (int i = 0; i < ROWS.length / 2; i++) {
                            var next = code.newLabel();
                            code.aload(1)
                                    .checkcast(ConstantDescs.CD_Integer)
                                    .invokevirtual(ConstantDescs.CD_Integer, "intValue",
                                            MethodTypeDesc.of(ConstantDescs.CD_int))
                                    .ldc(i)
                                    .if_icmpne(next)
                                    .invokestatic(me, "row" + i, MethodTypeDesc.of(ConstantDescs.CD_Object))
                                    .areturn()
                                    .labelBinding(next);
                        }
                        code.aconst_null().areturn();
                    });
        });
    }

    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Exception {
        Logging loader = new Logging(gen());
        Function<Object, Object> g = (Function<Object, Object>) loader.loadClass("Gen")
                .getConstructor().newInstance();
        // Resolves `apply`'s own references (Integer) before anything is logged.
        g.apply(-1);
        for (int i = 0; i < ROWS.length / 2; i++) {
            List<String> asked;
            synchronized (loader) {
                loader.asked.clear();
                loader.logging = true;
            }
            String outcome;
            try {
                Object mt = g.apply(i);
                outcome = mt == null ? " null" : "";
            } catch (Throwable t) {
                outcome = " " + t;
            }
            synchronized (loader) {
                loader.logging = false;
                asked = new ArrayList<>(loader.asked);
            }
            System.out.println(ROWS[2 * i] + " " + asked + outcome);
        }
    }
}
