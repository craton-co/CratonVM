// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: WALK21-4 deferred lines
// (`CRATONVM_STACKWALK_DEFERRED_LINES`: the lean capture, the line resolved
// when the frame's batch is built, `settle_walk_line` writing the frame's
// slot of its chunk) and WALK21-8, the per-thread method-slot front cache
// (`CRATONVM_STACKWALK_METHOD_SLOT_FRONT_CACHE`: `find_method_index_memoized`,
// 256 direct-mapped entries, verified by name + descriptor).
//
// Risk targeted:
//  * deferred lines: the line of a frame consumed in a LATER batch than the
//    first (walks past the first batch size), a frame never consumed, frames
//    whose line differs per call (the same method calling the walk from four
//    different lines), a compiled caller and an OSR loop's frame (the line
//    inside the loop), StackFrame.toStackTraceElement() and getByteCodeIndex
//    against the line;
//  * the front cache: overloads (same name, different descriptor), the same
//    name AND descriptor in nine classes (40 receivers; key collisions in 256
//    lines and in the shared memo), and two threads walking the same frames
//    concurrently.
// Lines need `javac -g`. Cold, hot (50k walks spread over the shapes), a
// late class (new frames / a deopt of a walking caller), again.
//
// Run:
//   javac -g -d out R21Fz2WalkLines1.java
//   java -Xmx256m -cp out R21Fz2WalkLines1                               # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2WalkLines1
//   CRATONVM_STACKWALK_DEFERRED_LINES=1 cratonvm -Xmx256m -cp out R21Fz2WalkLines1
//   CRATONVM_STACKWALK_METHOD_SLOT_FRONT_CACHE=1 cratonvm ...
//   all walk opt-ins together (CRATONVM_STACKWALK_DEEP_WALK_LINES=1 _DEFERRED_LINES=1 _PIPELINE_LAYOUT_ONCE=1
//     _OPTION_ORDINALS_ONCE=1 _PIPELINE_INVOKE_BY_ID=1 _METHOD_SLOT_FRONT_CACHE=1)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.util.List;
import java.util.stream.Collectors;

