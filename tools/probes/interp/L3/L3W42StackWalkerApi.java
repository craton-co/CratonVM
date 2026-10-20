// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L3 (review of the stack walks): the
// `java.lang.StackWalker` API's refusals and frame accessors, the reflection
// frames it hides and shows, and a lambda body's walk -- one row per case.
// A review probe: no CratonVM change was made for it, and its CratonVM
// output was not predicted from the code (the walker's frames come from
// `native-builtins/src/lang_stackwalker.rs` and `vm/src/runtime/stackwalker.rs`,
// the checks from the JDK's own `StackWalker` / `StackFrameInfo` Java).
// A `SHOW_HIDDEN_FRAMES` walk from a lambda body is left out on purpose: the
// proxy frame is absent by design
// (`docs/internal/fixed-bugs/interpreter-L6-lambda-proxy-frames-do-not-exist-RETIRED-20261004.md`).
//
// HotSpot 25 prints (no agent; the same with -Xint; measured, JDK 25.0.3):
//     declaring-class-without-option: java.lang.UnsupportedOperationException: No access to RETAIN_CLASS_REFERENCE
//     caller-class-without-option: java.lang.UnsupportedOperationException: This stack walker does not have RETAIN_CLASS_REFERENCE access
//     caller-of-lambda-body: returned L3W42StackWalkerApi
//     caller-from-static: returned L3W42StackWalkerApi
//     estimate-depth-zero: java.lang.IllegalArgumentException: estimateDepth must be > 0
//     null-option: java.lang.NullPointerException: null
//     walk-null: java.lang.NullPointerException: null
//     forEach-null: java.lang.NullPointerException: null
//     escaped-stream: java.lang.IllegalStateException: This stack stream is not valid for walking.
//     frame-method-type: returned (StackWalker)Object
//     frame-method-type-no-option: java.lang.UnsupportedOperationException: No access to RETAIN_CLASS_REFERENCE
//     frame-descriptor-no-option: java.lang.UnsupportedOperationException: No access to RETAIN_CLASS_REFERENCE
//     frame-native: returned false
//     frame-ste: returned lambda$main$19 file=L3W42StackWalkerApi.java line>0=true
//     reflect-default: returned P.names,P.viaReflection,P.lambda$main$21,P.row
//     reflect-show: returned P.names,P.viaReflection,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke
//     reflect-caller: returned L3W42StackWalkerApi
//     lambda-default: returned P.names,P.lambda$main$24,P.lambda$main$25
//     skip-limit: returned [row, main]
//     options-set: returned L3W42StackWalkerApi
//     thread-trace-top: returned java.lang.Thread.getStackTrace lambda$main$30
// (`frame-method-type` names the frame that called `walk`: the row's lambda,
// a static `lambda$main$N(StackWalker)Object` capturing the walker.)
//
// CratonVM, host run of wave 42 (default and --nojit) differed in seven rows:
// `caller-class-without-option` returned the class, `null-option` had the
// message "StackWalker.getInstance: option must not be null", `walk-null` and
// `forEach-null` returned null, `escaped-stream` returned 1,
// `frame-descriptor-no-option` returned the descriptor, and `reflect-show`
// showed no reflection frame. The wave-42 follow-up (lane L3) makes the
// natives serving `StackWalker` (`stack_walker.rs`, `phases_late/reflect_invoke.rs`,
// `lang_stackwalker.rs`) make the JDK's checks, so the first five of those
// print HotSpot's lines; `escaped-stream` and `reflect-show` differed until
// wave 43 (lane L3), which fixed both
// (docs/internal/fixed-bugs/interpreter-L3-a-stackwalker-stream-stays-usable-after-walk-returns-FIXED-20261007.md,
// docs/internal/fixed-bugs/interpreter-L3-a-reflective-call-leaves-no-method-invoke-frame-FIXED-20261007.md).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W42StackWalkerApi`.
import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.lang.reflect.Method;
import java.util.Set;
import java.util.function.Supplier;
import java.util.stream.Collectors;
import java.util.stream.Stream;

public class L3W42StackWalkerApi {
    interface Call {
        Object run() throws Throwable;
    }

    static void row(String name, Call call) {
        String out;
        try {
            Object r = call.run();
            out = "returned " + r;
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static String names(StackWalker w, int limit) {
        return w.walk(s -> s.limit(limit)
                .map(f -> f.getClassName().replace("L3W42StackWalkerApi", "P") + "." + f.getMethodName())
                .collect(Collectors.joining(",")));
    }

    public static String viaReflection(StackWalker w) {
        return names(w, 4);
    }

    public static Class<?> callerOfReflected() {
        return StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).getCallerClass();
    }

    static Class<?> caller() {
        return StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).getCallerClass();
    }

    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Throwable {
        StackWalker plain = StackWalker.getInstance();
        StackWalker retain = StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE);
        row("declaring-class-without-option", () -> plain.walk(s -> s.findFirst().get().getDeclaringClass()));
        row("caller-class-without-option", () -> plain.getCallerClass());
        row("caller-of-lambda-body", () -> retain.getCallerClass().getName());
        row("caller-from-static", () -> caller().getSimpleName());
        row("estimate-depth-zero", () -> StackWalker.getInstance(Set.of(), 0));
        row("null-option", () -> StackWalker.getInstance((Option) null));
        row("walk-null", () -> plain.walk(null));
        row("forEach-null", () -> {
            plain.forEach(null);
            return null;
        });
        Stream<StackFrame>[] escaped = new Stream[1];
        plain.walk(s -> {
            escaped[0] = s;
            return null;
        });
        row("escaped-stream", () -> escaped[0].count());
        row("frame-method-type", () -> retain.walk(s -> s.findFirst().get().getMethodType()));
        row("frame-method-type-no-option", () -> plain.walk(s -> s.findFirst().get().getMethodType()));
        row("frame-descriptor-no-option", () -> plain.walk(s -> s.findFirst().get().getDescriptor()));
        row("frame-native", () -> plain.walk(s -> s.findFirst().get().isNativeMethod()));
        row("frame-ste", () -> plain.walk(s -> {
            StackTraceElement e = s.findFirst().get().toStackTraceElement();
            return e.getMethodName() + " file=" + e.getFileName() + " line>0=" + (e.getLineNumber() > 0);
        }));
        Method m = L3W42StackWalkerApi.class.getMethod("viaReflection", StackWalker.class);
        row("reflect-default", () -> m.invoke(null, plain));
        row("reflect-show", () -> m.invoke(null, StackWalker.getInstance(Option.SHOW_REFLECT_FRAMES)));
        Method c = L3W42StackWalkerApi.class.getMethod("callerOfReflected");
        row("reflect-caller", () -> ((Class<?>) c.invoke(null)).getName());
        Supplier<String> lam = () -> names(plain, 3);
        row("lambda-default", () -> lam.get());
        row("skip-limit", () -> plain.walk(s -> s.skip(1).limit(2)
                .map(StackFrame::getMethodName).collect(Collectors.toList())));
        row("options-set", () -> StackWalker
                .getInstance(Set.of(Option.RETAIN_CLASS_REFERENCE, Option.SHOW_HIDDEN_FRAMES))
                .walk(s -> s.findFirst().get().getDeclaringClass().getSimpleName()));
        row("thread-trace-top", () -> {
            StackTraceElement[] st = Thread.currentThread().getStackTrace();
            return st[0].getClassName() + "." + st[0].getMethodName() + " " + st[1].getMethodName();
        });
    }
}
