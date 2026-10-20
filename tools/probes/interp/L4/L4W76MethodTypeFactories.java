// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 76, lane L4 -- three default-off switches:
//   CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE=1 (proposal
//     `i75-L4-proposal-compatible-method-type-factories-yield-to-bytecode-20261108`):
//     over a real JDK, `--compatible` registers no `MethodType.methodType`
//     bridges, so the four factories run the JDK's bytecode (one `makeImpl`)
//     and a bad component throws from Java with the JDK's frames;
//   CRATONVM_NATIVE_OWN_BODY_HINT=1 and
//   CRATONVM_EXECUTE_NO_JIT_GATE_WITHOUT_JIT=1 (proposal
//     `i75-L4-proposal-a-native-re-entry-into-its-own-body-by-resolved-method-20261108`):
//     the `makeImpl` memo's miss re-enters the JDK body by its declared index,
//     and `interpreter::execute` skips its JIT gate block in a VM without a
//     JIT. Neither may move any answer.
//
// Rows: every factory overload answers the interned instance
// (`fromMethodDescriptorString`'s), across a stream of types never seen
// before (memo misses: the re-entry path) and their repeats (memo hits); a
// `null` component and a `void` parameter throw what HotSpot throws, with the
// `makeImpl` frame where the JDK's body raises it. Each row runs three times
// and prints `<answer> x3` when the three agree.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// one: true x3
// two: true x3
// varargs: true x3
// array: true x3
// array-empty: true x3
// stream-fresh: 512 x3
// stream-repeat: 512 x3
// npe-rtype-only: java.lang.NullPointerException makeImpl=1 caller=methodType x3
// npe-array-elem: java.lang.NullPointerException makeImpl=1 caller=methodType x3
// npe-varargs-elem: java.lang.NullPointerException makeImpl=1 caller=methodType x3
// npe-varargs-array: java.lang.NullPointerException makeImpl=0 top=methodType x3
// iae-void-array: java.lang.IllegalArgumentException: parameter type cannot be void x3
// iae-void-varargs: java.lang.IllegalArgumentException: parameter type cannot be void x3
// after-errors: true x3
// HOTSPOT_EXPECTED_END
//
// Switch sets, `--compatible` and `--jdk-only`, each with `--nojit` too:
//   A: (none)
//   B: CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE=1
//   C: CRATONVM_NATIVE_OWN_BODY_HINT=1 CRATONVM_EXECUTE_NO_JIT_GATE_WITHOUT_JIT=1
// Expected: `--jdk-only` SAME in every set; `--compatible` set B SAME (the
// `npe-*` rows are the ones the bridges get wrong in set A).
//
// SETUP: none; `java|cratonvm [--compatible|--jdk-only] [--nojit] -cp . L4W76MethodTypeFactories`.
import java.lang.invoke.MethodType;
import java.lang.reflect.Array;
import java.util.ArrayList;
import java.util.List;

public class L4W76MethodTypeFactories {
    public static class Box {
        public int v = 3;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static String once(Row r) {
        try {
            return String.valueOf(r.run());
        } catch (Throwable t) {
            return describe(t);
        }
    }

