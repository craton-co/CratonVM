// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: the stack-walk switches of lane walk
// (`CRATONVM_STACKWALK_DEEP_WALK_LINES`, `_DEFERRED_LINES`,
// `_PIPELINE_LAYOUT_ONCE`, `_OPTION_ORDINALS_ONCE`, `_PIPELINE_INVOKE_BY_ID`,
// `_METHOD_SLOT_FRONT_CACHE`, and the default-ON `_REFLECTION_SCREEN_EXACT`).
//
// Risk targeted: line numbers and frame lists of COMPILED frames (inlined /
// spliced callees, deep recursion past the lean-build depth), answered from
// deferred or cached data, must equal HotSpot's; the reflection screen must
// drop exactly `Method` / `Constructor` / `MethodAccessor` /
// `ConstructorAccessor` frames (and keep them with SHOW_REFLECT_FRAMES). Rows:
//   * a walk at depth 5, 60 and 400 in a hot recursive method (called 50k
//     times first), listing `class.method:line` of the probe's own frames;
//   * the same with RETAIN_CLASS_REFERENCE (declaring class names) and a
//     `limit(k)` / `skip(k)` pipeline, `getCallerClass()`;
//   * a walk inside a method called through `Method.invoke` and through
//     `Constructor.newInstance`, with and without SHOW_REFLECT_FRAMES
//     (frames outside the probe are printed only for jdk.internal.reflect
//     and java.lang.reflect classes, which the screen decides);
//   * a walk inside a `<clinit>` triggered reflectively (`Class.forName`
//     through `Method.invoke`, the caller-sensitive adapter shape);
//   * Throwable.getStackTrace() of the same frames (the line table path).
//
// Run:
//   javac -g -d out R21FzWalk1.java
//   java -Xmx256m -cp out R21FzWalk1                                      # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzWalk1
//   CRATONVM_STACKWALK_DEEP_WALK_LINES=1 CRATONVM_STACKWALK_DEFERRED_LINES=1 CRATONVM_STACKWALK_PIPELINE_LAYOUT_ONCE=1
//     CRATONVM_STACKWALK_OPTION_ORDINALS_ONCE=1 CRATONVM_STACKWALK_PIPELINE_INVOKE_BY_ID=1
//     CRATONVM_STACKWALK_METHOD_SLOT_FRONT_CACHE=1 cratonvm ... (all opt-ins; then each alone if a row differs)
//   CRATONVM_STACKWALK_REFLECTION_SCREEN_EXACT=0 cratonvm ... (kill control)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.util.List;
import java.util.Set;
import java.util.stream.Collectors;

public class R21FzWalk1 {
    static final StackWalker PLAIN = StackWalker.getInstance();
    static final StackWalker RETAIN = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);
    static final StackWalker REFLECT = StackWalker.getInstance(
            Set.of(StackWalker.Option.SHOW_REFLECT_FRAMES, StackWalker.Option.RETAIN_CLASS_REFERENCE));

    static boolean keep(StackWalker.StackFrame f) {
        String c = f.getClassName();
        return c.startsWith("R21FzWalk1") || c.startsWith("jdk.internal.reflect.")
                || c.startsWith("java.lang.reflect.");
    }

    static String fmt(StackWalker.StackFrame f) {
        String c = f.getClassName();
        if (c.startsWith("R21FzWalk1")) {
            return c + "." + f.getMethodName() + ":" + f.getLineNumber();
        }
        return c + "." + f.getMethodName();
    }

    static String walk(StackWalker w, int limit) {
        return w.walk(s -> s.filter(R21FzWalk1::keep).limit(limit).map(R21FzWalk1::fmt)
                .collect(Collectors.joining(" ")));
    }

    static String walkSkip(int skip) {
        return PLAIN.walk(s -> s.skip(skip).filter(R21FzWalk1::keep).limit(3).map(R21FzWalk1::fmt)
                .collect(Collectors.joining(" ")));
    }

    static String trace(int limit) {
        StackTraceElement[] st = new Throwable().getStackTrace();
        StringBuilder sb = new StringBuilder();
        int n = 0;
        for (StackTraceElement e : st) {
            if (e.getClassName().startsWith("R21FzWalk1") && n++ < limit) {
                sb.append(e.getMethodName()).append(':').append(e.getLineNumber()).append(' ');
            }
        }
        return sb.toString().trim();
    }

    static int sink;

    static String rec(int d, int mode) {
        if (d == 0) {
            switch (mode) {
                case 0:
                    return walk(PLAIN, 8);
                case 1:
                    return walk(RETAIN, 8) + " caller=" + callerName();
                case 2:
                    return walkSkip(4);
                case 3:
                    return trace(8);
                default:
                    sink++;
                    return "";
            }
        }
        String r = rec(d - 1, mode);
        sink += d;
        return r;
    }

    static String callerName() {
        return RETAIN.getCallerClass().getName();
    }

    public static String target(int mode) {
        return mode == 0 ? walk(PLAIN, 6) : walk(REFLECT, 12);
    }

    public static class Made {
        public final String seen;

        public Made(int mode) {
            seen = mode == 0 ? walk(PLAIN, 6) : walk(REFLECT, 12);
        }
    }

    public static class InitWalk {
        public static final String SEEN = walk(PLAIN, 6) + " || " + walk(REFLECT, 14);
    }

    static void rows(String tag) throws Exception {
        for (int depth : new int[] {5, 60, 400}) {
            for (int mode = 0; mode < 4; mode++) {
                System.out.println("row " + tag + " d" + depth + " m" + mode + " " + rec(depth, mode));
            }
        }
        Method t = R21FzWalk1.class.getMethod("target", int.class);
        System.out.println("row " + tag + " invoke-plain " + t.invoke(null, 0));
        System.out.println("row " + tag + " invoke-reflect " + t.invoke(null, 1));
        Constructor<Made> c = Made.class.getConstructor(int.class);
        System.out.println("row " + tag + " ctor-plain " + c.newInstance(0).seen);
        System.out.println("row " + tag + " ctor-reflect " + c.newInstance(1).seen);
    }

    public static void main(String[] args) throws Exception {
        rows("cold");
        for (int r = 0; r < 50_000; r++) {
            rec(6, 4);
        }
        String[] hot = new String[3];
        for (int r = 0; r < 2_000; r++) {
            hot[r % 3] = rec(8, r % 3);
        }
        System.out.println("warm " + sink + " " + hot[0].length() + " " + hot[1].length() + " " + hot[2].length());
        rows("hot");
        Method forName = Class.class.getMethod("forName", String.class);
        Class<?> k = (Class<?>) forName.invoke(null, "R21FzWalk1$InitWalk");
        System.out.println("clinit " + k.getField("SEEN").get(null));
        List<String> list = List.of("a");
        System.out.println("lambda " + list.stream().map(x -> walk(PLAIN, 4)).findFirst().orElse(""));
        rows("after");
    }
}
