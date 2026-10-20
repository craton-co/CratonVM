// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 23, lane fuzz9 (free choice): hidden-class UNLOAD churn
// next to live, hot hidden classes of the SAME name. Two "keeper" hidden
// classes (strongly reachable all along) and, per generation, 8 ephemeral
// ones (defined without ClassOption.STRONG, used, dropped, System.gc()) are
// all defined from the same bytes (this_class R21Fz9HU). Every one refers to
// its own name: invokestatic of its own methods (recursion), getstatic of its
// own K, `new R21Fz9HU` + checkcast to its own name. At generation 3 the
// ORDINARY class R21Fz9HU (same name, app loader) is loaded too. After each
// generation the keepers must still bind every self reference to themselves
// (their own K), and their frames must be reported (StackWalker
// SHOW_HIDDEN_FRAMES) or hidden (default StackWalker, Throwable stack traces)
// exactly as HotSpot does, including from compiled, inlined frames.
//
// Risk targeted: wave 22 "recorded hidden class-file names on ClassStore
// pruned at unload" (pruning keyed by name would drop a LIVE keeper's record)
// and "one self-name answer for doors / dispatch_static / unwinder"
// (CRATONVM_HIDDEN_SELF_RECORDED_NAME, CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS, the
// opt-in CRATONVM_JIT_HIDDEN_SELF_NAME_PREFILTER).
//
// Hidden names are printed with "/0x<hex>" replaced by "/0xH". Whether and
// when the ephemeral classes are unloaded is never printed.
//
// Run:
//   javac -d out R21Fz9HiddenUnload1.java
//   java -Xmx256m -cp out R21Fz9HiddenUnload1              # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz9HiddenUnload1          # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz9HiddenUnload1
//   cratonvm --nojit -Xmx256m -cp out R21Fz9HiddenUnload1
//   CRATONVM_JIT_HIDDEN_SELF_NAME_PREFILTER=1 cratonvm -Xmx256m -cp out R21Fz9HiddenUnload1
//
// HotSpot 25 expectation (JEP 371: methods of hidden classes are hidden frames;
// not yet run):
//   keeper name=R21Fz9HU/0xH hidden=true
//   walkH R21Fz9HiddenUnload1.frames R21Fz9HU/0xH.walk R21Fz9HU/0xH.walk R21Fz9HU/0xH.walk R21Fz9HiddenUnload1.callWalk R21Fz9HiddenUnload1.generation R21Fz9HiddenUnload1.main
//   walk  R21Fz9HiddenUnload1.frames R21Fz9HiddenUnload1.callWalk R21Fz9HiddenUnload1.generation R21Fz9HiddenUnload1.main
//   dive  R21Fz9HiddenUnload1.callDive R21Fz9HiddenUnload1.dives R21Fz9HiddenUnload1.generation R21Fz9HiddenUnload1.main
//   (the walkH / walk / dive lines are printed once, in generation 0)
//   gen 0..5: k0=true k1=true eph=true walkH=same walk=same dive=2000/2000 2000/2000
//   normal run=12 (printed before "gen 3": the ordinary class, K=7, run(5) = 7 + 5)
//   final k0=true k1=true

import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.List;
import java.util.stream.Collectors;

public class R21Fz9HiddenUnload1 {
    static final int HOT = 20_000;
    static final int EPH_CALLS = 2_000;
    static final int DIVES = 2_000;
    static final int GENS = 6;
    static final MethodType RUN = MethodType.methodType(int.class, int.class);
    static final MethodType WALK = MethodType.methodType(String.class, int.class, boolean.class);

    static byte[] bytes;
    static String walkH0;
    static String walk0;
    static String dive0;

    static String norm(String s) {
        return s.replaceAll("/0x[0-9a-fA-F]+", "/0xH");
    }

    // Called from the hidden classes' walk().
    static String frames(boolean hidden) {
        StackWalker w = hidden
                ? StackWalker.getInstance(StackWalker.Option.SHOW_HIDDEN_FRAMES)
                : StackWalker.getInstance();
        return w.walk(s -> s.map(f -> norm(f.getClassName()) + "." + f.getMethodName())
                .filter(x -> x.startsWith("R21Fz9"))
                .collect(Collectors.joining(" ")));
    }

    static final class Keeper {
        final MethodHandles.Lookup lookup;
        final Class<?> cls;
        final int k;

        Keeper(MethodHandles.Lookup lk, int k) throws Throwable {
            lookup = lk.defineHiddenClass(bytes, true);
            cls = lookup.lookupClass();
            this.k = k;
            lookup.findStaticVarHandle(cls, "K", int.class).set(k);
        }

