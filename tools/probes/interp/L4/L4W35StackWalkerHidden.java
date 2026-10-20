// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 35 (orchestrator): a StackWalker skips hidden
// frames unless it has `SHOW_HIDDEN_FRAMES`, as HotSpot's stack walk does
// (docs/internal/fixed-bugs/interpreter-L4-cross-loader-type-checks-and-trace-shapes-FIXED-20261001.md,
// item 4). Wave 34 did the same for Throwable traces.
//
// Each row walks from inside a callee and prints whether the hidden frame
// shows, with the default options and with SHOW_HIDDEN_FRAMES:
//
//   scoped-value  inside `ScopedValue.where(k, v).run(r)`:
//                 `ScopedValue$Carrier.runWith` is `@Hidden`
//   hidden-class  inside a method of a `Lookup.defineHiddenClass` class
//   virtual       inside a virtual thread's task: `Thread.runWith` is
//                 `@Hidden`
//
// The name row is the hidden class's frame as a SHOW_HIDDEN_FRAMES walk names
// it: HotSpot keeps the `/0x…` suffix of a hidden class's name.
//
// Before wave 35 CratonVM showed all three with the default options, and
// named the hidden class `…$Hid.0x…`.
// `--compatible` keeps them.
//
// Run: javac -d out L4W35StackWalkerHidden.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W35StackWalkerHidden
//
// Expected HotSpot 25 output (default and -Xint):
//   scoped-value default: false
//   scoped-value SHOW_HIDDEN_FRAMES: true
//   hidden-class default: false
//   hidden-class SHOW_HIDDEN_FRAMES: true
//   hidden-class name: L4W35StackWalkerHidden$Hid/0x<n>
//   virtual default: false
//   virtual SHOW_HIDDEN_FRAMES: true
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.Predicate;

public class L4W35StackWalkerHidden {
    static final ScopedValue<String> KEY = ScopedValue.newInstance();

    static StackWalker plain() {
        return StackWalker.getInstance();
    }

    static StackWalker showing() {
        return StackWalker.getInstance(StackWalker.Option.SHOW_HIDDEN_FRAMES);
    }

    static boolean any(StackWalker w, Predicate<StackWalker.StackFrame> p) {
        return w.walk(s -> s.anyMatch(p));
    }

    static final Predicate<StackWalker.StackFrame> RUN_WITH_CARRIER =
            f -> f.getClassName().equals("java.lang.ScopedValue$Carrier") && f.getMethodName().equals("runWith");
    static final Predicate<StackWalker.StackFrame> HIDDEN_CLASS =
            f -> f.getClassName().contains("$Hid") && f.getMethodName().equals("go");
    static final Predicate<StackWalker.StackFrame> RUN_WITH_THREAD =
            f -> f.getClassName().equals("java.lang.Thread") && f.getMethodName().equals("runWith");

    static final boolean[] SEEN = new boolean[2];
    static String hiddenName = "none";

    /** Called from the hidden class's `go`. */
    public static void probeHidden() {
        SEEN[0] = any(plain(), HIDDEN_CLASS);
        SEEN[1] = any(showing(), HIDDEN_CLASS);
        hiddenName = showing().walk(s -> s.filter(HIDDEN_CLASS).findFirst())
                .map(f -> f.getClassName().replaceAll("0x[0-9a-fA-F]+$", "0x<n>"))
                .orElse("none");
    }

    public static void main(String[] args) throws Throwable {
        boolean[] sv = new boolean[2];
        ScopedValue.where(KEY, "v").run(() -> {
            sv[0] = any(plain(), RUN_WITH_CARRIER);
            sv[1] = any(showing(), RUN_WITH_CARRIER);
        });
        System.out.println("scoped-value default: " + sv[0]);
        System.out.println("scoped-value SHOW_HIDDEN_FRAMES: " + sv[1]);

        byte[] b = ClassFile.of().build(ClassDesc.of("L4W35StackWalkerHidden$Hid"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("go", MethodTypeDesc.of(ConstantDescs.CD_void),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.invokestatic(ClassDesc.of("L4W35StackWalkerHidden"), "probeHidden",
                            MethodTypeDesc.of(ConstantDescs.CD_void)).return_());
        });
        Class<?> h = MethodHandles.lookup().defineHiddenClass(b, true).lookupClass();
        MethodHandles.lookup().findStatic(h, "go", MethodType.methodType(void.class)).invokeExact();
        System.out.println("hidden-class default: " + SEEN[0]);
        System.out.println("hidden-class SHOW_HIDDEN_FRAMES: " + SEEN[1]);
        System.out.println("hidden-class name: " + hiddenName);

        AtomicReference<boolean[]> vt = new AtomicReference<>();
        Thread t = Thread.ofVirtual().start(() -> vt.set(new boolean[] {
            any(plain(), RUN_WITH_THREAD), any(showing(), RUN_WITH_THREAD)}));
        t.join();
        System.out.println("virtual default: " + vt.get()[0]);
        System.out.println("virtual SHOW_HIDDEN_FRAMES: " + vt.get()[1]);
    }
}
