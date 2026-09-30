// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 46, lane L3 -- review: `java.lang.invoke` frames
// in stack walks and traces. A target reached through a lambda, a method
// reference, `MethodHandle.invokeExact` / `invoke` (direct, bound, adapted)
// and `Method.invoke` walks the stack with each `StackWalker` option set, asks
// `getCallerClass`, and throws; the rows print what each view lists.
//
// Frames print as `Class.method`, innermost first, from the target down to
// the probe's `Q.via*` caller (the probe's classes shortened to `P` / `Q`);
// a hidden class's `/0x<address>` suffix prints as `/0x`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// lambda-throw: P.lambda$main$0,Q.viaLambda
// lambda-thread-trace: java.lang.Thread.getStackTrace,P.lambda$main$1,Q.viaLambda
// lambda-walk-default: P.walk,P.lambda$main$2,Q.viaLambda
// lambda-walk-reflect: P.walk,P.lambda$main$3,Q.viaLambda
// lambda-caller-class: P
// methodref-walk-default: P.walk,P.target,Q.viaMethodRef
// methodref-throw: P.thrower,Q.viaMethodRef
// methodref-caller-class: Q
// mh-walk-default: P.walk,P.target,Q.viaMh
// mh-walk-reflect: P.walk,P.target,Q.viaMh
// mh-throw: P.thrower,Q.viaMh
// mh-thread-trace: java.lang.Thread.getStackTrace,P.threadTrace,Q.viaMh
// mh-caller-class: Q
// mh-bound-walk-default: P.walk,P.targetWith,Q.viaBoundMh
// mh-generic-walk-default: P.walk,P.target,Q.viaGenericMh
// mh-hidden-shows-invoke-frames: true
// reflect-walk-default: P.walk,P.target,Q.viaReflection
// reflect-caller-class: Q
// reflect-hidden-shows-invoke-frames: true
// HOTSPOT_EXPECTED_END
//
// A review probe (wave 46, lane L3): no code changed for it. Every row but
// the last two is expected to match in both modes: HotSpot hides the lambda
// proxy, the method-handle and the reflection frames from these views, and
// CratonVM runs none of them (its lambda dispatch runs the body as the
// callee of the `invokeinterface`; `invokeExact` / `invoke` and
// `Method.invoke` are natives), so `getCallerClass` and every walk agree.
// The two `*-hidden-shows-invoke-frames` rows ask whether a
// SHOW_HIDDEN_FRAMES walk lists a `java.lang.invoke` frame (a `LambdaForm$*`
// or `*$Holder` method-handle frame) between the target and its caller:
// HotSpot runs the call through its LambdaForms and lists them; CratonVM is
// expected to print `false` on both (read from the code: nothing it runs for
// the call is a `java.lang.invoke` frame; the host run decides). That is the
// hidden-frame display detail of the lambda proxy frame, retired by design in
// docs/internal/fixed-bugs/interpreter-L6-lambda-proxy-frames-do-not-exist-RETIRED-20261004.md;
// the direction that would list them is in
// docs/known-issues/interpreter/i46-L3-proposal-one-table-of-the-jdk-frames-a-native-stands-for-20261010.md.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W46InvokeFramesInWalks`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;