        MethodHandle mh(String name, MethodType t) throws ReflectiveOperationException {
            return lookup.findStatic(cls, name, t);
        }
    }

    static boolean runSum(Keeper kp, int calls) throws Throwable {
        MethodHandle run = kp.mh("run", RUN);
        long sum = 0;
        for (int n = 0; n < calls; n++) {
            sum += (int) run.invokeExact(n);
        }
        long want = (long) calls * kp.k + (long) calls * (calls - 1) / 2;
        return sum == want;
    }

    static String callWalk(Keeper kp, boolean hidden) throws Throwable {
        return (String) kp.mh("walk", WALK).invokeExact(2, hidden);
    }

    static String callDive(MethodHandle dive) throws Throwable {
        try {
            int r = (int) dive.invokeExact(3);
            return "no throw " + r;
        } catch (IllegalStateException e) {
            List<String> out = new ArrayList<>();
            for (StackTraceElement el : e.getStackTrace()) {
                String s = norm(el.getClassName()) + "." + el.getMethodName();
                if (s.startsWith("R21Fz9")) {
                    out.add(s);
                }
            }
            return String.join(" ", out);
        }
    }

    static int dives(Keeper kp) throws Throwable {
        MethodHandle dive = kp.mh("dive", RUN);
        int same = 0;
        for (int i = 0; i < DIVES; i++) {
            String s = callDive(dive);
            if (dive0 == null) {
                dive0 = s;
                System.out.println("dive  " + s);
            }
            if (s.equals(dive0)) {
                same++;
            }
        }
        return same;
    }

    static int touchNormal() {
        R21Fz9HU.K = 7;
        return R21Fz9HU.run(5);
    }

    static boolean ephemerals(MethodHandles.Lookup lk, int g) throws Throwable {
        boolean ok = true;
        for (int j = 0; j < 8; j++) {
            Keeper e = new Keeper(lk, 1000 + g * 8 + j);
            ok &= runSum(e, EPH_CALLS);
        }
        return ok;
    }

    static void generation(MethodHandles.Lookup lk, Keeper k0, Keeper k1, int g) throws Throwable {
        if (g == 3) {
            System.out.println("normal run=" + touchNormal());
        }
        boolean eph = ephemerals(lk, g);
        boolean r0 = runSum(k0, HOT);
        boolean r1 = runSum(k1, HOT);
        String wh = callWalk(k0, true);
        String w = callWalk(k1, false);
        if (walkH0 == null) {
            walkH0 = wh;
            walk0 = w;
            System.out.println("walkH " + wh);
            System.out.println("walk  " + w);
        }
        int d0 = dives(k0);
        int d1 = dives(k1);
        System.out.println("gen " + g + " k0=" + r0 + " k1=" + r1 + " eph=" + eph
                + " walkH=" + (wh.equals(walkH0) ? "same" : wh)
                + " walk=" + (w.equals(walk0) ? "same" : w)
                + " dive=" + d0 + "/" + DIVES + " " + d1 + "/" + DIVES);
        System.gc();
    }

    public static void main(String[] args) throws Throwable {
        try (InputStream in = R21Fz9HiddenUnload1.class.getResourceAsStream("/R21Fz9HU.class")) {
            bytes = in.readAllBytes();
        }
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Keeper k0 = new Keeper(lk, 100);
        Keeper k1 = new Keeper(lk, 200);
        System.out.println("keeper name=" + norm(k0.cls.getName()) + " hidden=" + k0.cls.isHidden());
        for (int g = 0; g < GENS; g++) {
            generation(lk, k0, k1, g);
        }
        System.out.println("final k0=" + runSum(k0, HOT) + " k1=" + runSum(k1, HOT));
    }
}

// The bytes of every hidden class above, and (from generation 3) an ordinary
// class of the same name. Every member refers to the class's own name.
class R21Fz9HU {
    static int K;
    int f;

    static int depth(int n) {
        return n <= 0 ? K : depth(n - 1);
    }

    int inst(int n) {
        f = n;
        return depth(n & 3) + f;
    }

    static int run(int n) {
        Object o = new R21Fz9HU();
        return ((R21Fz9HU) o).inst(n);
    }

    static int dive(int n) {
        if (n == 0) {
            throw new IllegalStateException("dive");
        }
        return dive(n - 1) + 1;
    }

    static String walk(int n, boolean hidden) {
        return n == 0 ? R21Fz9HiddenUnload1.frames(hidden) : walk(n - 1, hidden);
    }
}
