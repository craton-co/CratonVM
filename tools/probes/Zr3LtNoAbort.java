// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * zr z3/lt: fills a ZGC heap from a COMPILED boxing loop, where the only
 * allocation in the loop is the wrapper {@code Integer.valueOf} /
 * {@code Long.valueOf} makes beyond the box cache. Before z3/lt the JIT
 * boxing helpers' cold arm (direct TLAB bump missed) called the infallible
 * {@code NativeContext::alloc_object}, which on a full ZGC heap ended in
 * {@code GarbageCollector::alloc_object}'s {@code abort()}; HotSpot collects
 * and throws {@code OutOfMemoryError: Java heap space}.
 *
 * <p>Usage: {@code Zr3LtNoAbort [int|long|mixed] [rounds] [slots]}. Each round
 * prints {@code OOME caught <mode>} (or {@code no OOME}, which means the slot
 * array was big enough to hold every box -- raise {@code slots} or lower
 * {@code -Xmx}); the last line proves the VM recovered and is deterministic.
 * Run at {@code -Xmx64m}. The stdout must equal HotSpot's
 * ({@code java -XX:+UseZGC -Xmx64m Zr3LtNoAbort int 3}).
 */
public class Zr3LtNoAbort {
    static Object[] keep;

    static Integer boxI(int v) {
        return Integer.valueOf(v);
    }

    static Long boxL(long v) {
        return Long.valueOf(v);
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "int";
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        int slots = args.length > 2 ? Integer.parseInt(args[2]) : 6_000_000;

        // Warm-up: get boxI / boxL and their Integer/Long.valueOf call sites
        // compiled before the heap is anywhere near full, so the fill below
        // allocates through the JIT boxing helpers, not the interpreter.
        long warm = 0;
        for (int i = 0; i < 400_000; i++) {
            warm += boxI(100_000 + (i & 4095)).intValue();
            warm += boxL(100_000L + (i & 4095)).longValue();
        }
        System.out.println("warm " + (warm != 0));

        for (int r = 0; r < rounds; r++) {
            String m = mode.equals("mixed") ? ((r & 1) == 0 ? "int" : "long") : mode;
            try {
                fill(m, slots);
                System.out.println("no OOME " + m);
            } catch (OutOfMemoryError e) {
                keep = null;
                System.out.println("OOME caught " + m);
            }
        }

        // Recovery: the VM is alive and the boxes are gone.
        long s = 0;
        for (int i = 0; i < 1_000_000; i++) {
            s += boxI(1000 + (i & 1023)).intValue();
        }
        System.out.println("recovered " + s);
    }

    static void fill(String m, int slots) {
        Object[] a = new Object[slots];
        keep = a;
        if (m.equals("long")) {
            long v = 1L << 40;
            for (int i = 0; i < a.length; i++) {
                a[i] = boxL(v++);
            }
        } else {
            int v = 1 << 20;
            for (int i = 0; i < a.length; i++) {
                a[i] = boxI(v++);
            }
        }
    }
}
