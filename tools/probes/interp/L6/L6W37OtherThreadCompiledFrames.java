// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L6: another thread's
// `Thread.getStackTrace()` / `Thread.getAllStackTraces()` while that thread is
// parked under COMPILED frames, one of them a hidden class's method
// (docs/internal/fixed-bugs/interpreter-L6-another-threads-trace-omits-compiled-frames-FIXED-20261002.md;
// the interpreted shape is `L4/L4W35OtherThreadHidden`).
//
// A platform thread runs `hot` 40,000 times through a `defineHiddenClass`
// `Op` (so `hot`, `drive`, the hidden `apply` and `leaf` are compiled or
// inlined), then its last call parks inside `leaf`. `main` reads the trace and
// prints only this probe's own frames up to `hot` (the JDK's park internals
// and the thread's entry vary), innermost first.
//
// HotSpot leaves the hidden `apply` out of `getStackTrace()` and keeps it in
// `getAllStackTraces()`. CratonVM published another thread's trace from its
// INTERPRETER frames only (`stackwalker::capture_frames_no_lines`), so a
// compiled frame was missing from both (see the page). Since wave 38 (lane L7)
// the publishing thread interleaves its compiled activations
// (`stackwalker::capture_published_trace`); positive control:
// CRATONVM_DBG_STTRACE=1 prints `[sttrace] publish: interpreted=N
// compiled-activations=M entries=K` with M > 0 in default mode.
//
// Run: javac -d out L6W37OtherThreadCompiledFrames.java && cratonvm --java-home <jdk25> [--nojit] -cp out L6W37OtherThreadCompiledFrames
//
// Expected HotSpot 25 output (default and -Xint):
//   getStackTrace: L6W37OtherThreadCompiledFrames.leaf < L6W37OtherThreadCompiledFrames.drive < L6W37OtherThreadCompiledFrames.hot
//   getAllStackTraces: L6W37OtherThreadCompiledFrames.leaf < L6W37OtherThreadCompiledFrames$Hid/<hidden>.apply < L6W37OtherThreadCompiledFrames.drive < L6W37OtherThreadCompiledFrames.hot
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;

public class L6W37OtherThreadCompiledFrames {
    public interface Op {
        int apply(int x);
    }

    static final CountDownLatch PARKED = new CountDownLatch(1);
    static final CountDownLatch RELEASE = new CountDownLatch(1);

    public static int leaf(int x) {
        if (x < 0) {
            PARKED.countDown();
            try {
                RELEASE.await();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
            return 0;
        }
        return x + 1;
    }

    static int drive(Op op, int x) {
        return op.apply(x);
    }

    static int hot(Op op) {
        int sum = 0;
        for (int i = 0; i < 40_000; i++) {
            sum += drive(op, i);
        }
        return sum + drive(op, -1);
    }

    static Op hiddenOp() throws Throwable {
        ClassDesc self = ClassDesc.of("L6W37OtherThreadCompiledFrames");
        MethodTypeDesc intInt = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
        byte[] b = ClassFile.of().build(ClassDesc.of("L6W37OtherThreadCompiledFrames$Hid"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(ClassDesc.of("L6W37OtherThreadCompiledFrames$Op"));
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .return_());
            cb.withMethodBody("apply", intInt, ClassFile.ACC_PUBLIC,
                    code -> code.iload(1).invokestatic(self, "leaf", intInt).ireturn());
        });
        Class<?> h = MethodHandles.lookup().defineHiddenClass(b, true).lookupClass();
        return (Op) h.getConstructor().newInstance();
    }

    static String names(StackTraceElement[] trace) {
        List<String> out = new ArrayList<>();
        for (StackTraceElement e : trace) {
            String cls = e.getClassName();
            if (!cls.startsWith("L6W37OtherThreadCompiledFrames")) {
                continue;
            }
            int slash = cls.indexOf('/');
            if (slash >= 0) {
                cls = cls.substring(0, slash) + "/<hidden>";
            }
            out.add(cls + "." + e.getMethodName());
            if (e.getMethodName().equals("hot")) {
                break;
            }
        }
        return String.join(" < ", out);
    }

    public static void main(String[] args) throws Throwable {
        Op op = hiddenOp();
        Thread t = new Thread(() -> hot(op), "parked");
        t.start();
        PARKED.await();
        Thread.sleep(200);
        StackTraceElement[] one = t.getStackTrace();
        StackTraceElement[] all = Thread.getAllStackTraces().get(t);
        System.out.println("getStackTrace: " + names(one));
        System.out.println("getAllStackTraces: " + names(all));
        RELEASE.countDown();
        t.join();
    }
}
