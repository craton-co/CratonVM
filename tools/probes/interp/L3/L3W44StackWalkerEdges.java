// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 44, lane L3 -- review of StackWalker / stack
// trace edges against HotSpot: the frame accessors without and with
// RETAIN_CLASS_REFERENCE, getCallerClass at the bottom and through
// reflection, walks that skip and limit, a trace of an unstarted and a
// finished thread, and a Throwable that does not write its trace.
//
// HotSpot 25 prints (measured, JDK 25.0.3, Windows box; the same with -Xint):
//     declaring-class-plain: java.lang.UnsupportedOperationException: No access to RETAIN_CLASS_REFERENCE
//     method-type-plain: java.lang.UnsupportedOperationException: No access to RETAIN_CLASS_REFERENCE
//     descriptor-plain: java.lang.UnsupportedOperationException: No access to RETAIN_CLASS_REFERENCE
//     declaring-class-retain: returned L3W44StackWalkerEdges
//     method-type-retain: returned (StackWalker)StackFrame
//     descriptor-retain: returned (Ljava/lang/StackWalker;)Ljava/lang/StackWalker$StackFrame;
//     bci-nonnegative: returned true
//     to-element-class: returned L3W44StackWalkerEdges
//     to-element-method: returned top
//     is-native: returned false
//     caller-plain: java.lang.UnsupportedOperationException: This stack walker does not have RETAIN_CLASS_REFERENCE access
//     caller-in-main: returned L3W44StackWalkerEdges
//     caller-through-invoke: returned L3W44StackWalkerEdges
//     caller-direct: returned L3W44StackWalkerEdges
//     skip-limit: returned [row, main]
//     walk-null: java.lang.NullPointerException: null
//     forEach-count: returned true
//     instance-empty-set: returned true
//     instance-null-option: java.lang.NullPointerException: null
//     instance-null-set: java.lang.NullPointerException: Cannot invoke "java.util.Set.isEmpty()" because "options" is null
//     instance-null-set-depth: java.lang.NullPointerException: null
//     instance-bad-depth: java.lang.IllegalArgumentException: estimateDepth must be > 0
//     unstarted-trace: returned 0
//     finished-trace: returned 0
//     not-writable: returned 0
//     not-writable-fill: returned 0
//     set-trace-null-element: java.lang.NullPointerException: stackTrace[0]
//     set-trace-null: java.lang.NullPointerException: Cannot invoke "[Ljava.lang.StackTraceElement;.clone()" because "stackTrace" is null
//     set-trace-then-read: returned [A.b(C.java:7)]
//     current-thread-top: returned getStackTrace
//     current-thread-second: returned lambda$main$35
//
// CratonVM base `5248262b7` (read from the code, not run): `instance-null-set`
// printed `java.lang.NullPointerException: StackWalker.getInstance: options Set
// must not be null` and `instance-null-set-depth` the same message (the
// natives in `native-builtins/src/stack_walker.rs`); fixed in wave 44 to
// HotSpot's (the helpful message of the JDK body's `options.isEmpty()`, and
// `Objects.requireNonNull`'s none). The other rows were not predicted from
// the code; wave 42's `L3W42StackWalkerApi` covers the accessor refusals.
// `current-thread-second` names the row's lambda, whose number javac assigns
// (the same class file on both VMs).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W44StackWalkerEdges`.
import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.lang.reflect.Method;
import java.util.EnumSet;
import java.util.List;
import java.util.Set;
import java.util.stream.Collectors;

public class L3W44StackWalkerEdges {
    interface Call {
        Object run() throws Throwable;
    }

    static void row(String name, Call call) {
        String out;
        try {
            out = "returned " + call.run();
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static StackFrame top(StackWalker w) {
        return w.walk(s -> s.findFirst().orElseThrow());
    }

    public static Class<?> callerOf() {
        return StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).getCallerClass();
    }

    public static void main(String[] args) throws Exception {
        StackWalker plain = StackWalker.getInstance();
        StackWalker retain = StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE);
        row("declaring-class-plain", () -> top(plain).getDeclaringClass());
        row("method-type-plain", () -> top(plain).getMethodType());
        row("descriptor-plain", () -> top(plain).getDescriptor());
        row("declaring-class-retain", () -> top(retain).getDeclaringClass().getSimpleName());
        row("method-type-retain", () -> top(retain).getMethodType());
        row("descriptor-retain", () -> top(retain).getDescriptor());
        row("bci-nonnegative", () -> top(plain).getByteCodeIndex() >= 0);
        row("to-element-class", () -> top(plain).toStackTraceElement().getClassName());
        row("to-element-method", () -> top(plain).toStackTraceElement().getMethodName());
        row("is-native", () -> top(plain).isNativeMethod());
        row("caller-plain", () -> plain.getCallerClass());
        row("caller-in-main", () -> retain.getCallerClass().getSimpleName());
        row("caller-through-invoke", () -> {
            Method m = L3W44StackWalkerEdges.class.getMethod("callerOf");
            return ((Class<?>) m.invoke(null)).getSimpleName();
        });
        row("caller-direct", () -> callerOf().getSimpleName());
        row("skip-limit", () -> plain.walk(s -> s.skip(1).limit(2)
                .map(StackFrame::getMethodName).collect(Collectors.toList())));
        row("walk-null", () -> plain.walk(null));
        row("forEach-count", () -> {
            int[] n = {0};
            plain.forEach(f -> n[0]++);
            return n[0] > 0;
        });
        row("instance-empty-set", () -> StackWalker.getInstance(Set.of()).walk(s -> s.count() > 0));
        row("instance-null-option", () -> StackWalker.getInstance((Option) null));
        row("instance-null-set", () -> StackWalker.getInstance((Set<Option>) null));
        row("instance-null-set-depth", () -> StackWalker.getInstance((Set<Option>) null, 4));
        row("instance-bad-depth", () -> StackWalker.getInstance(EnumSet.noneOf(Option.class), 0));
        row("unstarted-trace", () -> new Thread(() -> { }).getStackTrace().length);
        row("finished-trace", () -> {
            Thread t = new Thread(() -> { });
            t.start();
            t.join();
            return t.getStackTrace().length;
        });
        row("not-writable", () -> new Throwable("m", null, true, false) { }.getStackTrace().length);
        row("not-writable-fill", () -> {
            Throwable t = new Throwable("m", null, true, false) { };
            t.fillInStackTrace();
            return t.getStackTrace().length;
        });
        row("set-trace-null-element", () -> {
            Throwable t = new Throwable();
            t.setStackTrace(new StackTraceElement[] {null});
            return "set";
        });
        row("set-trace-null", () -> {
            new Throwable().setStackTrace(null);
            return "set";
        });
        row("set-trace-then-read", () -> {
            Throwable t = new Throwable();
            t.setStackTrace(new StackTraceElement[] {new StackTraceElement("A", "b", "C.java", 7)});
            return List.of(t.getStackTrace()).toString();
        });
        row("current-thread-top", () -> Thread.currentThread().getStackTrace()[0].getMethodName());
        row("current-thread-second", () -> Thread.currentThread().getStackTrace()[1].getMethodName());
    }
}
