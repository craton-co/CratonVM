// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L6: `-XX:MaxJavaStackTraceDepth` counts
// only the frames a Throwable's trace RECORDS, so hidden frames (here a
// `defineHiddenClass` class's `apply`, one per level of a recursion through
// it) do not use up the depth
// (docs/internal/fixed-bugs/interpreter-L4-cross-loader-type-checks-and-trace-shapes-FIXED-20261001.md,
// item 4). Wave 34 dropped hidden frames AFTER the capture had kept the
// innermost N, so a trace through hidden frames came out shorter than N:
// predicted `frames: 6` / `step frames: 6` here (read from
// `vm_exec.rs` `capture_throwable_stack_trace`, not measured).
//
// Rows:
//   frames        the trace's length (the cap, 12)
//   step frames   how many of them are this probe's `step` frames
//   hidden frames how many name a hidden class (`/0x` in the class name)
//
// Run: javac -d out L6W37HiddenFramesDepthCap.java && cratonvm --java-home <jdk25> [--nojit] -XX:MaxJavaStackTraceDepth=12 -cp out L6W37HiddenFramesDepthCap
//
// Expected HotSpot 25 output (default and -Xint, -XX:MaxJavaStackTraceDepth=12):
//   frames: 12
//   step frames: 12
//   hidden frames: 0
// (With -XX:+UnlockDiagnosticVMOptions -XX:+ShowHiddenFrames HotSpot prints
// 12 / 6 / 6.) `--compatible` keeps the hidden frames: 12 / 6 / 6.
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;

public class L6W37HiddenFramesDepthCap {
    public interface Op {
        int apply(int n);
    }

    static Op op;

    public static int step(int n) {
        if (n == 0) {
            throw new IllegalStateException("bottom");
        }
        return op.apply(n - 1) + 1;
    }

    public static void main(String[] args) throws Throwable {
        ClassDesc self = ClassDesc.of("L6W37HiddenFramesDepthCap");
        MethodTypeDesc intInt = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
        byte[] b = ClassFile.of().build(ClassDesc.of("L6W37HiddenFramesDepthCap$Hid"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(ClassDesc.of("L6W37HiddenFramesDepthCap$Op"));
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .return_());
            cb.withMethodBody("apply", intInt, ClassFile.ACC_PUBLIC,
                    code -> code.iload(1).invokestatic(self, "step", intInt).ireturn());
        });
        Class<?> h = MethodHandles.lookup().defineHiddenClass(b, true).lookupClass();
        op = (Op) h.getConstructor().newInstance();
        try {
            step(40);
        } catch (IllegalStateException e) {
            StackTraceElement[] trace = e.getStackTrace();
            int steps = 0;
            int hidden = 0;
            for (StackTraceElement f : trace) {
                if (f.getMethodName().equals("step")) {
                    steps++;
                }
                if (f.getClassName().contains("/0x")) {
                    hidden++;
                }
            }
            System.out.println("frames: " + trace.length);
            System.out.println("step frames: " + steps);
            System.out.println("hidden frames: " + hidden);
        }
    }
}
