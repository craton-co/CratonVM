// Interpreter round i1, wave 26, lane L5 — JVMS §5.4.3 in COMPILED code: an
// `invokestatic l5gen/Opt.m()` whose `CONSTANT_Class` entry failed to resolve
// keeps failing after another entry (another class's) has loaded `Opt`, also
// once the calling method is hot enough to be compiled. The interpreter
// rethrows the record since wave 25 (`L5W25OwnerFailureRecordInvoke`,
// `static` rows); the compiler's call-site resolver (`jit_bridge.rs`
// `CpResolvers::invoke`) answered the site from the constant pool by name and
// the compiled body could call `Opt.m()` and return 8.
//
// A `Flaky` loader refuses `l5gen.Opt` on its FIRST request and defines it on
// every later one; `l5gen.Static` and `l5gen.Other` are defined by it and each
// call `Opt.m()` through their own constant pool. The classes are generated
// with the java.lang.classfile API so that `l5gen.Opt` exists nowhere on the
// class path.
//
// Run (no setup):
//   javac -d out L5W26CompiledOwnerFailureRecord.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W26CompiledOwnerFailureRecord
//
// Expected output — HotSpot 25 with `-Xint` (compare verbatim):
//   first: java.lang.NoClassDefFoundError
//   other: 8
//   hot failures: 50000 of 50000
//   loader requests: 2
//
// HotSpot 25 WITH its compilers does NOT honour the record in compiled code:
// C1 / C2 look the owner up by name in the loader's dictionary (`ciEnv`),
// where `Other`'s resolution has put it, and bind the call. Measured on the
// lane's box: `hot failures` 303, 316 (default), 323 (`-XX:TieredStopAtLevel=1`),
// 6939 (`-XX:-TieredCompilation`) of 50000 — a count that depends on when the
// compile lands. JVMS §5.4.3 says every later resolution fails; CratonVM
// keeps its two tiers consistent with that (and with HotSpot's interpreter),
// so compare CratonVM, `--nojit` or not, against `-Xint`.
//
// CratonVM before wave 26 (from the code, not run): with the JIT on, once
// `Static.applyAsInt` was compiled its call bound `Opt.m()` by name, as
// HotSpot's does, so `hot failures` fell short of 50000; `--nojit` matched.
// See
// docs/internal/fixed-bugs/interpreter-L5-invokevirtual-does-not-resolve-an-unloaded-owner-before-the-null-check-FIXED-20260930.md, (b).

import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.HashMap;
import java.util.Map;
import java.util.function.Consumer;
import java.util.function.IntUnaryOperator;

public class L5W26CompiledOwnerFailureRecord {
    static final ClassDesc OPT = ClassDesc.of("l5gen.Opt");
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc INT_INT =
            MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
    static final int PUBLIC = ClassFile.ACC_PUBLIC;

    static byte[] opt() {
        return ClassFile.of().build(OPT, clb -> clb
                .withFlags(PUBLIC)
                .withMethodBody("m", INT, PUBLIC | ClassFile.ACC_STATIC,
                        cb -> cb.bipush(8).ireturn()));
    }

    static byte[] user(String name, Consumer<CodeBuilder> body) {
        return ClassFile.of().build(ClassDesc.of(name), clb -> clb
                .withFlags(PUBLIC)
                .withInterfaceSymbols(ClassDesc.of("java.util.function.IntUnaryOperator"))
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("applyAsInt", INT_INT, PUBLIC, body));
    }

    static final class Flaky extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();
        int optRequests;

        Flaky() {
            super(ClassLoader.getPlatformClassLoader());
            bytes.put("l5gen.Opt", opt());
            bytes.put("l5gen.Static", user("l5gen.Static",
                    cb -> cb.invokestatic(OPT, "m", INT).ireturn()));
            bytes.put("l5gen.Other", user("l5gen.Other",
                    cb -> cb.invokestatic(OPT, "m", INT).ireturn()));
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
                if (name.equals("l5gen.Opt") && optRequests++ == 0) {
                    throw new ClassNotFoundException(name);
                }
                return defineClass(name, b, 0, b.length);
            }
        }

        IntUnaryOperator make(String name) throws Exception {
            return (IntUnaryOperator) loadClass(name).getConstructor().newInstance();
        }
    }

    public static void main(String[] args) throws Exception {
        Flaky loader = new Flaky();
        IntUnaryOperator st = loader.make("l5gen.Static");
        try {
            System.out.println("first: " + st.applyAsInt(0));
        } catch (Throwable t) {
            System.out.println("first: " + t.getClass().getName());
        }
        System.out.println("other: " + loader.make("l5gen.Other").applyAsInt(0));
        int failures = 0;
        final int calls = 50000;
        for (int i = 0; i < calls; i++) {
            try {
                st.applyAsInt(i);
            } catch (NoClassDefFoundError e) {
                failures++;
            }
        }
        System.out.println("hot failures: " + failures + " of " + calls);
        System.out.println("loader requests: " + loader.optRequests);
    }
}
