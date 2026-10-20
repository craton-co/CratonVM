// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: the three capture doors agree. WALK21-4
// (`CRATONVM_STACKWALK_DEFERRED_LINES`) changes only the StackWalker capture
// (`capture_stack_walk_trace_deferred`, lines resolved per batch), WALK21-1
// (`CRATONVM_STACKWALK_DEEP_WALK_LINES`) the lean build of deep frames, and
// WALK21-8 (`CRATONVM_STACKWALK_METHOD_SLOT_FRONT_CACHE`) the slot memo every
// capture uses -- Throwable, Thread.getStackTrace (own and another thread's)
// and StackWalker.
//
// Risk targeted: one capture path drifting from the others for the same
// frames: line numbers of compiled frames (inlined callees, a frame
// mid-OSR-loop, a frame that deopted between two captures), lambda and
// stream frames, constructor and static-initializer frames, a frame of a
// method overloaded in the same class. Every row captures the SAME stack
// three ways and prints each list plus whether they agree; another thread's
// stack is captured while it is parked at a known line. Lines need `javac -g`.
// Cold, hot (50k captures), a deopt trigger (a new receiver class on the
// capturing path), again.
//
// Run:
//   javac -g -d out R21Fz2WalkLines2.java
//   java -Xmx256m -cp out R21Fz2WalkLines2                               # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2WalkLines2
//   CRATONVM_STACKWALK_DEFERRED_LINES=1 cratonvm ..., CRATONVM_STACKWALK_DEEP_WALK_LINES=1 cratonvm ...,
//   CRATONVM_STACKWALK_METHOD_SLOT_FRONT_CACHE=1 cratonvm ..., all walk opt-ins together
//   CRATONVM_STACKWALK_REFLECTION_SCREEN_EXACT=0 cratonvm ... (kill)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.LockSupport;
import java.util.stream.Collectors;
import java.util.stream.IntStream;

public class R21Fz2WalkLines2 {
    static final int WARM = 50_000;

    interface Tap {
        String tap(int k);
    }

    static final class Direct implements Tap {
        public String tap(int k) {
            return capture(k);
        }
    }

    static final class Other implements Tap {
        public String tap(int k) {
            String r = capture(k + 1);
            return r;
        }
    }

    static String norm(String cls, String m, int line) {
        String c = cls.replaceAll("\\$\\$Lambda.*", "\\$\\$Lambda");
        String n = m.startsWith("lambda$") ? "lambda" : m;
        return c.substring(c.lastIndexOf('.') + 1) + "." + n + ":" + line;
    }

    static String fromThrowable(int skip, int n) {
        StackTraceElement[] st = new Throwable().getStackTrace();
        StringBuilder sb = new StringBuilder();
        for (int i = skip; i < st.length && i < skip + n; i++) {
            sb.append(i == skip ? "" : ",").append(norm(st[i].getClassName(), st[i].getMethodName(), st[i].getLineNumber()));
        }
        return sb.toString();
    }

    static String fromThread(int skip, int n) {
        StackTraceElement[] st = Thread.currentThread().getStackTrace();
        // st[0] is Thread.getStackTrace itself.
        StringBuilder sb = new StringBuilder();
        for (int i = skip + 1; i < st.length && i < skip + 1 + n; i++) {
            sb.append(i == skip + 1 ? "" : ",").append(norm(st[i].getClassName(), st[i].getMethodName(), st[i].getLineNumber()));
        }
        return sb.toString();
    }

    static String fromWalker(int skip, int n) {
        return StackWalker.getInstance().walk(s -> s.skip(skip).limit(n)
                .map(f -> norm(f.getClassName(), f.getMethodName(), f.getLineNumber())).collect(Collectors.joining(",")));
    }

    static String capture(int k) {
        // Each capture skips its helper frame and this one (whose lines differ).
        String a = fromThrowable(2, 5);
        String b = fromThread(2, 5);
        String c = fromWalker(2, 5);
        return (a.equals(b) && b.equals(c) ? "agree " : "DISAGREE ") + a + " | " + b + " | " + c + " #" + k;
    }

    static String inLambda(int k) {
        return IntStream.of(k).mapToObj(R21Fz2WalkLines2::capture).findFirst().orElse("none");
    }

    static String inLoop(int n) {
        String last = "";
        for (int i = 0; i < n; i++) {
            if (i == n - 1) {
                last = capture(i);
            }
        }
        return last;
    }

    static final class Ctor {
        final String s;

        Ctor(int k) {
            s = capture(k);
        }
    }

    static final class Clinit {
        static final String S = capture(-1);
    }

    static String over(int k) {
        return capture(k);
    }

    static String over(String k) {
        return capture(k.length());
    }

    static void rows(String tag, Tap t) {
        System.out.println("row " + tag + " tap " + t.tap(1));
        System.out.println("row " + tag + " lambda " + inLambda(2));
        System.out.println("row " + tag + " ctor " + new Ctor(3).s);
        System.out.println("row " + tag + " over " + over(4) + " || " + over("abcde"));
        System.out.println("row " + tag + " loop " + inLoop(tag.equals("hot") ? 2_000_000 : 5));
    }

    public static void main(String[] args) throws Exception {
        Tap d = new Direct();
        System.out.println("clinit " + Clinit.S);
        rows("cold", d);
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + d.tap(r & 7).length();
            if ((r & 15) == 0) {
                h = h * 31 + over(r).length() + new Ctor(r).s.length();
            }
        }
        System.out.println("warm hash " + h);
        rows("hot", d);
        // Another thread's stack, parked at a known line.
        CountDownLatch parked = new CountDownLatch(1);
        Thread other = new Thread(() -> parkHere(parked), "parker");
        other.start();
        parked.await();
        StackTraceElement[] st;
        do {
            Thread.sleep(1);
            st = other.getStackTrace();
        } while (other.getState() != Thread.State.WAITING);
        st = other.getStackTrace();
        StringBuilder sb = new StringBuilder("other");
        for (StackTraceElement e : st) {
            if (e.getClassName().startsWith("R21Fz2WalkLines2")) {
                sb.append(' ').append(norm(e.getClassName(), e.getMethodName(), e.getLineNumber()));
            }
        }
        System.out.println(sb);
        LockSupport.unpark(other);
        other.join();
        // Deopt trigger: a second Tap class on the hot capturing path.
        Tap o = new Other();
        rows("late", o);
        rows("after", d);
    }


    static void parkHere(CountDownLatch parked) {
        parkDeeper(parked, 3);
    }

    static void parkDeeper(CountDownLatch parked, int d) {
        if (d > 0) {
            parkDeeper(parked, d - 1);
            return;
        }
        parked.countDown();
        LockSupport.park();
    }
}
