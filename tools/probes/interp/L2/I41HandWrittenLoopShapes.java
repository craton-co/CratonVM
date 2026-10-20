// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Shape 3 of
// docs/internal/fixed-bugs/i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled-FIXED-20261007.md:
// loops javac never emits, built with java.lang.classfile, whose back-edge
// polls the single-pass admission refused, so an activation running them
// finished compiled after an agent's retransform withdrew its body.
//
// Each loop reads a spliced `Callee.value()` until it differs from the value
// it was given, for at most LIMIT iterations, and answers how many were left
// (-1: never saw a new value). An agent retransforms `Callee` 300 ms into the
// call. HotSpot deoptimizes the running loop and reads the new value at once.
//
// Rows (`seen=true`: the running loop read the retransformed value):
//   indy-header  an unconditional back edge whose loop header is an
//                `invokedynamic` (the header's map may be the indy trap's
//                snapshot; refused as `invokedynamic-header`)
//   below-int    a CONDITIONAL back edge with an int on the operand stack
//                below its own operand (refused as
//                `operands-below-the-branch`)
//   below-ref    the same with a reference below
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3 with the agent, default flags and -Xint)
// indy-header seen=true
// below-int seen=true
// below-ref seen=true
// HOTSPOT_EXPECTED_END
// Without the agent both VMs print `no agent`.
//
// CratonVM (debug build, measured): the single-pass control is
// `CRATONVM_TIER_C2_THRESHOLD=1000000000 CRATONVM_JIT_OSR_OPTIMIZING=0`.
// Before the shape-3 fix `below-int` and `below-ref` printed `seen=false`
// there (`CRATONVM_DBG_JITC=1`: `[jit-mode-exit-refused]
// operands-below-the-branch`); after it, HotSpot's lines. `indy-header`
// printed `true` before the fix too: a bridged `invokedynamic` is a call, and
// the post-call exit after it leaves although the header map is refused
// (`invokedynamic-header`); the `goto` fallback serves a header whose
// `invokedynamic` makes no call. Default flags: HotSpot's lines before and
// after (the optimizing tier owns these loops).
//
// SETUP: a jar whose manifest has
//     Premain-Class: I41HandWrittenLoopShapes$Agent
//     Can-Retransform-Classes: true
// containing I41HandWrittenLoopShapes*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar I41HandWrittenLoopShapes
import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.invoke.CallSite;
import java.lang.invoke.ConstantCallSite;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;
import java.util.function.Consumer;

public class I41HandWrittenLoopShapes {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Callee {
        public static int value() {
            return 12345;
        }
    }

    /** Rewrites `sipush 12345; ireturn` to answer 12345 + n on the n-th transform. */
    static final class Bump implements ClassFileTransformer {
        int bumps;

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"I41HandWrittenLoopShapes$Callee".equals(className)) {
                return null;
            }
            bumps++;
            byte[] out = bytes.clone();
            for (int i = 0; i + 3 < out.length; i++) {
                if (out[i] == 0x11 && out[i + 1] == 0x30 && out[i + 2] == 0x39
                        && out[i + 3] == (byte) 0xAC) {
                    out[i + 2] = (byte) (0x39 + bumps);
                }
            }
            return out;
        }
    }

    public static int tick() {
        return 0;
    }

    public static CallSite bsm(MethodHandles.Lookup lookup, String name, MethodType type)
            throws ReflectiveOperationException {
        return new ConstantCallSite(lookup.findStatic(I41HandWrittenLoopShapes.class, "tick", type));
    }

    static final ClassDesc HOST = ClassDesc.of("I41HandWrittenLoopShapes");
    static final ClassDesc CALLEE = ClassDesc.of("I41HandWrittenLoopShapes$Callee");
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc SPIN =
            MethodTypeDesc.of(ConstantDescs.CD_long, ConstantDescs.CD_int, ConstantDescs.CD_long);
    static final long LIMIT = 4_000_000_000L;

    /** Written every iteration, so no compiler may delete the loop. */
    public static volatile long sink;

    /**
     * `if (Callee.value() != v0) goto found; sink = n; if (--n <= 0) goto none;`,
     * then the back edge.
     */
    static void countDown(CodeBuilder code, Label found) {
        code.invokestatic(CALLEE, "value", INT)
                .iload(0)
                .if_icmpne(found)
                .lload(1)
                .putstatic(HOST, "sink", ConstantDescs.CD_long)
                .lload(1)
                .lconst_1()
                .lsub()
                .dup2()
                .lstore(1)
                .lconst_0()
                .lcmp();
    }

    static byte[] generate() {
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, HOST, "bsm",
                MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                        ConstantDescs.CD_String, ConstantDescs.CD_MethodType));
        int flags = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
        return ClassFile.of().build(ClassDesc.of("I41HandWrittenLoopShapesGen"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("indyHeader", SPIN, flags, code -> {
                Label head = code.newLabel();
                Label found = code.newLabel();
                Label none = code.newLabel();
                code.labelBinding(head)
                        .invokedynamic(DynamicCallSiteDesc.of(bsm, "tick", INT))
                        .pop();
                countDown(code, found);
                code.ifle(none)
                        .goto_(head)
                        .labelBinding(found)
                        .lload(1)
                        .lreturn()
                        .labelBinding(none)
                        .loadConstant(-1L)
                        .lreturn();
            });
            Consumer<CodeBuilder> belowInt = code -> code.iconst_5();
            Consumer<CodeBuilder> belowRef = code -> code.loadConstant("below");
            for (Object[] row : new Object[][] {{"belowInt", belowInt}, {"belowRef", belowRef}}) {
                @SuppressWarnings("unchecked")
                Consumer<CodeBuilder> push = (Consumer<CodeBuilder>) row[1];
                cb.withMethodBody((String) row[0], SPIN, flags, code -> {
                    Label head = code.newLabel();
                    Label found = code.newLabel();
                    push.accept(code);
                    code.labelBinding(head);
                    countDown(code, found);
                    code.ifgt(head)
                            .pop()
                            .loadConstant(-1L)
                            .lreturn()
                            .labelBinding(found)
                            .pop()
                            .lload(1)
                            .lreturn();
                });
            }
        });
    }

    static boolean seen(Method m) throws Exception {
        // Many short calls, then time for the background compiler: the long
        // call below must run a METHOD-ENTRY body (OSR entry refuses a loop
        // header with a live operand stack).
        for (int i = 0; i < 20000; i++) {
            m.invoke(null, Callee.value(), 50L);
        }
        Thread.sleep(1000);
        for (int i = 0; i < 2000; i++) {
            m.invoke(null, Callee.value(), 50L);
        }
        int before = Callee.value();
        Thread swap = new Thread(() -> {
            try {
                Thread.sleep(300);
                inst.retransformClasses(Callee.class);
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
        });
        swap.start();
        long left = (Long) m.invoke(null, before, LIMIT);
        swap.join();
        return left != -1;
    }

    public static void main(String[] args) throws Exception {
        if (inst == null || !inst.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        inst.addTransformer(new Bump(), true);
        Class<?> gen = MethodHandles.lookup().defineClass(generate());
        for (String[] row : new String[][] {
                {"indy-header", "indyHeader"}, {"below-int", "belowInt"}, {"below-ref", "belowRef"}}) {
            Method m = gen.getMethod(row[1], int.class, long.class);
            System.out.println(row[0] + " seen=" + seen(m));
        }
    }
}
