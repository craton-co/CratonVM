// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 34 (orchestrator): which frames a Throwable's
// stack trace shows around hidden code
// (docs/internal/fixed-bugs/interpreter-L4-cross-loader-type-checks-and-trace-shapes-FIXED-20261001.md,
// item 4). HotSpot hides the frames of hidden classes and of `@Hidden`
// methods unless `-XX:+ShowHiddenFrames`.
//
// Each row prints the trace's method names (class simple name + method),
// innermost first, up to this probe's own `row` frame.
//
//   hidden-class  a method of a class defined with `Lookup.defineHiddenClass`
//                 throws; the trace skips it
//   scoped-value  `ScopedValue.where(k, v).run(r)`: `Carrier.runWith` is
//                 `@Hidden`
//   lambda        a lambda body throws (the proxy's frame is hidden)
//   virtual       a virtual thread's task throws: does the `@Hidden`
//                 `Thread.runWith` frame show? (Only that is printed: the
//                 frames below it depend on whether the JDK runs virtual
//                 threads on continuations.)
//
// Before wave 34 CratonVM showed the hidden class's `go` frame (named
// `0x0`), `ScopedValue$Carrier.runWith` and `Thread.runWith`. `--compatible`
// keeps them.
//
// Run: javac -d out L4W34HiddenFrames.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W34HiddenFrames
//
// Expected HotSpot 25 output (default and -Xint):
//   hidden-class: L4W34HiddenFrames.boom < L4W34HiddenFrames.row
//   scoped-value: L4W34HiddenFrames.boom < ScopedValueContainer.runWithoutScope < ScopedValueContainer.run < ScopedValue$Carrier.run < L4W34HiddenFrames.row
//   lambda: L4W34HiddenFrames.boom < L4W34HiddenFrames.lambda$row$2 < L4W34HiddenFrames.row
//   virtual runWith shown: false
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicReference;

public class L4W34HiddenFrames {
    static String names(Throwable t) {
        List<String> out = new ArrayList<>();
        for (StackTraceElement e : t.getStackTrace()) {
            String cls = e.getClassName();
            int slash = cls.indexOf('/');
            if (slash >= 0) {
                cls = cls.substring(0, slash) + "/<hidden>";
            }
            String simple = cls.substring(cls.lastIndexOf('.') + 1);
            out.add(simple + "." + e.getMethodName());
            if (e.getMethodName().equals("row")) {
                break;
            }
        }
        return String.join(" < ", out);
    }

    static final ScopedValue<String> KEY = ScopedValue.newInstance();

    static void boom() {
        throw new IllegalStateException("boom");
    }

    static String row(String what) throws Throwable {
        try {
            switch (what) {
                case "hidden-class" -> {
                    byte[] b = ClassFile.of().build(ClassDesc.of("L4W34HiddenFrames$Hid"), cb -> {
                        cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
                        cb.withMethodBody("go", MethodTypeDesc.of(ConstantDescs.CD_void),
                                ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                                code -> code.invokestatic(ClassDesc.of("L4W34HiddenFrames"), "boom",
                                        MethodTypeDesc.of(ConstantDescs.CD_void)).return_());
                    });
                    Class<?> h = MethodHandles.lookup().defineHiddenClass(b, true).lookupClass();
                    MethodHandles.lookup().findStatic(h, "go", java.lang.invoke.MethodType.methodType(void.class))
                            .invokeExact();
                }
                case "scoped-value" -> ScopedValue.where(KEY, "v").run(L4W34HiddenFrames::boom);
                case "lambda" -> {
                    Runnable r = () -> boom();
                    r.run();
                }
                default -> throw new IllegalArgumentException(what);
            }
            return "no exception";
        } catch (IllegalStateException e) {
            return names(e);
        }
    }

    static String virtual() throws Exception {
        AtomicReference<Throwable> seen = new AtomicReference<>();
        Thread t = Thread.ofVirtual().unstarted(L4W34HiddenFrames::boom);
        t.setUncaughtExceptionHandler((th, e) -> seen.set(e));
        t.start();
        t.join();
        for (StackTraceElement e : seen.get().getStackTrace()) {
            if (e.getClassName().equals("java.lang.Thread") && e.getMethodName().equals("runWith")) {
                return "true";
            }
        }
        return "false";
    }

    public static void main(String[] args) throws Throwable {
        for (String what : new String[] {"hidden-class", "scoped-value", "lambda"}) {
            System.out.println(what + ": " + row(what));
        }
        System.out.println("virtual runWith shown: " + virtual());
    }
}