public class R21Fz2WalkLines1 {
    static final int WARM = 50_000;
    static final StackWalker W = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);

    static String lines(int limit) {
        return W.walk(s -> s.limit(limit).map(f -> f.getMethodName() + ":" + f.getLineNumber())
                .collect(Collectors.joining(",")));
    }

    static String full(int skip, int limit) {
        return W.walk(s -> s.skip(skip).limit(limit).map(f -> {
            StackTraceElement e = f.toStackTraceElement();
            return f.getDeclaringClass().getSimpleName() + "." + f.getMethodName() + f.getDescriptor() + ":"
                    + f.getLineNumber() + "=" + e.getLineNumber() + (e.getMethodName().equals(f.getMethodName()) ? "" : "!");
        }).collect(Collectors.joining(",")));
    }

    // One method, four call lines.
    static String fromLines(int which) {
        switch (which) {
            case 0:
                return lines(3);
            case 1:
                return lines(3);
            case 2:
                return full(0, 3);
            default:
                return W.walk(s -> s.skip(1).findFirst().map(f -> f.getMethodName() + ":" + f.getLineNumber())
                        .orElse("none"));
        }
    }

    // Overloads: same name, different descriptors.
    static String ov(int x) {
        return full(0, 2);
    }

    static String ov(long x) {
        return full(0, 2);
    }

    static String ov(Object x) {
        return full(0, 2);
    }

    static String ov(int x, int y) {
        return full(0, 2);
    }

    // A deep recursion: frames consumed in later batches.
    static String deep(int d, int limit) {
        if (d == 0) {
            return lines(limit);
        }
        return deep(d - 1, limit);
    }

    static String osrLoop(int n) {
        String last = "";
        for (int i = 0; i < n; i++) {
            if (i == n - 1) {
                last = lines(2);
            }
        }
        return last;
    }

    interface Walker {
        String run();
    }

    // Nine classes (40 receivers) with the same method name and descriptor.
    static final Walker[] SAME = new Walker[40];

    static {
        for (int i = 0; i < SAME.length; i++) {
            SAME[i] = Clones.make(i);
        }
    }

    static String row() {
        StringBuilder sb = new StringBuilder();
        for (int w = 0; w < 4; w++) {
            sb.append(" [").append(fromLines(w)).append(']');
        }
        sb.append(" ov[").append(ov(1)).append('|').append(ov(1L)).append('|').append(ov("o")).append('|')
                .append(ov(1, 2)).append(']');
        sb.append(" deep[").append(deep(40, 3)).append('|').append(deep(300, 2)).append('|')
                .append(deep(300, 290).length()).append(']');
        StringBuilder same = new StringBuilder();
        for (Walker w : SAME) {
            same.append(w.run()).append(';');
        }
        sb.append(" same=").append(same.toString().hashCode()).append(' ').append(SAME[0].run()).append(' ')
                .append(SAME[39].run());
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        System.out.println("row cold" + row());
        System.out.println("osr cold " + osrLoop(10));
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + fromLines(r & 3).hashCode();
            if ((r & 7) == 0) {
                h = h * 31 + ov(r).hashCode() + ov((long) r).hashCode() + SAME[r % 40].run().hashCode();
            }
            if ((r & 255) == 0) {
                h = h * 31 + deep(60, 50).hashCode();
            }
        }
        System.out.println("warm hash " + h);
        System.out.println("row hot" + row());
        System.out.println("osr hot " + osrLoop(3_000_000));
        String[] res = new String[2];
        Thread t1 = new Thread(() -> {
            String r0 = "";
            for (int i = 0; i < 1_000; i++) {
                r0 = row();
            }
            res[0] = r0;
        });
        Thread t2 = new Thread(() -> {
            String r0 = "";
            for (int i = 0; i < 5_000; i++) {
                r0 = deep(30, 25) + SAME[i % 40].run();
            }
            res[1] = r0;
        });
        t1.start();
        t2.start();
        t1.join();
        t2.join();
        System.out.println("thread1" + res[0]);
        System.out.println("thread2 " + res[1]);
        // A late class with the shared name/descriptor, and a caller first
        // seen now.
        Walker late = new Walker() {
            public String run() {
                return full(0, 2);
            }
        };
        List<String> got = List.of(late.run(), ov(new Object() {
        }));
        System.out.println("late " + got);
        System.out.println("row after" + row());
    }
}

// Nine classes: the same method name and descriptor, different lines.
final class Clones {
    static R21Fz2WalkLines1.Walker make(int i) {
        switch (i % 8) {
            case 0: return new C0();
            case 1: return new C1();
            case 2: return new C2();
            case 3: return new C3();
            case 4: return new C4();
            case 5: return new C5();
            case 6: return new C6();
            default: return i < 20 ? new C7() : new C8(i);
        }
    }

    static final class C0 implements R21Fz2WalkLines1.Walker { public String run() { return R21Fz2WalkLines1.full(0, 2); } }
    static final class C1 implements R21Fz2WalkLines1.Walker {
        public String run() {
            return R21Fz2WalkLines1.full(0, 2);
        }
    }
    static final class C2 implements R21Fz2WalkLines1.Walker {

        public String run() {

            return R21Fz2WalkLines1.full(0, 2);
        }
    }
    static final class C3 implements R21Fz2WalkLines1.Walker { public String run() { return R21Fz2WalkLines1.full(1, 1); } }
    static final class C4 implements R21Fz2WalkLines1.Walker { public String run() { String s = "x"; return s + R21Fz2WalkLines1.full(0, 2); } }
    static final class C5 implements R21Fz2WalkLines1.Walker { public String run() { return R21Fz2WalkLines1.full(0, 3); } }
    static final class C6 implements R21Fz2WalkLines1.Walker { public String run() { return R21Fz2WalkLines1.full(0, 1); } }
    static final class C7 implements R21Fz2WalkLines1.Walker { public String run() { return R21Fz2WalkLines1.full(0, 2); } }
    static final class C8 implements R21Fz2WalkLines1.Walker {
        final int i;

        C8(int i) {
            this.i = i;
        }

        public String run() {
            return i + R21Fz2WalkLines1.full(0, 2);
        }
    }
}
