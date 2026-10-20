// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 125, lane L1 -- the review probe for the router
// census row `lang_stackwalker.rs` `base java/lang/StackStreamFactory$StackFrameBuffer`
// (i74-L1 step 2). No family row: the one native on that class,
// `StackFrameBuffer.fill`, is a reviewed `Intrinsic` registered only under
// `--jdk-only` over a real JDK (`CRATONVM_STACKWALK_JDK_PIPELINE_BATCHED`); its
// routed arm (`invoke_special_bytecode_only`) is the decline for a buffer that
// is not a pipeline walk's. `--compatible` registers nothing there. Page:
// docs/internal/proposals/interpreter/i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows-20261107.md.
//
// Covers the walks that reach `fill` (a StackFrameInfo buffer: method info
// kept) and the ClassFrameBuffer ones beside them (DROP_METHOD_INFO), across
// the first-batch boundary: frame counts and names of a deep walk, skip and
// limit past the first batch, a short-circuiting findFirst deep in the walk,
// RETAIN_CLASS_REFERENCE declaring classes, a nested walk inside a walk, and
// a walk that consumes every frame twice through a collected list. TIME lines:
// a deep walk's count, with and without method info.
//
// Must be SAME in --jdk-only, --jdk-only with CRATONVM_STACKWALK_JDK_PIPELINE_BATCHED=0,
// and --compatible (TIME lines filtered):
//   javac -d out L1W125StackFrameBufferWalks.java
//   java -cp out L1W125StackFrameBufferWalks
//   cratonvm --jdk-only -cp out L1W125StackFrameBufferWalks
//   CRATONVM_STACKWALK_JDK_PIPELINE_BATCHED=0 cratonvm --jdk-only -cp out L1W125StackFrameBufferWalks
//   cratonvm --compatible -cp out L1W125StackFrameBufferWalks
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint identical; TIME lines filtered)
// plain depth=0 frames>=true deepFrames=1 first=[lambda$probeFrames$0, deep, probeFrames] lastIsMain=true
// retain depth=0 frames>=true deepFrames=1 first=[lambda$probeFrames$0, deep, probeFrames] lastIsMain=true
// drop depth=0 frames>=true ownFrames>depth=true last=L1W125StackFrameBufferWalks
// plain depth=3 frames>=true deepFrames=4 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// retain depth=3 frames>=true deepFrames=4 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// drop depth=3 frames>=true ownFrames>depth=true last=L1W125StackFrameBufferWalks
// plain depth=7 frames>=true deepFrames=8 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// retain depth=7 frames>=true deepFrames=8 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// drop depth=7 frames>=true ownFrames>depth=true last=L1W125StackFrameBufferWalks
// plain depth=8 frames>=true deepFrames=9 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// retain depth=8 frames>=true deepFrames=9 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// drop depth=8 frames>=true ownFrames>depth=true last=L1W125StackFrameBufferWalks
// plain depth=9 frames>=true deepFrames=10 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// retain depth=9 frames>=true deepFrames=10 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// drop depth=9 frames>=true ownFrames>depth=true last=L1W125StackFrameBufferWalks
// plain depth=30 frames>=true deepFrames=31 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// retain depth=30 frames>=true deepFrames=31 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// drop depth=30 frames>=true ownFrames>depth=true last=L1W125StackFrameBufferWalks
// plain depth=64 frames>=true deepFrames=65 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// retain depth=64 frames>=true deepFrames=65 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// drop depth=64 frames>=true ownFrames>depth=true last=L1W125StackFrameBufferWalks
// plain depth=200 frames>=true deepFrames=201 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// retain depth=200 frames>=true deepFrames=201 first=[lambda$probeFrames$0, deep, deep] lastIsMain=true
// drop depth=200 frames>=true ownFrames>depth=true last=L1W125StackFrameBufferWalks
// skip 0 limit 4: lambda$skipLimit$0,deep,deep,deep
// skip 8 limit 4: deep,deep,deep,deep
// skip 40 limit 6: deep,deep,deep,deep,deep,deep
// skip 45 limit 20: deep,deep,deep,deep,deep,deep,deep,skipLimit,main
// drop skip 30 limit 3: L1W125StackFrameBufferWalks,L1W125StackFrameBufferWalks,L1W125StackFrameBufferWalks
// drop getMethodName: java.lang.UnsupportedOperationException: null
// findMain depth=150 plain: L1W125StackFrameBufferWalks.main hasBci=true line>0=true
// findMain depth=150 retain: L1W125StackFrameBufferWalks.main hasBci=true line>0=true
// nested: outerFirst5=5 inner=62
// declaring: L1W125StackFrameBufferWalks
// descriptor: (ILjava/util/function/Function;)Ljava/lang/Object; type=(int,Function)Object native=false ste=deep
// twice: collected deep=91 last=main again=lambda$twice$0
// caller: L1W125StackFrameBufferWalks
// escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
// sink>0=true
// HOTSPOT_EXPECTED_END

