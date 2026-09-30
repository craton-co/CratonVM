// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 43, lane L3 -- the JDK frames of a reflective
// call (`Method.invoke`, `Constructor.newInstance`) in every view of the stack:
// a `Throwable`'s trace, `Thread.getStackTrace()` and `StackWalker` with and
// without `SHOW_REFLECT_FRAMES`. HotSpot lists `Method.invoke` and
// `DirectMethodHandleAccessor.invoke` (three frames for a constructor) between
// the caller and the target in the first three views, and hides them from a
// default walker.
// docs/internal/fixed-bugs/interpreter-L3-a-reflective-call-leaves-no-method-invoke-frame-FIXED-20261007.md
//
// Rows print frames as `Class.method`, the probe's own class shortened to `P`,
// innermost first, from the target down to `main`.
//
// HotSpot 25 prints (no agent; the same with -Xint; measured, JDK 25.0.3):
//     throw-through-invoke: P.thrower,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.lambda$main$0,P.row,P.main
//     throw-through-invoke-text: java.base/jdk.internal.reflect.DirectMethodHandleAccessor.invoke(DirectMethodHandleAccessor.java:104) | java.base/java.lang.reflect.Method.invoke(Method.java:565)
//     throwable-in-target: P.target,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.lambda$main$2,P.row,P.main
//     thread-trace-in-target: P.target,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.lambda$main$3,P.row,P.main
//     walk-default: P.target,P.lambda$main$4,P.row,P.main
//     walk-reflect: P.target,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.lambda$main$5,P.row,P.main
//     walk-reflect-forEach: P.target,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.lambda$main$6,P.row,P.main
//     nested-invoke: P.target,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.outer,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.lambda$main$7,P.row,P.main
//     ctor-throwable: P$Made.<init>,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance,java.lang.reflect.Constructor.newInstanceWithCaller,java.lang.reflect.Constructor.newInstance,P.lambda$main$8,P.row,P.main
//     ctor-walk-default: P$Made.<init>,P.lambda$main$9,P.row,P.main
//     virtual-target: P$Inst.run,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.lambda$main$10,P.row,P.main
//     invocation-target-exception-top: jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.lambda$main$11
//
// CratonVM before wave 43 (read from the code: `Method.invoke` and
// `Constructor.newInstance` are natives in both modes and push no frame) lists
// no reflection frame in any row, e.g.
//     throw-through-invoke: P.thrower,P.lambda$main$0,P.row,P.main
// and `throw-through-invoke-text` prints `none`. Since wave 43 every row but
// the last prints HotSpot's line; `invocation-target-exception-top` still
// lists the caller first (the exception is raised by the native itself, and
// no frame of the target is above the call:
// docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md).
// Since wave 45 that row prints HotSpot's line too: the call's frames go on
// top of a throwable its native raised (`L3W45ReflectiveRaise`).
// Until wave 44 a row run with the JIT on could differ where a compiled
// activation entered at the call's depth left the call's frames out; since
// wave 44 the frames go right below it (`L3W44ReflectionCompiledTarget`).
//
// Positive control: `CRATONVM_DBG_STTRACE=1` prints (wave 44's form)
// `STTRACE_DBG_REFLECT depth=<d> jit_depth=<j> position=Some(<p>) compiled_after=<bool>`
// per listed call (the base prints no such line).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W43ReflectionFrames`.
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Supplier;
import java.util.stream.Collectors;

public class L3W43ReflectionFrames {
    interface Call {
        Object run() throws Throwable;
    }

    static void row(String name, Call call) {
        String out;
        try {
            Object r = call.run();
            out = String.valueOf(r);
        } catch (Throwable t) {
            out = "threw " + t;
        }
        System.out.println(name + ": " + out);
    }

    static String shorten(String className) {
        return className.replace("L3W43ReflectionFrames", "P");
    }

    /** Frames from the first `from`-named one down to `main`, joined. */
    static String names(StackTraceElement[] trace, String from) {
        List<String> out = new ArrayList<>();
        boolean on = false;
        for (StackTraceElement e : trace) {
            String n = shorten(e.getClassName()) + "." + e.getMethodName();
            if (!on && n.equals(from)) {
                on = true;
            }
            if (on) {
                out.add(n);
                if (n.equals("P.main")) {
                    break;
                }
            }
        }
        return on ? String.join(",", out) : "missing " + from;
    }

    static volatile String captured;

    public static void thrower() {
        throw new IllegalStateException("from the target");
    }

