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
// Wave 49 (lane L2) made the probe deterministic against HotSpot 25 with
// DEFAULT flags (the reference the probe harness runs). HotSpot's C1 / C2
// look the owner up by name in the loader's dictionary (`ciEnv`), where
// `Other`'s resolution has put it, and bind the call: a compiled
// `applyAsInt` returns 8 from then on, so the old `hot failures` count was
// "calls made before the compile landed" -- 303 to 6939 of 50000 on one box,
// 571 on the host, never the same twice. CratonVM keeps §5.4.3 in both tiers
// (HotSpot's interpreter does too; the decision and its reasons are in
// docs/internal/fixed-bugs/interpreter-L2-hotspot-compiled-code-binds-a-call-whose-owner-resolution-failed-FIXED-20261013.md).
// So the probe now prints only what HotSpot answers the same way on every run:
//
// * `huge ...` rows: `l5gen.Static` has a second method, `IntBinaryOperator
//   .applyAsInt`, that calls `Opt.m()` through the SAME constant-pool entry
//   and is padded past HotSpot's 8000-byte `HugeMethodLimit`
//   (`DontCompileHugeMethods` is on by default), so HotSpot always runs it in
//   the interpreter and every call rethrows the record. CratonVM's
//   single-pass tier has no such limit (only its optimizing tier does,
//   `ir::IR_MAX_BYTECODE_SIZE`), so with the JIT on this is the row that
//   checks CratonVM's COMPILED site against a deterministic HotSpot answer.
//   The padding is skipped at run time (`ifge` over it), so the loop is cheap.
// * `cold failures`: the small `applyAsInt`'s first 100 calls after `Opt`
//   loaded -- fewer than HotSpot's tier-3 invocation threshold (200), so they
//   run interpreted on HotSpot and fail.
// * `hot outcomes`: the small method's next 50000 calls each either fail
//   with `NoClassDefFoundError` or return 8 (HotSpot: some of each; CratonVM:
//   all failures); anything else (another error, another value) is counted
//   out.
//
// The huge method's first call comes right after the first failure (a cold
// site of the same entry) and again hot, after `Opt` loaded (wave-48 rule:
// the fixed case first and again later).
//
// Run (no setup):
//   javac -d out L5W26CompiledOwnerFailureRecord.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W26CompiledOwnerFailureRecord
//
// Expected output — HotSpot 25.0.3, default flags, and `-Xint` (compare verbatim):
//   first: java.lang.NoClassDefFoundError
//   huge first: java.lang.NoClassDefFoundError
//   other: 8
//   cold failures: 100 of 100
//   huge hot failures: 50000 of 50000
//   hot outcomes: 50000 of 50000
//   loader requests: 2
//
// CratonVM before wave 26 (from the code, not run): with the JIT on, once
// `Static.applyAsInt` was compiled its call bound `Opt.m()` by name, as
// HotSpot's does; `--nojit` matched. See
// docs/internal/fixed-bugs/interpreter-L5-invokevirtual-does-not-resolve-an-unloaded-owner-before-the-null-check-FIXED-20260930.md, (b).

import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.HashMap;
import java.util.Map;
import java.util.function.Consumer;
import java.util.function.IntBinaryOperator;
import java.util.function.IntUnaryOperator;

public class L5W26CompiledOwnerFailureRecord {
    static final ClassDesc OPT = ClassDesc.of("l5gen.Opt");
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc INT_INT =
            MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
    static final MethodTypeDesc INT_INT_INT =
            MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int, ConstantDescs.CD_int);
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    // Past HotSpot's HugeMethodLimit (8000 bytes of bytecode).
    static final int PADDING = 8100;

    static byte[] opt() {
        return ClassFile.of().build(OPT, clb -> clb
                .withFlags(PUBLIC)
                .withMethodBody("m", INT, PUBLIC | ClassFile.ACC_STATIC,
                        cb -> cb.bipush(8).ireturn()));
    }

    // `applyAsInt(int, int)`: if the first argument is >= 0, jump over the
    // padding straight to the call; the padding is reachable (a negative
    // argument runs it), so no dead code.
    static void huge(CodeBuilder cb) {
        Label call = cb.newLabel();
        cb.iload(1).ifge(call);
        for (int i = 0; i < PADDING; i++) {
            cb.nop();
        }
        cb.labelBinding(call);
        cb.invokestatic(OPT, "m", INT).ireturn();
    }

    static byte[] user(String name, Consumer<CodeBuilder> body, boolean withHuge) {
        return ClassFile.of().build(ClassDesc.of(name), clb -> {
            clb.withFlags(PUBLIC);
            if (withHuge) {
                clb.withInterfaceSymbols(
                        ClassDesc.of("java.util.function.IntUnaryOperator"),
                        ClassDesc.of("java.util.function.IntBinaryOperator"));
                clb.withMethodBody("applyAsInt", INT_INT_INT, PUBLIC,
                        L5W26CompiledOwnerFailureRecord::huge);
            } else {
                clb.withInterfaceSymbols(ClassDesc.of("java.util.function.IntUnaryOperator"));
            }
            clb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                    cb -> cb.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    ConstantDescs.MTD_void)
                            .return_());
            clb.withMethodBody("applyAsInt", INT_INT, PUBLIC, body);
        });
    }

    static final class Flaky extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();
        int optRequests;

        Flaky() {
            super(ClassLoader.getPlatformClassLoader());
            bytes.put("l5gen.Opt", opt());
            bytes.put("l5gen.Static", user("l5gen.Static",
                    cb -> cb.invokestatic(OPT, "m", INT).ireturn(), true));
            bytes.put("l5gen.Other", user("l5gen.Other",
                    cb -> cb.invokestatic(OPT, "m", INT).ireturn(), false));
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

        Object make(String name) throws Exception {
            return loadClass(name).getConstructor().newInstance();
        }
    }

    public static void main(String[] args) throws Exception {
        Flaky loader = new Flaky();
        Object st = loader.make("l5gen.Static");
        IntUnaryOperator small = (IntUnaryOperator) st;
        IntBinaryOperator huge = (IntBinaryOperator) st;
        try {
            System.out.println("first: " + small.applyAsInt(0));
        } catch (Throwable t) {
            System.out.println("first: " + t.getClass().getName());
        }
        try {
            System.out.println("huge first: " + huge.applyAsInt(0, 0));
        } catch (Throwable t) {
            System.out.println("huge first: " + t.getClass().getName());
        }
        System.out.println("other: "
                + ((IntUnaryOperator) loader.make("l5gen.Other")).applyAsInt(0));

        final int cold = 100;
        int coldFailures = 0;
        for (int i = 0; i < cold; i++) {
            try {
                small.applyAsInt(i);
            } catch (NoClassDefFoundError e) {
                coldFailures++;
            }
        }
        System.out.println("cold failures: " + coldFailures + " of " + cold);

        final int calls = 50000;
        int hugeFailures = 0;
        for (int i = 0; i < calls; i++) {
            try {
                huge.applyAsInt(i, i);
            } catch (NoClassDefFoundError e) {
                hugeFailures++;
            }
        }
        System.out.println("huge hot failures: " + hugeFailures + " of " + calls);

        int outcomes = 0;
        for (int i = 0; i < calls; i++) {
            try {
                if (small.applyAsInt(i) == 8) {
                    outcomes++;
                }
            } catch (NoClassDefFoundError e) {
                outcomes++;
            }
        }
        System.out.println("hot outcomes: " + outcomes + " of " + calls);
        System.out.println("loader requests: " + loader.optRequests);
    }
}
