// JIT round 21 wave 1, lane walk: StackWalker deep walks, answers and cost.
//
// Target (r20 handoff perf table, R20PNatWalk): deep walks count / drop / own / own-reflective cost
// 64927 / 68056 / 95208 / 87605 ns against HotSpot 3823 / 3534 / 4487 / 2373 (~47 frames). This probe
// times the same shapes at depth 50 and 200 (the recursion's own frames; the stack under it is the
// probe's harness and main), the short-circuit walks HotSpot answers from its first batch
// (findFirst, dropWhile to the 4th frame), getCallerClass at depth 200 (plain and through
// Method.invoke), and a RETAIN_CLASS_REFERENCE + SHOW_REFLECT_FRAMES walk. The recursion is
// deepA -> stepA -> deepB -> stepB -> deepA ...: after the warm-up the step* methods are inlined into
// the compiled deep* bodies, so pass 2's fact lines list frames of inlined compiled methods; the
// cha-* rows walk under a frame deoptimized by a class-hierarchy invalidation (Circle is loaded
// only in pass 2, after Square's call site was compiled monomorphic).
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21WalkDeep.java):
//   P=tools/probes/out
//   cratonvm -Xmx512m -cp $P R21WalkDeep                         # --jdk-only (the launcher default)
//   cratonvm --compatible -Xmx512m -cp $P R21WalkDeep            # both modes, same fact lines
//   java -Xmx512m -cp $P R21WalkDeep                             # HotSpot 25
//   optional argument: iterations per timed rep (default 2000); `facts` prints the fact lines only.
// Arms (this lane's switches, both opt-in; run each in both modes, 5 interleaved reps):
//   def   ||
//   deep  |CRATONVM_STACKWALK_DEEP_WALK_LINES=1|          (WALK21-1: frames past 128 keep the lean build)
//   once  |CRATONVM_STACKWALK_PIPELINE_LAYOUT_ONCE=1|     (WALK21-2: pipeline layout resolved once)
//   both  |CRATONVM_STACKWALK_DEEP_WALK_LINES=1 CRATONVM_STACKWALK_PIPELINE_LAYOUT_ONCE=1|
//   lines |CRATONVM_STACKWALK_DEFERRED_LINES=1|           (WALK21-4: a frame's line resolved when built)
//   opts  |CRATONVM_STACKWALK_OPTION_ORDINALS_ONCE=1|     (WALK21-5: walker option ordinals once)
//   byid  |CRATONVM_STACKWALK_PIPELINE_INVOKE_BY_ID=1|    (WALK21-6: JDK entry points by class id)
//   front |CRATONVM_STACKWALK_METHOD_SLOT_FRONT_CACHE=1|  (WALK21-8: lock-free method-slot cache)
//   caller|CRATONVM_STACKWALK_CALLER_CLASS_TAIL=1|        (WALK21-9: getCallerClass from the top frames)
//   window|CRATONVM_STACKWALK_WINDOW_CAPTURE=1|           (WALK21-10: a window, the rest on demand)
//   all   |every switch above =1|
// Wave-3 deciders: caller -> getcaller-d200 (target ~1 us, flat in depth: compare a d50 run);
// window -> walk-first-d50 / walk-first-d200 / walk-dropwhile-d200 (target: d200 == d50);
// controls walk-count-d200 / walk-own-d200 (must not lose beyond noise). Every fact line equal to
// HotSpot in both arms (stderr must show no `[walk-window]` line).
//   exact0|CRATONVM_STACKWALK_REFLECTION_SCREEN_EXACT=0|  (WALK21-3 kill-switch control, facts only)
// Deciders: deep -> time walk-count-d200, walk-own-d200, walk-retain-reflect-d200 (control:
// the d50 rows, which have no frame past 128, must not move); once / opts / byid -> walk-first-d50,
// walk-first-d200, walk-dropwhile-d200 (fixed per-walk cost), controls the count rows; lines ->
// walk-first-d200, walk-dropwhile-d200 (control walk-count-d200, flat); front -> every row (small).
// Census (not for timing): CRATONVM_DBG_WALK_SECTIONS=1 prints `[walk-sections] ... capture= screen=
// build= apply= batch-build= batch-frame=` per thread, and (WALK21-7) one `[walk-frame-split]
// vm=.. frames=N ... capture/frame= screen/frame= first-build/frame= batch-build/frame=
// java+function/frame= per-walk=` line per VM and walk depth (first walk and every 512th).
//
// HotSpot expectation: every non-`time` line identical (diff them; `p1` is the cold pass, `p2` the
// pass after the warm-up and the CHA invalidation). HotSpot time rows ~2-5 us for the d50 full walks,
// ~8-15 us at d200, ~1-3 us for walk-first / walk-dropwhile at any depth (one batch), getcaller
// ~0.3-1 us.

