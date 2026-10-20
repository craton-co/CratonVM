// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 92, lane L2 -- `-XX:+ShowHiddenFrames` lists the
// hidden frames this VM never pushes (page
// `i84-L2-show-hidden-frames-lists-no-frames-the-vm-never-pushed`). Wave 84
// kept every hidden frame that EXISTS (`L2W84ShowHiddenFrames`); HotSpot also
// lists a lambda proxy's SAM frame (`Outer$$Lambda/0x....run`) and the
// `LambdaForm` frames of a `MethodHandle.invokeExact`, which this VM serves
// without pushing them. The walkers' stand-ins (interpreter round i1 waves
// 48/50) now apply to the throwable capture, `Thread.getStackTrace()` and
// EVERY walk under the flag (HotSpot's `StackWalk::fill_in_frames` skips a
// hidden frame only `!ShowHiddenFrames`), while `getCallerClass` keeps
// skipping them by their member bit (`CallerClassFinder.consumeFrames` tests
// `isHidden()` in Java) and answers as without the flag.
//
//   lambda-throwable     `$$Lambda` frames in a Throwable raised in a lambda body
//   lambda-thread        the same in Thread.currentThread().getStackTrace()
//   lambda-walker        the same in an option-less StackWalker walk
//   lambda-walker-option the same with Option.SHOW_HIDDEN_FRAMES (both blocks 1)
//   mh-throwable-forms   does a Throwable raised in a method called through
//                        MethodHandle.invokeExact list a LambdaForm / Holder frame?
//   caller-lambda        StackWalker.getCallerClass() asked by a method a method
//                        reference's proxy called (its hidden frame is skipped)
//   caller-mh            the same, the asking method called through invokeExact
//
// Run (stdout must be the block for the run, byte for byte; each also with
// --compatible and with --nojit):
//   $J/javac -d out tools/probes/interp/L2/L2W92ShowHiddenFrames.java
//   $B --java-home $J/.. -cp out L2W92ShowHiddenFrames                     -> DEFAULT
//   $B -XX:+UnlockDiagnosticVMOptions -XX:+ShowHiddenFrames --java-home $J/.. -cp out L2W92ShowHiddenFrames
//                                                                          -> SHOW
//   $J/java -cp out L2W92ShowHiddenFrames                                  -> DEFAULT
//   $J/java -XX:+UnlockDiagnosticVMOptions -XX:+ShowHiddenFrames -cp out L2W92ShowHiddenFrames
//                                                                          -> SHOW
//
// Derived from the JDK 25 sources (`java_lang_Throwable::fill_in_stack_trace`
// `skip_hidden = !ShowHiddenFrames`; `StackWalk::fill_in_frames`;
// `StackStreamFactory.CallerClassFinder.consumeFrames`; `Thread.getStackTrace`
// is `new Exception().getStackTrace()` for the current thread). Not measured
// on a HotSpot build at the time of writing; `mh-throwable-forms` in SHOW
// depends on the LambdaForm shapes HotSpot picks (it lists at least one
// `java.lang.invoke.LambdaForm$...` or `...$Holder` frame for a static
// `invokeExact`) -- refresh from $J/java before reading a DIFF.
//
// HOTSPOT_EXPECTED_BEGIN DEFAULT (JDK 25, default flags)
// lambda-throwable 0
// lambda-thread 0
// lambda-walker 0
// lambda-walker-option 1
// mh-throwable-forms false
// caller-lambda L2W92ShowHiddenFrames
// caller-mh L2W92ShowHiddenFrames
// HOTSPOT_EXPECTED_END
//
// HOTSPOT_EXPECTED_BEGIN SHOW (-XX:+UnlockDiagnosticVMOptions -XX:+ShowHiddenFrames)
// lambda-throwable 1
// lambda-thread 1
// lambda-walker 1
// lambda-walker-option 1
// mh-throwable-forms true
// caller-lambda L2W92ShowHiddenFrames
// caller-mh L2W92ShowHiddenFrames
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.Supplier;

public class L2W92ShowHiddenFrames {
    static final StackWalker PLAIN = StackWalker.getInstance();
    static final StackWalker SHOWING = StackWalker.getInstance(StackWalker.Option.SHOW_HIDDEN_FRAMES);
    static final StackWalker RETAINING =
            StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);

    static int lambdaFrames(StackTraceElement[] trace) {
        int n = 0;
        for (StackTraceElement e : trace) {
            if (e.getClassName().contains("$$Lambda")) {
                n++;
            }
        }
        return n;
    }

    static long walkerLambdaFrames(StackWalker walker) {
        return walker.walk(s -> s.filter(f -> f.getClassName().contains("$$Lambda")).count());
    }

    static boolean hasFormFrame(StackTraceElement[] trace) {
        for (StackTraceElement e : trace) {
            String c = e.getClassName();
            if (c.startsWith("java.lang.invoke.LambdaForm$") || c.endsWith("$Holder")) {
                return true;
            }
        }
        return false;
    }

    static StackTraceElement[] raised() {
        return new Throwable().getStackTrace();
    }

    static String whoCalls() {
        return RETAINING.getCallerClass().getName();
    }

    public static void main(String[] args) throws Throwable {
        Supplier<Integer> t = () -> lambdaFrames(new Throwable().getStackTrace());
        System.out.println("lambda-throwable " + t.get());
        Supplier<Integer> th = () -> lambdaFrames(Thread.currentThread().getStackTrace());
        System.out.println("lambda-thread " + th.get());
        Supplier<Long> w = () -> walkerLambdaFrames(PLAIN);
        System.out.println("lambda-walker " + w.get());
        Supplier<Long> wo = () -> walkerLambdaFrames(SHOWING);
        System.out.println("lambda-walker-option " + wo.get());

        MethodHandle mh = MethodHandles.lookup()
                .findStatic(L2W92ShowHiddenFrames.class, "raised",
                        MethodType.methodType(StackTraceElement[].class));
        StackTraceElement[] viaMh = (StackTraceElement[]) mh.invokeExact();
        System.out.println("mh-throwable-forms " + hasFormFrame(viaMh));

        // A method reference: the proxy's `get` calls `whoCalls` directly, so
        // the frame after the asking one is the (hidden) proxy's.
        Supplier<String> c = L2W92ShowHiddenFrames::whoCalls;
        System.out.println("caller-lambda " + c.get());
        MethodHandle who = MethodHandles.lookup()
                .findStatic(L2W92ShowHiddenFrames.class, "whoCalls", MethodType.methodType(String.class));
        System.out.println("caller-mh " + (String) who.invokeExact());
    }
}