    static String describe(Throwable t) {
        if (t instanceof NullPointerException) {
            int makeImpl = 0;
            String caller = "?";
            StackTraceElement[] st = t.getStackTrace();
            for (int i = 0; i < st.length; i++) {
                if (st[i].getClassName().equals("java.lang.invoke.MethodType")
                        && st[i].getMethodName().equals("makeImpl")) {
                    makeImpl++;
                    if (i + 1 < st.length) caller = st[i + 1].getMethodName();
                }
            }
            if (makeImpl == 0) {
                String top = st.length > 0 && st[0].getClassName().equals("java.lang.invoke.MethodType")
                        ? st[0].getMethodName() : "?";
                return t.getClass().getName() + " makeImpl=0 top=" + top;
            }
            return t.getClass().getName() + " makeImpl=" + makeImpl + " caller=" + caller;
        }
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static void row(String name, Row r) {
        List<String> answers = new ArrayList<>();
        for (int i = 0; i < 3; i++) answers.add(once(r));
        boolean same = answers.get(0).equals(answers.get(1)) && answers.get(1).equals(answers.get(2));
        System.out.println(name + ": " + (same ? answers.get(0) + " x3" : String.join(" | ", answers)));
    }

    static int streamBase = 0;

    public static void main(String[] args) throws Throwable {
        ClassLoader loader = L4W76MethodTypeFactories.class.getClassLoader();
        String box = "LL4W76MethodTypeFactories$Box;";

        row("one", () -> MethodType.methodType(int.class)
                == MethodType.fromMethodDescriptorString("()I", loader));
        row("two", () -> MethodType.methodType(int.class, Box.class)
                == MethodType.fromMethodDescriptorString("(" + box + ")I", loader));
        row("varargs", () -> MethodType.methodType(void.class, Box.class, long.class, String.class)
                == MethodType.fromMethodDescriptorString("(" + box + "JLjava/lang/String;)V", loader));
        row("array", () -> MethodType.methodType(long.class, new Class<?>[] {int.class, Box.class})
                == MethodType.fromMethodDescriptorString("(I" + box + ")J", loader));
        row("array-empty", () -> MethodType.methodType(long.class, new Class<?>[0])
                == MethodType.methodType(long.class));

        // 16 int-array and 16 long-array classes, 1..16 dimensions.
        Class<?>[] pool = new Class<?>[32];
        for (int d = 1; d <= 16; d++) {
            pool[d - 1] = Array.newInstance(int.class, new int[d]).getClass();
            pool[d + 15] = Array.newInstance(long.class, new int[d]).getClass();
        }
        // Each run of `stream-fresh` interns 512 types never seen before (the
        // memo's miss path) and checks each against its descriptor-built twin;
        // `stream-repeat` re-asks the last run's types (hits).
        row("stream-fresh", () -> {
            int ok = 0;
            int base = streamBase;
            streamBase += 512;
            for (int i = 0; i < 512; i++) {
                int c = base + i;
                Class<?> a = pool[c & 31], b = pool[(c >> 5) & 31], d = pool[(c >> 10) & 31];
                MethodType mt = MethodType.methodType(void.class, a, b, d);
                String desc = "(" + a.descriptorString() + b.descriptorString() + d.descriptorString() + ")V";
                if (mt == MethodType.fromMethodDescriptorString(desc, loader)) ok++;
            }
            return ok;
        });
        row("stream-repeat", () -> {
            int ok = 0;
            int base = streamBase - 512;
            for (int i = 0; i < 512; i++) {
                int c = base + i;
                Class<?> a = pool[c & 31], b = pool[(c >> 5) & 31], d = pool[(c >> 10) & 31];
                if (MethodType.methodType(void.class, a, b, d) == MethodType.methodType(void.class, new Class<?>[] {a, b, d})) ok++;
            }
            return ok;
        });

        row("npe-rtype-only", () -> MethodType.methodType((Class<?>) null));
        row("npe-array-elem", () -> MethodType.methodType(int.class, new Class<?>[] {null}));
        row("npe-varargs-elem", () -> MethodType.methodType(int.class, String.class, (Class<?>) null));
        row("npe-varargs-array", () -> MethodType.methodType(int.class, String.class, (Class<?>[]) null));
        row("iae-void-array", () -> MethodType.methodType(int.class, new Class<?>[] {void.class}));
        row("iae-void-varargs", () -> MethodType.methodType(int.class, String.class, void.class));
        row("after-errors", () -> MethodType.methodType(int.class, String.class, Box.class)
                == MethodType.methodType(int.class, new Class<?>[] {String.class, Box.class}));
    }
}
