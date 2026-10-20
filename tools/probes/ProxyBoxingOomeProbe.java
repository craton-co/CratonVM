// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gc-common round 2026-09-23, wave 5, lane C5. Drives the dynamic-proxy
// dispatch path (argument boxing, the argument Object[], the synthetic Method)
// on a heap that is filling with live data, then checks that the process
// survived to catch the OutOfMemoryError and that the proxy still works
// afterwards.
//
// Before w5-c those allocations used the INFALLIBLE heap entry points: on
// exhaustion they aborted the process (Generational, ZGC) or spent G1's
// emergency reserve. Now they report a catchable OutOfMemoryError. Expected on
// every collector and on HotSpot:
//
//   ProxyBoxingOomeProbe OOM caught ... ; after-OOM proxy sum=<n> OK
//
// never a `FATAL:` line, never rc=127/134. Run it at a small heap, for example:
//
//   ./target/release/cratonvm -XX:+UseG1GC -Xmx48m -cp tools/probes ProxyBoxingOomeProbe
//
// Loop counters are `int` stepping by 1 on purpose, as elsewhere in this kit
// (see common-w4o-shadow-stack-dereferences-a-mistyped-odd-primitive).
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Proxy;
import java.util.ArrayList;
import java.util.List;

public class ProxyBoxingOomeProbe {
    public interface Adder {
        long add(int a, long b, double c, boolean d);
    }

    public static void main(String[] args) {
        InvocationHandler h = (proxy, method, a) -> {
            int x = (Integer) a[0];
            long y = (Long) a[1];
            double z = (Double) a[2];
            boolean d = (Boolean) a[3];
            return x + y + (long) z + (d ? 1L : 0L);
        };
        Adder adder = (Adder) Proxy.newProxyInstance(
                ProxyBoxingOomeProbe.class.getClassLoader(), new Class<?>[] {Adder.class}, h);

        List<Object> keep = new ArrayList<>();
        long sum = 0;
        int calls = 0;
        boolean oom = false;
        try {
            for (int r = 0; r < 2_000_000; r++) {
                keep.add(new int[64]);
                for (int i = 0; i < 8; i++) {
                    sum += adder.add(i, r, 0.5, (i & 1) == 0);
                    calls++;
                }
            }
        } catch (OutOfMemoryError e) {
            oom = true;
            int retained = keep.size();
            keep = null;
            System.out.println("ProxyBoxingOomeProbe OOM caught after " + retained
                    + " retained, " + calls + " proxy calls");
        }
        keep = null;
        long after = 0;
        for (int i = 0; i < 100; i++) {
            after += adder.add(i, 1, 1.0, true);
        }
        // sum over i of (i + 1 + 1 + 1) for i in 0..99 = 4950 + 300
        boolean ok = after == 5250L;
        System.out.println("ProxyBoxingOomeProbe " + (oom ? "" : "(no OOM) ")
                + "after-OOM proxy sum=" + after + (ok ? " OK" : " WRONG") + " checksum=" + sum);
    }
}
