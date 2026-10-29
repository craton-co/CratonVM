// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.reflect.Field;
import java.util.concurrent.atomic.AtomicInteger;

final class AtomicProtectedOsrProbe {
    private static final AtomicInteger next = new AtomicInteger();
    static int finallyCount;
    static final class Discarded {
        final int index;
        Discarded(AtomicInteger counter) { index = counter.getAndIncrement(); }
    }
    static int protectedLoop(int count, boolean fail) throws Exception {
        Field field = AtomicProtectedOsrProbe.class.getDeclaredField("next");
        field.setAccessible(true);
        AtomicInteger counter = (AtomicInteger) field.get(null);
        int saved = counter.get();
        int completed = 0;
        char[] rooted = new char[] {'l', 'i', 'v', 'e'};
        try {
            while (counter.get() < count) {
                new Discarded(counter);
                completed++;
                if (completed == count / 2) {
                    System.gc();
                    if (fail) counter = null;
                }
            }
            if (rooted[0] != 'l' || rooted[3] != 'e') throw new AssertionError("root after gc");
        } catch (NullPointerException expected) {
            if (!fail || completed != count / 2 || rooted[0] != 'l' || rooted[3] != 'e') {
                throw new AssertionError("guard continuation");
            }
        } finally {
            next.set(saved);
            finallyCount++;
        }
        if (next.get() != saved) throw new AssertionError("restore");
        return completed;
    }
    public static void main(String[] args) throws Exception {
        // The loop runs only once per shape; its hot back-edge, not method
        // invocations, must install the optimizing artifact.
        if (protectedLoop(200000, false) != 200000) throw new AssertionError("complete");
        if (protectedLoop(200000, true) != 100000) throw new AssertionError("caught");
        if (finallyCount != 2) throw new AssertionError("finally replay");
        System.out.println("ATOMIC-PROTECTED-OSR-OK");
    }
}
