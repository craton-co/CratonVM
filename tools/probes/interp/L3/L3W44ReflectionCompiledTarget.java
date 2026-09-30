// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 44, lane L3 -- the JDK frames of a reflective
// call whose target runs COMPILED, called from a compiled caller: the capture
// must place `Method.invoke` / `DirectMethodHandleAccessor.invoke` between
// the compiled caller (its JIT chain entry pushed before the call) and the
// compiled target (its entry pushed after it, at the same interpreter depth).
// docs/internal/fixed-bugs/interpreter-L3-proposal-compiled-activations-name-their-jit-entry-FIXED-20261008.md
// docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md (item 2)
//
// `callVia` and `hot` are warmed (40,000 reflective calls, and direct calls
// of `hot`) before each captured call, so a JIT has compiled both. Rows print
// frames as `Class.method`, the probe's own class shortened to `P`, innermost
// first, from the target down to `main`.
//
// HotSpot 25 prints (JIT and -Xint alike; measured, JDK 25.0.3, Windows box):
//     compiled-throwable: P.hot,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.callVia,P.row,P.main
//     compiled-thread-trace: P.hot,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.callVia,P.row,P.main
//     compiled-walk-reflect: P.hot,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.callVia,P.row,P.main
//     compiled-walk-default: P.hot,P.callVia,P.row,P.main
//     compiled-throw-through-invoke: P.hotThrower,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.callThrower,P.main
//
// CratonVM, `--nojit`: HotSpot's lines since wave 43 (an interpreter frame of
// the target is above every call). CratonVM with the JIT, base `5248262b7`
// (read from the code): when the reflective native entered `hot`'s compiled
// body, its chain entry is at the call's interpreter depth after the call,
// `jit_chain_entered_at_depth_since` answers true, and the call's frames are
// left out: `compiled-throwable: P.hot,P.callVia,P.row,P.main` (and the same for
// the thread trace, the reflect walk and the throw-through row). Since wave
// 44 every row prints HotSpot's line.
//
// Positive control: `CRATONVM_DBG_STTRACE=1` prints, per listed call,
// `STTRACE_DBG_REFLECT depth=<d> jit_depth=<j> position=Some(<p>) compiled_after=true`
// when the frames went right below a compiled target (the new placement
// engaged). The base prints `... compiled_since=true` for the same call and
// lists nothing. `compiled_after=false` on every line means the target ran
// interpreted there, and the probe then proves nothing about the placement
// (`CRATONVM_DBG_JITC=1` shows whether `hot` compiled).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W44ReflectionCompiledTarget`.
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.stream.Collectors;

public class L3W44ReflectionCompiledTarget {
    static String shorten(String className) {
        return className.replace("L3W44ReflectionCompiledTarget", "P");
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

    static String fromTarget(String walk) {
        int at = walk.indexOf("P.hot");
        return at < 0 ? "missing target in " + walk : walk.substring(at);
    }

    static volatile String captured;
    static long sink;

    public static long hot(int mode) {
        if (mode == 0) {
            return sink + 1;
        }
        switch (mode) {
            case 1 -> captured = names(new Throwable().getStackTrace(), "P.hot");
            case 2 -> captured = names(Thread.currentThread().getStackTrace(), "P.hot");
            case 3 -> captured = StackWalker.getInstance(StackWalker.Option.SHOW_REFLECT_FRAMES)
                    .walk(s -> s.map(f -> shorten(f.getClassName()) + "." + f.getMethodName())
                            .collect(Collectors.joining(",")));
            case 4 -> captured = StackWalker.getInstance()
                    .walk(s -> s.map(f -> shorten(f.getClassName()) + "." + f.getMethodName())
                            .collect(Collectors.joining(",")));
            default -> captured = "unknown mode " + mode;
        }
        return 0;
    }

    public static long hotThrower(boolean doThrow) {
        if (doThrow) {
            throw new IllegalStateException("from the compiled target");
        }
        return sink + 2;
    }

    static Method HOT;
    static Method THROWER;

    static long callVia(int mode) throws Exception {
        return (Long) HOT.invoke(null, mode);
    }

    static long callThrower(boolean doThrow) throws Exception {
        return (Long) THROWER.invoke(null, doThrow);
    }

    static void warm() throws Exception {
        for (int i = 0; i < 40_000; i++) {
            sink += callVia(0);
            sink += hot(0);
            sink += callThrower(false);
            sink += hotThrower(false);
        }
    }

    static void row(String name, int mode) throws Exception {
        warm();
        captured = null;
        callVia(mode);
        String out = mode == 3 || mode == 4 ? fromTarget(captured) : captured;
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Exception {
        HOT = L3W44ReflectionCompiledTarget.class.getMethod("hot", int.class);
        THROWER = L3W44ReflectionCompiledTarget.class.getMethod("hotThrower", boolean.class);
        row("compiled-throwable", 1);
        row("compiled-thread-trace", 2);
        row("compiled-walk-reflect", 3);
        row("compiled-walk-default", 4);
        warm();
        String out;
        try {
            callThrower(true);
            out = "no exception";
        } catch (InvocationTargetException e) {
            out = names(e.getCause().getStackTrace(), "P.hotThrower");
        }
        System.out.println("compiled-throw-through-invoke: " + out);
    }
}
