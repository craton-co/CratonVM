// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.stream.Collectors;

/**
 * Differential oracle for `StackWalker`: every observable the API exposes,
 * printed to STDOUT in a form that must be byte-identical between HotSpot and
 * CratonVM.
 *
 * This exists because the walk implementation is being moved off the eager
 * "materialise every frame before the Function runs" native onto the JDK's own
 * batched `StackStreamFactory` path, and a walk that is merely FAST is not a
 * walk that is RIGHT. Line numbers are deliberately printed for this file's own
 * frames only (they are stable properties of this source), and JDK-internal
 * frame line numbers are never printed — those legitimately differ by JDK build.
 */
public class StackWalkerCrossVmProbe {

    static final StackWalker PLAIN = StackWalker.getInstance();
    static final StackWalker RETAIN = StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE);
    static final StackWalker SHOW_REFLECT =
            StackWalker.getInstance(Option.SHOW_REFLECT_FRAMES);

    // ---------------------------------------------------------------- helpers

    /** `Cls.method` for frames of this probe, `<jdk>` for anything else. */
    static String id(StackFrame f) {
        String c = f.getClassName();
        if (!c.startsWith("StackWalkerCrossVmProbe")) {
            return "<other>";
        }
        return c + "." + f.getMethodName();
    }

    static String own(List<StackFrame> frames) {
        return frames.stream().map(StackWalkerCrossVmProbe::id).collect(Collectors.joining(","));
    }

    // ------------------------------------------------------------- the ladder
    // Each rung is a separate stream shape. A batched, lazy walk and an eager
    // one must agree on all of them.

    static void rungCollectAll() {
        List<StackFrame> all = PLAIN.walk(s -> s.collect(Collectors.toList()));
        System.out.println("collect-all-own=" + own(all));
        System.out.println("collect-all-first=" + id(all.get(0)));
    }

    static void rungCount() {
        // The absolute count includes JDK/launcher frames, which differ by VM.
        // Count only OUR frames — that is a property of this program.
        long n = PLAIN.walk(s -> s.filter(f -> f.getClassName()
                .startsWith("StackWalkerCrossVmProbe")).count());
        System.out.println("count-own=" + n);
    }

    static void rungFindFirstHit() {
        Optional<StackFrame> f = PLAIN.walk(s -> s.filter(x -> x.getMethodName()
                .equals("rungFindFirstHit")).findFirst());
        System.out.println("findFirst-hit=" + f.map(StackWalkerCrossVmProbe::id).orElse("EMPTY"));
    }

    static void rungFindFirstMiss() {
        Optional<StackFrame> f = PLAIN.walk(s -> s.filter(x -> x.getMethodName()
                .equals("no-such-method-anywhere")).findFirst());
        System.out.println("findFirst-miss=" + f.isPresent());
    }

    static void rungSkipLimit() {
        List<StackFrame> f = PLAIN.walk(s -> s.skip(1).limit(3).collect(Collectors.toList()));
        System.out.println("skip1-limit3=" + own(f));
    }

    static void rungSkipPastEnd() {
        Optional<StackFrame> f = PLAIN.walk(s -> s.skip(100000).findFirst());
        System.out.println("skip-past-end=" + f.isPresent());
    }

    static void rungFrameFields() {
        StackFrame f = PLAIN.walk(s -> s.findFirst()).orElseThrow();
        System.out.println("field-class=" + f.getClassName());
        System.out.println("field-method=" + f.getMethodName());
        System.out.println("field-file=" + f.getFileName());
        System.out.println("field-line-positive=" + (f.getLineNumber() > 0));
        System.out.println("field-bci-nonneg=" + (f.getByteCodeIndex() >= 0));
        System.out.println("field-native=" + f.isNativeMethod());
        System.out.println("field-toString=" + f.toString().replaceAll(":[0-9]+\\)", ":LINE)"));
        StackTraceElement ste = f.toStackTraceElement();
        System.out.println("ste-class=" + ste.getClassName());
        System.out.println("ste-method=" + ste.getMethodName());
        System.out.println("ste-file=" + ste.getFileName());
        System.out.println("ste-line-eq-frame=" + (ste.getLineNumber() == f.getLineNumber()));
    }

    static void rungDeclaringClass() {
        Class<?> c = RETAIN.walk(s -> s.findFirst()).orElseThrow().getDeclaringClass();
        System.out.println("declaring=" + c.getName());
        // Without RETAIN_CLASS_REFERENCE the same call must throw.
        String thrown;
        try {
            PLAIN.walk(s -> s.findFirst()).orElseThrow().getDeclaringClass();
            thrown = "NO-THROW";
        } catch (UnsupportedOperationException e) {
            thrown = "UnsupportedOperationException";
        }
        System.out.println("declaring-no-retain=" + thrown);
    }

    static void rungRetainedAfterWalk() {
        // A frame returned OUT of walk() keeps its permission.
        StackFrame f = RETAIN.walk(s -> s.findFirst()).orElseThrow();
        System.out.println("retained-after-walk=" + f.getDeclaringClass().getName());
    }

    static void rungForEach() {
        List<String> seen = new ArrayList<>();
        PLAIN.forEach(f -> {
            if (f.getClassName().startsWith("StackWalkerCrossVmProbe")) {
                seen.add(id(f));
            }
        });
        System.out.println("forEach-own=" + String.join(",", seen));
    }

    static void rungCallerClass() {
        System.out.println("callerClass=" + callee());
    }

    static String callee() {
        return RETAIN.getCallerClass().getName();
    }

    static void rungNested() {
        // A walk started while another walk's Function is running.
        String outer = PLAIN.walk(s -> {
            String inner = PLAIN.walk(t -> t.filter(f -> f.getMethodName().equals("rungNested"))
                    .findFirst().map(StackWalkerCrossVmProbe::id).orElse("EMPTY"));
            return inner + "|" + s.filter(f -> f.getMethodName().equals("rungNested"))
                    .findFirst().map(StackWalkerCrossVmProbe::id).orElse("EMPTY");
        });
        System.out.println("nested=" + outer);
    }

    static void rungRepeatStable() {
        // The same walk, 200 times, must give the same answer every time --
        // a batched walk that leaks its cursor between calls fails here.
        String first = null;
        for (int i = 0; i < 200; i++) {
            String cur = PLAIN.walk(s -> own(s.collect(Collectors.toList())));
            if (first == null) {
                first = cur;
            } else if (!first.equals(cur)) {
                System.out.println("repeat-stable=DIVERGED@" + i + " " + cur);
                return;
            }
        }
        System.out.println("repeat-stable=true");
    }

    static void rungDeep(int d) {
        if (d > 0) {
            rungDeep(d - 1);
            return;
        }
        long own = PLAIN.walk(s -> s.filter(f -> f.getMethodName().equals("rungDeep")).count());
        System.out.println("deep-own-frames=" + own);
        Optional<StackFrame> hit = PLAIN.walk(s -> s.filter(f -> f.getMethodName()
                .equals("rungDeepEntry")).findFirst());
        System.out.println("deep-findFirst=" + hit.map(StackWalkerCrossVmProbe::id).orElse("EMPTY"));
    }

    static void rungDeepEntry() {
        rungDeep(40);
    }

    static void rungReflectFrames() throws Exception {
        // SHOW_REFLECT_FRAMES: the option must at least not break the walk.
        java.lang.reflect.Method m =
                StackWalkerCrossVmProbe.class.getDeclaredMethod("reflectTarget");
        m.setAccessible(true);
        m.invoke(null);
    }

    static void reflectTarget() {
        long n = SHOW_REFLECT.walk(s -> s.filter(f -> f.getMethodName().equals("reflectTarget"))
                .count());
        System.out.println("reflect-target-frames=" + n);
    }

    static void rungAnyMatchShortCircuit() {
        boolean b = PLAIN.walk(s -> s.anyMatch(f -> f.getMethodName()
                .equals("rungAnyMatchShortCircuit")));
        System.out.println("anyMatch=" + b);
    }

    static void rungMapToString() {
        String s = PLAIN.walk(st -> st.limit(2).map(StackWalkerCrossVmProbe::id)
                .collect(Collectors.joining("/")));
        System.out.println("map-limit2=" + s);
    }

    public static void main(String[] args) throws Exception {
        rungCollectAll();
        rungCount();
        rungFindFirstHit();
        rungFindFirstMiss();
        rungSkipLimit();
        rungSkipPastEnd();
        rungFrameFields();
        rungDeclaringClass();
        rungRetainedAfterWalk();
        rungForEach();
        rungCallerClass();
        rungNested();
        rungRepeatStable();
        rungDeepEntry();
        rungReflectFrames();
        rungAnyMatchShortCircuit();
        rungMapToString();
        System.out.println("PROBE-OK");
    }
}
