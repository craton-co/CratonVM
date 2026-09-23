// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Spins ONE accessor for a fixed wall time so an external sampler (gdb) sees a steady state.
//   cratonvm [--jdk-only] [--add-exports java.base/jdk.internal.misc=ALL-UNNAMED] -cp . AccessorSpin <which> <seconds>
// which: heapgetint | sma | unsafe | checkindex | chmput
// Compile with: javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED AccessorSpin.java
import java.nio.ByteBuffer;
import java.util.Objects;
import java.util.concurrent.ConcurrentHashMap;
import jdk.internal.misc.ScopedMemoryAccess;
import jdk.internal.misc.Unsafe;

public class AccessorSpin {
    public static void main(String[] a) {
        String which = a[0];
        long end = System.nanoTime() + Long.parseLong(a[1]) * 1_000_000_000L;
        long s = 0;
        long calls = 0;
        ByteBuffer hb = ByteBuffer.allocate(1 << 16);
        byte[] arr = new byte[4096];
        Unsafe u = Unsafe.getUnsafe();
        ScopedMemoryAccess sma = ScopedMemoryAccess.getScopedMemoryAccess();
        long base = u.arrayBaseOffset(byte[].class);
        ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
        while (System.nanoTime() < end) {
            for (int i = 0; i < 20000; i++) {
                switch (which) {
                    case "heapgetint" -> s += hb.getInt((i & 1023) * 4);
                    case "sma" -> s += sma.getIntUnaligned(null, arr, base + ((i & 511) << 2), true);
                    case "unsafe" -> s += u.getIntUnaligned(arr, base + ((i & 511) << 2), true);
                    case "checkindex" -> s += Objects.checkIndex(i & 1023, 1024);
                    case "chmput" -> { m.put(i & 0xFFFF, i); s += m.size(); }
                    default -> throw new IllegalArgumentException(which);
                }
            }
            calls += 20000;
        }
        System.out.println(which + " calls=" + calls + " sink=" + s);
    }
}