    public static void target(String mode) {
        switch (mode) {
            case "throwable" -> captured = names(new Throwable().getStackTrace(), "P.target");
            case "thread" -> captured = names(Thread.currentThread().getStackTrace(), "P.target");
            case "walk-default" -> captured = StackWalker.getInstance().walk(s -> s
                    .map(f -> shorten(f.getClassName()) + "." + f.getMethodName())
                    .collect(Collectors.joining(",")));
            case "walk-reflect" -> captured = StackWalker
                    .getInstance(StackWalker.Option.SHOW_REFLECT_FRAMES).walk(s -> s
                    .map(f -> shorten(f.getClassName()) + "." + f.getMethodName())
                    .collect(Collectors.joining(",")));
            case "walk-reflect-forEach" -> {
                List<String> seen = new ArrayList<>();
                StackWalker.getInstance(StackWalker.Option.SHOW_REFLECT_FRAMES)
                        .forEach(f -> seen.add(shorten(f.getClassName()) + "." + f.getMethodName()));
                captured = String.join(",", seen);
            }
            default -> captured = "unknown mode " + mode;
        }
    }

    public static void outer() throws Exception {
        TARGET.invoke(null, "throwable");
    }

    public static final class Made {
        public Made(String mode) {
            if (mode.equals("throwable")) {
                captured = names(new Throwable().getStackTrace(), "P$Made.<init>");
            } else {
                captured = StackWalker.getInstance().walk(s -> s
                        .map(f -> shorten(f.getClassName()) + "." + f.getMethodName())
                        .collect(Collectors.joining(",")));
            }
        }
    }

    public static final class Inst {
        public void run() {
            captured = names(new Throwable().getStackTrace(), "P$Inst.run");
        }
    }

    static Method TARGET;

    static String trimWalk(String walk) {
        int at = walk.indexOf("P.target");
        if (at < 0) {
            at = walk.indexOf("P$Made.<init>");
        }
        return at < 0 ? "missing target in " + walk : walk.substring(at);
    }

    public static void main(String[] args) throws Exception {
        TARGET = L3W43ReflectionFrames.class.getMethod("target", String.class);
        Method thrower = L3W43ReflectionFrames.class.getMethod("thrower");
        Method outer = L3W43ReflectionFrames.class.getMethod("outer");
        Constructor<Made> made = Made.class.getConstructor(String.class);
        Method run = Inst.class.getMethod("run");

        row("throw-through-invoke", () -> {
            try {
                thrower.invoke(null);
                return "no exception";
            } catch (InvocationTargetException e) {
                return names(e.getCause().getStackTrace(), "P.thrower");
            }
        });
        row("throw-through-invoke-text", () -> {
            try {
                thrower.invoke(null);
                return "no exception";
            } catch (InvocationTargetException e) {
                List<String> out = new ArrayList<>();
                for (StackTraceElement s : e.getCause().getStackTrace()) {
                    if (s.getClassName().startsWith("java.lang.reflect.")
                            || s.getClassName().startsWith("jdk.internal.reflect.")) {
                        out.add(s.toString());
                    }
                }
                return out.isEmpty() ? "none" : String.join(" | ", out);
            }
        });
        row("throwable-in-target", () -> {
            TARGET.invoke(null, "throwable");
            return captured;
        });
        row("thread-trace-in-target", () -> {
            TARGET.invoke(null, "thread");
            return captured;
        });
        row("walk-default", () -> {
            TARGET.invoke(null, "walk-default");
            return trimWalk(captured);
        });
        row("walk-reflect", () -> {
            TARGET.invoke(null, "walk-reflect");
            return trimWalk(captured);
        });
        row("walk-reflect-forEach", () -> {
            TARGET.invoke(null, "walk-reflect-forEach");
            return trimWalk(captured);
        });
        row("nested-invoke", () -> {
            outer.invoke(null);
            return captured;
        });
        row("ctor-throwable", () -> {
            made.newInstance("throwable");
            return captured;
        });
        row("ctor-walk-default", () -> {
            made.newInstance("walk");
            return trimWalk(captured);
        });
        row("virtual-target", () -> {
            run.invoke(new Inst());
            return captured;
        });
        row("invocation-target-exception-top", () -> {
            try {
                thrower.invoke(null);
                return "no exception";
            } catch (InvocationTargetException e) {
                StackTraceElement[] t = e.getStackTrace();
                List<String> out = new ArrayList<>();
                for (int i = 0; i < Math.min(3, t.length); i++) {
                    out.add(shorten(t[i].getClassName()) + "." + t[i].getMethodName());
                }
                return String.join(",", out);
            }
        });
    }
}
