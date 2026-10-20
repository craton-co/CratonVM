// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 16, lane review-fuzz5: two hidden classes defined from the
// SAME bytes, each with an OSR-compiled static loop that calls its own private
// static and instance methods, throws and catches inside the loop, and keeps
// its own statics; then the ordinary class of the same name is loaded from the
// class path and the hidden loops run again.
//
// Risk targeted: the wave-14 hidden self-reference routes
// (`CRATONVM_JIT_HIDDEN_SELF_BY_ID`: the compiled static tail, the bail of
// `jit_invoke_dispatch`, the by-name callee guard, `class_resolved_without_loading`),
// the lock-free hidden-class bitmap of wave 15 (`HiddenClassBits`, the
// prefilter `jit_hidden_self_owner` asks first), and the verifier's hidden
// self name (`CRATONVM_VERIFY_HIDDEN_SELF_NAME`). Once the ordinary
// `R21Fz5HoPayload` is loaded, any by-name answer for the hidden classes' own
// constant-pool name finds that third class and its statics: the per-class
// counts below then move between `h1`, `h2` and `plain`. The hidden instances
// also sit at a megamorphic `IntUnaryOperator` call site beside a lambda (itself
// a hidden class), an anonymous class and the ordinary class.
//
// Every count is a pure function of the calls made: `loop(n)` adds n to its
// class's `sCount` and `applyAsInt` adds 1.
//
// Passes: `h-cold`, `h-hot` (the OSR loops, 300k trips each), `mega` (the
// operator site, 200k calls), `plain` (the ordinary class loads and runs once),
// `h-after` and `mega-after` (the same again, with the name now taken), `final`.
//
// Run:
//   javac -d out R21Fz5HiddenOsr1.java
//   java -Xmx256m -cp out R21Fz5HiddenOsr1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz5HiddenOsr1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz5HiddenOsr1
//   cratonvm --nojit -Xmx256m -cp out R21Fz5HiddenOsr1
//   CRATONVM_JIT_HIDDEN_SELF_BY_ID=0 cratonvm ...               # control arm (expected to differ)
// HotSpot expectation: every line identical on every arm except the control.

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.IntUnaryOperator;

public class R21Fz5HiddenOsr1 {
    static byte[] payloadBytes() throws Exception {
        try (InputStream in = R21Fz5HiddenOsr1.class.getResourceAsStream("/R21Fz5HoPayload.class")) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int n;
            while ((n = in.read(buf)) > 0) {
                out.write(buf, 0, n);
            }
            return out.toByteArray();
        }
    }

    static final class Hidden {
        final String tag;
        final Class<?> cls;
        final MethodHandle loop;
        final MethodHandle count;
        final MethodHandle acc;
        final IntUnaryOperator op;

        Hidden(String tag, MethodHandles.Lookup lookup, byte[] bytes) throws Throwable {
            this.tag = tag;
            this.cls = lookup.defineHiddenClass(bytes, true).lookupClass();
            this.loop = lookup.findStatic(cls, "loop", MethodType.methodType(long.class, int.class));
            this.count = lookup.findStatic(cls, "count", MethodType.methodType(int.class));
            this.acc = lookup.findStatic(cls, "acc", MethodType.methodType(long.class));
            this.op = (IntUnaryOperator) cls.getDeclaredConstructor().newInstance();
        }

        long runLoop(int n) throws Throwable {
            return (long) loop.invokeExact(n);
        }

        String facts() throws Throwable {
            int c = (int) count.invokeExact();
            long a = (long) acc.invokeExact();
            return tag + " count=" + c + " acc=" + a + " hidden=" + cls.isHidden()
                    + " simple=" + cls.getSimpleName().replaceAll("/0x[0-9a-fA-F]+", "/0x#");
        }
    }

    static long mega(IntUnaryOperator[] ops, int n) {
        long h = 0;
        for (int i = 0; i < n; i++) {
            h = h * 31 + ops[i % ops.length].applyAsInt(i & 0xffff);
        }
        return h;
    }

    // The first mention of the ordinary class: loads it from the class path.
    static String plainArm() {
        long r = R21Fz5HoPayload.loop(1000);
        IntUnaryOperator op = new R21Fz5HoPayload();
        int x = op.applyAsInt(5);
        return "plain loop=" + r + " op=" + x + " count=" + R21Fz5HoPayload.count() + " acc=" + R21Fz5HoPayload.acc();
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        byte[] bytes = payloadBytes();
        Hidden h1 = new Hidden("h1", lookup, bytes);
        Hidden h2 = new Hidden("h2", lookup, bytes);
        System.out.println("same=" + (h1.cls == h2.cls));

        System.out.println("h-cold " + h1.runLoop(5000) + " " + h2.runLoop(7000));
        System.out.println("  " + h1.facts());
        System.out.println("  " + h2.facts());

        long a = 0, b = 0;
        for (int r = 0; r < 3; r++) {
            a = a * 31 + h1.runLoop(300_000);
            b = b * 31 + h2.runLoop(100_000 + r);
        }
        System.out.println("h-hot " + a + " " + b);
        System.out.println("  " + h1.facts());
        System.out.println("  " + h2.facts());

        IntUnaryOperator lambda = x -> x * 3 + 1;
        IntUnaryOperator anon = new IntUnaryOperator() {
            @Override
            public int applyAsInt(int x) {
                return x ^ 0x5a5a;
            }
        };
        IntUnaryOperator[] ops = {h1.op, lambda, h2.op, anon, h1.op};
        System.out.println("mega " + mega(ops, 200_000));
        System.out.println("  " + h1.facts());
        System.out.println("  " + h2.facts());

        System.out.println(plainArm());

        a = 0;
        b = 0;
        for (int r = 0; r < 3; r++) {
            a = a * 31 + h1.runLoop(300_000);
            b = b * 31 + h2.runLoop(100_000 + r);
        }
        System.out.println("h-after " + a + " " + b);
        IntUnaryOperator[] ops2 = {h1.op, lambda, h2.op, anon, new R21Fz5HoPayload(), h1.op};
        System.out.println("mega-after " + mega(ops2, 200_000));
        System.out.println("final");
        System.out.println("  " + h1.facts());
        System.out.println("  " + h2.facts());
        System.out.println("  plain count=" + R21Fz5HoPayload.count() + " acc=" + R21Fz5HoPayload.acc());
    }
}

// Defined twice as a hidden class from its own class-file bytes, and once
// more, later, as an ordinary class by the application class loader. Its own
// type appears in no field or method descriptor and in no array type, so the
// hidden copies never resolve their own name except as `this_class`.
class R21Fz5HoPayload implements java.util.function.IntUnaryOperator {
    static int sCount;
    static long sAcc;
    private int bias;

    public R21Fz5HoPayload() {
        bias = 3;
    }

    private static int step(int x) {
        sCount++;
        return x * 31 + 7;
    }

    private int priv(int x) {
        return x ^ bias;
    }

    @Override
    public int applyAsInt(int x) {
        return priv(step(x)) - 1;
    }

    private static void boom(int i) {
        throw new IllegalArgumentException("boom" + i);
    }

    public static long loop(int n) {
        long h = 0;
        for (int i = 0; i < n; i++) {
            h = h * 1_000_003L + step(i);
            if ((i & 4095) == 4095) {
                try {
                    boom(i);
                } catch (IllegalArgumentException e) {
                    h ^= e.getMessage().length();
                }
            }
        }
        sAcc += h;
        return h;
    }

    public static int count() {
        return sCount;
    }

    public static long acc() {
        return sAcc;
    }
}