import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.EnumSet;
import java.util.List;
import java.util.function.Supplier;
import java.util.stream.Collectors;

public class R21WalkDeep {
    static final StackWalker PLAIN = StackWalker.getInstance();
    static final StackWalker RETAIN = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);
    static final StackWalker RETAIN_REFLECT = StackWalker.getInstance(EnumSet.of(
            StackWalker.Option.RETAIN_CLASS_REFERENCE, StackWalker.Option.SHOW_REFLECT_FRAMES));
    static final StackWalker HIDDEN = StackWalker.getInstance(EnumSet.of(
            StackWalker.Option.RETAIN_CLASS_REFERENCE, StackWalker.Option.SHOW_HIDDEN_FRAMES));
    static final StackWalker DROP = StackWalker.getInstance(StackWalker.Option.DROP_METHOD_INFO);

    static long sink;

    // ---- the recursion: two methods per level, the step* hops tiny (inlined once compiled) ----

    static <T> T deepA(int n, Supplier<T> leaf) {
        if (n <= 0) return leaf.get();
        return stepA(n - 1, leaf);
    }

    static <T> T stepA(int n, Supplier<T> leaf) {
        return deepB(n, leaf);
    }

    static <T> T deepB(int n, Supplier<T> leaf) {
        if (n <= 0) return leaf.get();
        return stepB(n - 1, leaf);
    }

    static <T> T stepB(int n, Supplier<T> leaf) {
        return deepA(n, leaf);
    }

    /** The recursion's own frames: about `frames` of them (two per level). */
    static <T> T deep(int frames, Supplier<T> leaf) {
        return deepA(frames / 2, leaf);
    }

    // ---- getCallerClass shapes ----

    static final class Asker {
        static Class<?> ask() {
            return RETAIN.getCallerClass();
        }
    }

    static final class Caller {
        static Class<?> call() {
            return Asker.ask();
        }
    }

    static final class ReflectCaller {
        static final Method ASK;
        static {
            try {
                ASK = Asker.class.getDeclaredMethod("ask");
            } catch (ReflectiveOperationException e) {
                throw new ExceptionInInitializerError(e);
            }
        }

        static Class<?> call() {
            try {
                return (Class<?>) ASK.invoke(null);
            } catch (ReflectiveOperationException e) {
                throw new RuntimeException(e);
            }
        }
    }

    // ---- a walk through Method.invoke ----

    static final class Reflected {
        static final Method TARGET;
        static final Method PLAIN_TARGET;
        static {
            try {
                TARGET = Reflected.class.getDeclaredMethod("target");
                PLAIN_TARGET = Reflected.class.getDeclaredMethod("plainTarget");
            } catch (ReflectiveOperationException e) {
                throw new ExceptionInInitializerError(e);
            }
        }

        static String target() {
            return RETAIN_REFLECT.walk(s -> s.limit(6)
                    .map(f -> name(f.getDeclaringClass()) + "." + f.getMethodName())
                    .collect(Collectors.joining(",")));
        }

        static String plainTarget() {
            return PLAIN.walk(s -> s.limit(4)
                    .map(f -> strip(f.getClassName()) + "." + f.getMethodName())
                    .collect(Collectors.joining(",")));
        }

        static String via(Method m) {
            try {
                return (String) m.invoke(null);
            } catch (ReflectiveOperationException e) {
                throw new RuntimeException(e);
            }
        }
    }

    // ---- a call site compiled monomorphic, then invalidated (CHA) ----

    abstract static class Shape {
        abstract String leaf();
    }

    static final class Square extends Shape {
        String leaf() {
            return ownFrames(PLAIN, 12);
        }
    }

    static final class Circle extends Shape {
        String leaf() {
            return ownFrames(PLAIN, 12);
        }
    }

    static String viaShape(Shape s, int n) {
        if (n <= 0) return s.leaf();
        return viaShape(s, n - 1);
    }

    // ---- helpers ----

    static String strip(String className) {
        int at = className.indexOf("/0x");
        return at < 0 ? className : className.substring(0, at + 3);
    }

    static String name(Class<?> c) {
        return strip(c.getName());
    }

    /** method:line of the first `limit` frames of this probe's own classes. */
    static String ownFrames(StackWalker walker, int limit) {
        return walker.walk(s -> s.filter(f -> f.getClassName().startsWith("R21WalkDeep"))
                .limit(limit)
                .map(f -> f.getMethodName() + ":" + f.getLineNumber())
                .collect(Collectors.joining(",")));
    }

    /** Every frame of this probe's own classes, method:line. */
    static String allOwnFrames() {
        return PLAIN.walk(s -> s.filter(f -> f.getClassName().startsWith("R21WalkDeep"))
                .map(f -> f.getMethodName() + ":" + f.getLineNumber())
                .collect(Collectors.joining(",")));
    }

    // ---- facts ----

    static void fact(String pass, String row, Object value) {
        System.out.println(pass + " " + row + " " + value);
    }

    static void facts(String pass) {
        fact(pass, "d50-frames", deep(50, R21WalkDeep::allOwnFrames));
        String d200 = deep(200, R21WalkDeep::allOwnFrames);
        String[] d200Frames = d200.split(",");
        fact(pass, "d200-frames-count", d200Frames.length);
        fact(pass, "d200-frames-hash", d200.hashCode());
        fact(pass, "d200-frames-tail", String.join(",",
                Arrays.copyOfRange(d200Frames, Math.max(0, d200Frames.length - 6), d200Frames.length)));
        fact(pass, "d200-frames-at-128", String.join(",",
                Arrays.copyOfRange(d200Frames, Math.min(124, d200Frames.length),
                        Math.min(132, d200Frames.length))));
        fact(pass, "d50-count", deep(50, () -> PLAIN.walk(s -> s.count())));
        fact(pass, "d200-count", deep(200, () -> PLAIN.walk(s -> s.count())));
        fact(pass, "d200-drop-count", deep(200, () -> DROP.walk(s -> s.count())));
        fact(pass, "d200-own-count", deep(200, () -> PLAIN.walk(
                s -> s.filter(f -> f.getClassName().equals("R21WalkDeep")).count())));
        // Elements (the lazy StackTraceElement each frame builds at its first read) around the
        // 128-frame boundary of one walk, and the last frames of it.
        fact(pass, "d200-elements", deep(200, () -> PLAIN.walk(s -> {
            List<StackWalker.StackFrame> all = s.collect(Collectors.toList());
            List<String> out = new ArrayList<>();
            for (int i : new int[] {0, 1, 126, 127, 128, 129, 130, 199, 200, 201}) {
                if (i < all.size()) {
                    StackWalker.StackFrame f = all.get(i);
                    out.add(i + "=" + f.toStackTraceElement() + "/" + f.getFileName()
                            + "/" + f.isNativeMethod());
                }
            }
            return String.join(",", out);
        })));
        fact(pass, "d200-retain", deep(200, () -> RETAIN.walk(s -> s.skip(125).limit(6)
                .map(f -> name(f.getDeclaringClass()) + "." + f.getMethodName() + f.getDescriptor())
                .collect(Collectors.joining(",")))));
        fact(pass, "d50-bci", deep(50, () -> PLAIN.walk(s -> s.limit(8)
                .map(f -> f.getMethodName() + "@" + f.getByteCodeIndex())
                .collect(Collectors.joining(",")))));
        fact(pass, "d200-first", deep(200, () -> PLAIN.walk(s -> s.findFirst()
                .map(f -> f.getMethodName() + ":" + f.getLineNumber()).orElse("none"))));
        fact(pass, "d200-dropwhile", deep(200, () -> PLAIN.walk(s -> s
                .dropWhile(f -> !f.getMethodName().equals("deepB")).skip(1).findFirst()
                .map(f -> f.getMethodName() + ":" + f.getLineNumber()).orElse("none"))));
        fact(pass, "d200-getcaller", deep(200, () -> name(Caller.call())));
        fact(pass, "d200-getcaller-reflect", deep(200, () -> name(ReflectCaller.call())));
        fact(pass, "d50-retain-reflect", deep(50, () -> Reflected.via(Reflected.TARGET)));
        fact(pass, "d50-plain-reflect", deep(50, () -> Reflected.via(Reflected.PLAIN_TARGET)));
        fact(pass, "d50-hidden-lambda", deep(50, () -> HIDDEN.walk(s -> s.limit(5)
                .map(f -> name(f.getDeclaringClass()) + "." + f.getMethodName() + ":" + f.getLineNumber())
                .collect(Collectors.joining(",")))));
        fact(pass, "d50-trace", deep(50, () -> {
            StackTraceElement[] st = new Throwable().getStackTrace();
            StringBuilder b = new StringBuilder();
            for (int i = 0; i < 8 && i < st.length; i++) {
                b.append(i == 0 ? "" : ",").append(st[i].getMethodName()).append(':')
                        .append(st[i].getLineNumber());
            }
            return b.toString();
        }));
        fact(pass, "cha-square", viaShape(new Square(), 6));
    }

    static void warmCha() {
        Shape square = new Square();
        for (int i = 0; i < 20_000; i++) {
            sink += viaShape(square, 6).length();
        }
    }

    // ---- timing ----

    interface Op {
        long run();
    }

    static void time(String row, int iters, Op op) {
        for (int w = 0; w < 3; w++) {
            for (int i = 0; i < iters; i++) sink += op.run();
        }
        long[] reps = new long[9];
        for (int r = 0; r < reps.length; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) sink += op.run();
            reps[r] = (System.nanoTime() - t0) / iters;
        }
        Arrays.sort(reps);
        System.out.println("time " + row + " " + reps[reps.length / 2] + " ns/op");
    }

    static long countAt(int frames, StackWalker walker) {
        return deep(frames, () -> walker.walk(s -> s.count()));
    }

    static long ownAt(int frames) {
        return deep(frames, () -> PLAIN.walk(
                s -> s.filter(f -> f.getClassName().equals("R21WalkDeep")).count()));
    }

    static long retainReflectAt(int frames) {
        return deep(frames, () -> RETAIN_REFLECT.walk(
                s -> s.filter(f -> f.getDeclaringClass() == R21WalkDeep.class).count()));
    }

    static long firstAt(int frames) {
        return deep(frames, () -> PLAIN.walk(s -> s.findFirst().map(f -> 1L).orElse(0L)));
    }

    static long dropWhileAt(int frames) {
        return deep(frames, () -> PLAIN.walk(s -> s
                .dropWhile(f -> !f.getMethodName().equals("deepB")).findFirst()
                .map(f -> 1L).orElse(0L)));
    }

    static long callerAt(int frames) {
        return deep(frames, () -> Caller.call() == Caller.class ? 1L : 0L);
    }

    static long recursionOnly(int frames) {
        return deep(frames, () -> 1L);
    }

    public static void main(String[] args) {
        boolean factsOnly = args.length > 0 && args[0].equals("facts");
        int iters = (args.length > 0 && !factsOnly) ? Integer.parseInt(args[0]) : 2000;
        facts("p1");
        // Warm-up: compile the recursion (the step* hops inline into deep*), then the CHA site.
        for (int i = 0; i < 3000; i++) {
            sink += countAt(50, PLAIN) + firstAt(200);
        }
        warmCha();
        facts("p2");
        fact("p2", "cha-circle", viaShape(new Circle(), 6));
        fact("p2", "cha-square-after", viaShape(new Square(), 6));
        if (factsOnly) return;
        time("recursion-only-d200", iters, () -> recursionOnly(200));
        time("walk-count-d50", iters, () -> countAt(50, PLAIN));
        time("walk-count-d200", iters, () -> countAt(200, PLAIN));
        time("walk-drop-d50", iters, () -> countAt(50, DROP));
        time("walk-drop-d200", iters, () -> countAt(200, DROP));
        time("walk-own-d50", iters, () -> ownAt(50));
        time("walk-own-d200", iters, () -> ownAt(200));
        time("walk-retain-reflect-d50", iters, () -> retainReflectAt(50));
        time("walk-retain-reflect-d200", iters, () -> retainReflectAt(200));
        time("walk-first-d50", iters, () -> firstAt(50));
        time("walk-first-d200", iters, () -> firstAt(200));
        time("walk-dropwhile-d200", iters, () -> dropWhileAt(200));
        time("getcaller-d200", iters, () -> callerAt(200));
        System.out.println("done " + (sink != 0));
    }
}