import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.util.EnumSet;
import java.util.List;
import java.util.Optional;
import java.util.function.Function;
import java.util.stream.Collectors;
import java.util.stream.Stream;

public class L1W125StackFrameBufferWalks {

    static final StackWalker PLAIN = StackWalker.getInstance();
    static final StackWalker RETAIN = StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE);
    static final StackWalker DROP = StackWalker.getInstance(EnumSet.of(Option.DROP_METHOD_INFO,
            Option.RETAIN_CLASS_REFERENCE));

    static <T> T deep(int n, Function<Integer, T> leaf) {
        if (n == 0) {
            return leaf.apply(0);
        }
        return deep(n - 1, leaf);
    }

    static String probeFrames(StackWalker w, int depth) {
        return deep(depth, x -> w.walk(s -> {
            List<String> names = s.map(f -> f.getMethodName()).collect(Collectors.toList());
            int deepCount = 0;
            for (String name : names) {
                if (name.equals("deep")) {
                    deepCount++;
                }
            }
            return "frames>=" + (names.size() >= depth) + " deepFrames=" + deepCount + " first="
                    + names.subList(0, Math.min(3, names.size())) + " lastIsMain="
                    + names.get(names.size() - 1).equals("main");
        }));
    }

    static String dropFrames(int depth) {
        return deep(depth, x -> DROP.walk(s -> {
            List<Class<?>> classes = s.map(StackFrame::getDeclaringClass).collect(Collectors.toList());
            int own = 0;
            for (Class<?> c : classes) {
                if (c == L1W125StackFrameBufferWalks.class) {
                    own++;
                }
            }
            return "frames>=" + (classes.size() >= depth) + " ownFrames>depth=" + (own > depth) + " last="
                    + classes.get(classes.size() - 1).getSimpleName();
        }));
    }

    static String skipLimit(StackWalker w, int depth, int skip, int limit) {
        return deep(depth, x -> w.walk(s -> s.skip(skip).limit(limit)
                .map(f -> f.getMethodName() + (f.getClassName().equals(L1W125StackFrameBufferWalks.class.getName()) ? "" : "?"))
                .collect(Collectors.joining(","))));
    }

    static String findMain(StackWalker w, int depth) {
        return deep(depth, x -> w.walk(s -> {
            Optional<StackFrame> f = s.filter(fr -> fr.getMethodName().equals("main")).findFirst();
            return f.map(fr -> fr.getClassName() + "." + fr.getMethodName() + " hasBci=" + (fr.getByteCodeIndex() >= 0)
                    + " line>0=" + (fr.getLineNumber() > 0)).orElse("none");
        }));
    }

    static String nested(int depth) {
        return deep(depth, x -> RETAIN.walk(outer -> {
            long before = outer.limit(5).count();
            String inner = deep(20, y -> PLAIN.walk(s -> "inner=" + s.filter(f -> f.getMethodName().equals("deep")).count()));
            return "outerFirst5=" + before + " " + inner;
        }));
    }

    static String declaringClasses(int depth) {
        return deep(depth, x -> RETAIN.walk(s -> s.map(f -> f.getDeclaringClass().getSimpleName())
                .distinct().collect(Collectors.joining(","))));
    }

    static String descriptors(int depth) {
        return deep(depth, x -> RETAIN.walk(s -> s.filter(f -> f.getMethodName().equals("deep")).findFirst()
                .map(f -> f.getDescriptor() + " type=" + f.getMethodType() + " native=" + f.isNativeMethod()
                        + " ste=" + f.toStackTraceElement().getMethodName())
                .orElse("none")));
    }

    static String twice(int depth) {
        return deep(depth, x -> {
            List<StackFrame> all = PLAIN.walk(s -> s.collect(Collectors.toList()));
            int n = 0;
            for (StackFrame f : all) {
                if (f.getMethodName().equals("deep")) {
                    n++;
                }
            }
            String last = all.get(all.size() - 1).getMethodName();
            return "collected deep=" + n + " last=" + last + " again=" + all.get(0).getMethodName();
        });
    }

    public static void main(String[] args) {
        for (int depth : new int[] {0, 3, 7, 8, 9, 30, 64, 200}) {
            System.out.println("plain depth=" + depth + " " + probeFrames(PLAIN, depth));
            System.out.println("retain depth=" + depth + " " + probeFrames(RETAIN, depth));
            System.out.println("drop depth=" + depth + " " + dropFrames(depth));
        }
        System.out.println("skip 0 limit 4: " + skipLimit(PLAIN, 50, 0, 4));
        System.out.println("skip 8 limit 4: " + skipLimit(RETAIN, 50, 8, 4));
        System.out.println("skip 40 limit 6: " + skipLimit(RETAIN, 50, 40, 6));
        System.out.println("skip 45 limit 20: " + skipLimit(RETAIN, 50, 45, 20));
        System.out.println("drop skip 30 limit 3: " + deep(50, x -> DROP.walk(s -> s.skip(30).limit(3)
                .map(f -> f.getDeclaringClass().getSimpleName()).collect(Collectors.joining(",")))));
        try {
            deep(5, x -> DROP.walk(s -> s.findFirst().get().getMethodName()));
            System.out.println("drop getMethodName: no exception");
        } catch (UnsupportedOperationException e) {
            System.out.println("drop getMethodName: " + e.getClass().getName() + ": " + e.getMessage());
        }
        System.out.println("findMain depth=150 plain: " + findMain(PLAIN, 150));
        System.out.println("findMain depth=150 retain: " + findMain(RETAIN, 150));
        System.out.println("nested: " + nested(40));
        System.out.println("declaring: " + declaringClasses(60));
        System.out.println("descriptor: " + descriptors(12));
        System.out.println("twice: " + twice(90));
        System.out.println("caller: " + deep(25, x -> RETAIN.getCallerClass().getSimpleName()));
        try {
            Stream<StackFrame> escaped = PLAIN.walk(s -> s);
            escaped.count();
            System.out.println("escaped: no exception");
        } catch (IllegalStateException e) {
            System.out.println("escaped: " + e.getClass().getName() + ": " + e.getMessage());
        }

        // TIME rows.
        long sink = 0;
        for (int rep = 0; rep < 3; rep++) {
            int n = 2_000;
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                sink += deep(60, x -> PLAIN.walk(Stream::count));
            }
            System.out.println("TIME walk60-count rep=" + rep + " ns/op=" + (System.nanoTime() - t0) / n);
            t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                sink += deep(60, x -> DROP.walk(Stream::count));
            }
            System.out.println("TIME walk60-drop-count rep=" + rep + " ns/op=" + (System.nanoTime() - t0) / n);
        }
        System.out.println("sink>0=" + (sink > 0));
    }
}
