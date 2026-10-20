// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L6 -- probe for
// docs/internal/fixed-bugs/interpreter-i75-L6-a-short-sleep-after-an-async-submit-sleeps-3-seconds-FIXED-20261111.md:
// a Thread.sleep(50..100) within a second of an async submit (here
// CompletableFuture.runAsync) slept ~3 s (a 3 s default "worker floor"), on
// any thread of the process. HotSpot sleeps what was asked.
//
// Run, each mode (no CRATONVM_ASYNC_*_SLEEP_FLOOR_MS set):
//   cratonvm L6W79SleepAfterAsyncSubmit
//   cratonvm --compatible L6W79SleepAfterAsyncSubmit
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// main sleep(100) after runAsync: slept<1000ms=true
// main sleep(50) after supplyAsync: slept<1000ms=true
// other thread sleep(75) after runAsync: slept<1000ms=true
// main sleep(5) after runAsync: slept<1000ms=true
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.CompletableFuture;

public class L6W79SleepAfterAsyncSubmit {
    static String timed(long millis) throws InterruptedException {
        long t0 = System.nanoTime();
        Thread.sleep(millis);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        return "slept<1000ms=" + (ms < 1_000);
    }

    public static void main(String[] args) throws Exception {
        CompletableFuture.runAsync(() -> { }).join();
        System.out.println("main sleep(100) after runAsync: " + timed(100));

        CompletableFuture.supplyAsync(() -> 1).join();
        System.out.println("main sleep(50) after supplyAsync: " + timed(50));

        CompletableFuture.runAsync(() -> { }).join();
        String[] out = new String[1];
        Thread t = new Thread(() -> {
            try {
                out[0] = timed(75);
            } catch (InterruptedException e) {
                out[0] = "interrupted";
            }
        }, "w79-other");
        t.start();
        t.join();
        System.out.println("other thread sleep(75) after runAsync: " + out[0]);

        CompletableFuture.runAsync(() -> { }).join();
        System.out.println("main sleep(5) after runAsync: " + timed(5));
        System.out.println("done");
    }
}
