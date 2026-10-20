// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;

/**
 * gci i30/ob -- G1's allocation-failure full collection under
 * {@code -XX:+ExplicitGCInvokesConcurrent} ({@code i28-dg-report.md} review
 * item 1). HotSpot 25 runs it stop-the-world ({@code VM_G1CollectFull})
 * whatever the flag says; CratonVM's last-ditch cycle followed the flag (a
 * concurrent cycle). The probe fills the heap with 1 MiB arrays until
 * {@code OutOfMemoryError}, drops them and allocates again (the heap must be
 * reusable after the last-ditch collection). It never calls System.gc().
 *
 * <pre>
 *   java     -XX:+UseG1GC -XX:+ExplicitGCInvokesConcurrent -Xmx64m GciOb30LastDitchProbe
 *   cratonvm -XX:+UseG1GC -XX:+ExplicitGCInvokesConcurrent -Xmx64m GciOb30LastDitchProbe
 * </pre>
 *
 * Expected stdout (deterministic): {@code oome true}, {@code reuse true},
 * {@code done}. stderr: {@code [ob30] held=<n> reused=<n>}.
 */
public class GciOb30LastDitchProbe {
    public static void main(String[] args) {
        List<byte[]> fill = new ArrayList<>();
        boolean oome = false;
        try {
            while (true) {
                fill.add(new byte[1 << 20]);
            }
        } catch (OutOfMemoryError e) {
            oome = true;
        }
        int held = fill.size();
        fill = null;
        List<byte[]> again = new ArrayList<>();
        for (int i = 0; i < held / 2; i++) {
            again.add(new byte[1 << 20]);
        }
        boolean reuse = again.size() == held / 2 && held > 0;
        System.err.println("[ob30] held=" + held + " reused=" + again.size());
        System.out.println("oome " + oome);
        System.out.println("reuse " + reuse);
        System.out.println("done");
    }
}