public class L3W46InvokeFramesInWalks {
    static final StackWalker DEFAULT = StackWalker.getInstance();
    static final StackWalker REFLECT =
            StackWalker.getInstance(StackWalker.Option.SHOW_REFLECT_FRAMES);
    static final StackWalker HIDDEN =
            StackWalker.getInstance(StackWalker.Option.SHOW_HIDDEN_FRAMES);
    static final StackWalker CLASSES =
            StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);

    static String shorten(String className) {
        return className
                .replace("L3W46InvokeFramesInWalks$Q", "Q")
                .replace("L3W46InvokeFramesInWalks", "P")
                .replaceAll("/0x[0-9a-f]+", "/0x");
    }

    static boolean isStop(String cls, String method) {
        return cls.equals("Q") && method.startsWith("via");
    }

    /** Frames from `from` down to the first `Q.via*`. */
    static String walk(StackWalker w) {
        return w.walk(s -> {
            List<String> out = new ArrayList<>();
            for (StackWalker.StackFrame f : (Iterable<StackWalker.StackFrame>) s::iterator) {
                String cls = shorten(f.getClassName());
                if (cls.equals("P") && f.getMethodName().equals("walkHere")) {
                    continue;
                }
                out.add(cls + "." + f.getMethodName());
                if (isStop(cls, f.getMethodName()) || out.size() > 12) {
                    break;
                }
            }
            return String.join(",", out);
        });
    }

    /** Does a SHOW_HIDDEN_FRAMES walk list a `java.lang.invoke` frame before `Q.via*`? */
    static boolean hiddenInvokeFrames() {
        return HIDDEN.walk(s -> {
            for (StackWalker.StackFrame f : (Iterable<StackWalker.StackFrame>) s::iterator) {
                String cls = shorten(f.getClassName());
                if (isStop(cls, f.getMethodName())) {
                    return false;
                }
                if (cls.startsWith("java.lang.invoke.")) {
                    return true;
                }
            }
            return false;
        });
    }

    static String frames(StackTraceElement[] trace) {
        List<String> out = new ArrayList<>();
        for (StackTraceElement e : trace) {
            String cls = shorten(e.getClassName());
            out.add(cls + "." + e.getMethodName());
            if (isStop(cls, e.getMethodName()) || out.size() > 12) {
                break;
            }
        }
        return String.join(",", out);
    }

    // What the target does, chosen per row.
    static int mode;
    static String result;

    static final int WALK_DEFAULT = 0;
    static final int WALK_REFLECT = 1;
    static final int CALLER_CLASS = 2;
    static final int HIDDEN_INVOKE = 3;

    static void walkHere() {
        switch (mode) {
            case WALK_DEFAULT -> result = walk(DEFAULT);
            case WALK_REFLECT -> result = walk(REFLECT);
            case CALLER_CLASS -> result = "unused";
            case HIDDEN_INVOKE -> result = String.valueOf(hiddenInvokeFrames());
            default -> throw new IllegalStateException();
        }
    }

    public static void target() {
        if (mode == CALLER_CLASS) {
            result = shorten(CLASSES.getCallerClass().getName());
        } else {
            walkHere();
        }
    }

    public static void targetWith(String unused) {
        walkHere();
    }

    public static void thrower() {
        throw new IllegalStateException("from the target");
    }

    public static void threadTrace() {
        result = frames(Thread.currentThread().getStackTrace());
    }

    static final class Q {
        static String viaLambda(Runnable r) {
            result = null;
            try {
                r.run();
            } catch (IllegalStateException e) {
                return frames(e.getStackTrace());
            }
            return result;
        }

        static String viaMethodRef(int m) throws Throwable {
            mode = m;
            result = null;
            Runnable r = m < 0 ? L3W46InvokeFramesInWalks::thrower : L3W46InvokeFramesInWalks::target;
            try {
                r.run();
            } catch (IllegalStateException e) {
                return frames(e.getStackTrace());
            }
            return result;
        }

        static String viaMh(MethodHandle mh, int m) throws Throwable {
            mode = m;
            result = null;
            try {
                mh.invokeExact();
            } catch (IllegalStateException e) {
                return frames(e.getStackTrace());
            }
            return result;
        }

        static String viaBoundMh(MethodHandle mh, int m) throws Throwable {
            mode = m;
            result = null;
            mh.invokeExact();
            return result;
        }

        static String viaGenericMh(MethodHandle mh, int m) throws Throwable {
            mode = m;
            result = null;
            Object ignored = mh.invoke();
            return result;
        }

        static String viaReflection(Method method, int m) throws Throwable {
            mode = m;
            result = null;
            method.invoke(null);
            return result;
        }
    }

    public static void main(String[] args) throws Throwable {
        System.out.println("lambda-throw: " + Q.viaLambda(() -> {
            throw new IllegalStateException("from the lambda");
        }));
        System.out.println("lambda-thread-trace: " + Q.viaLambda(() -> {
            result = frames(Thread.currentThread().getStackTrace());
        }));
        System.out.println("lambda-walk-default: " + Q.viaLambda(() -> {
            result = walk(DEFAULT);
        }));
        System.out.println("lambda-walk-reflect: " + Q.viaLambda(() -> {
            result = walk(REFLECT);
        }));
        mode = CALLER_CLASS;
        // The lambda body is `target`'s caller.
        System.out.println("lambda-caller-class: " + Q.viaLambda(() -> target()));

        System.out.println("methodref-walk-default: " + Q.viaMethodRef(WALK_DEFAULT));
        System.out.println("methodref-throw: " + Q.viaMethodRef(-1));
        System.out.println("methodref-caller-class: " + Q.viaMethodRef(CALLER_CLASS));

        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodType voidType = MethodType.methodType(void.class);
        MethodHandle target = lookup.findStatic(L3W46InvokeFramesInWalks.class, "target", voidType);
        MethodHandle thrower = lookup.findStatic(L3W46InvokeFramesInWalks.class, "thrower", voidType);
        MethodHandle threadTrace =
                lookup.findStatic(L3W46InvokeFramesInWalks.class, "threadTrace", voidType);
        MethodHandle bound = MethodHandles.insertArguments(
                lookup.findStatic(L3W46InvokeFramesInWalks.class, "targetWith",
                        MethodType.methodType(void.class, String.class)),
                0, "bound");
        System.out.println("mh-walk-default: " + Q.viaMh(target, WALK_DEFAULT));
        System.out.println("mh-walk-reflect: " + Q.viaMh(target, WALK_REFLECT));
        System.out.println("mh-throw: " + Q.viaMh(thrower, WALK_DEFAULT));
        Q.viaMh(threadTrace, WALK_DEFAULT);
        System.out.println("mh-thread-trace: " + result);
        System.out.println("mh-caller-class: " + Q.viaMh(target, CALLER_CLASS));
        System.out.println("mh-bound-walk-default: " + Q.viaBoundMh(bound, WALK_DEFAULT));
        System.out.println("mh-generic-walk-default: " + Q.viaGenericMh(target, WALK_DEFAULT));
        System.out.println("mh-hidden-shows-invoke-frames: " + Q.viaMh(target, HIDDEN_INVOKE));

        Method method = L3W46InvokeFramesInWalks.class.getMethod("target");
        System.out.println("reflect-walk-default: " + Q.viaReflection(method, WALK_DEFAULT));
        System.out.println("reflect-caller-class: " + Q.viaReflection(method, CALLER_CLASS));
        System.out.println("reflect-hidden-shows-invoke-frames: "
                + Q.viaReflection(method, HIDDEN_INVOKE));
    }
}
