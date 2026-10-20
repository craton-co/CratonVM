// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L6 -- Thread.sleep(Duration) and
// Thread.join(Duration) at their edge values, on a platform and a virtual
// thread, against HotSpot 25 (JDK 25 `Thread.sleep(Duration)` converts with
// `NANOSECONDS.convert`, which saturates; a negative or zero duration returns
// after the interrupt check; a null one throws NullPointerException):
//  * Duration.ZERO and Duration.ofNanos(1) return;
//  * Duration.ofSeconds(Long.MAX_VALUE) (and ChronoUnit.FOREVER) sleep until
//    interrupted, then InterruptedException("sleep interrupted"), status
//    cleared;
//  * a pre-interrupted sleep(Duration.ZERO) throws, status cleared;
//  * join(Duration.ofSeconds(Long.MAX_VALUE)) on a thread that ends returns
//    true; join(Duration.ZERO) on a live thread returns false without
//    waiting; a pre-interrupted join(Duration.ofNanos(1)) on a live thread
//    throws InterruptedException (null message), status cleared.
// A virtual thread's sleep is `VirtualThread.sleepNanos`, whose
// `new InterruptedException()` has no message (a platform thread's is
// HotSpot's "sleep interrupted"). No timing in the output.
//
// HOTSPOT_EXPECTED_BEGIN
// platform zero: returned
// platform 1ns: returned
// platform null: NullPointerException
// platform pre-interrupted zero: InterruptedException sleep interrupted interrupted-after=false
// platform max-seconds: InterruptedException sleep interrupted interrupted-after=false
// platform forever: InterruptedException sleep interrupted interrupted-after=false
// platform join-max: true
// platform join-zero-live: false
// platform pre-interrupted join-1ns: InterruptedException null interrupted-after=false
// virtual zero: returned
// virtual 1ns: returned
// virtual null: NullPointerException
// virtual pre-interrupted zero: InterruptedException null interrupted-after=false
// virtual max-seconds: InterruptedException null interrupted-after=false
// virtual forever: InterruptedException null interrupted-after=false
// virtual join-max: true
// virtual join-zero-live: false
// virtual pre-interrupted join-1ns: InterruptedException null interrupted-after=false
// done
// HOTSPOT_EXPECTED_END
import java.time.Duration;
import java.time.temporal.ChronoUnit;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;

public class L6W100SleepDurationEdges {
    interface Body {
        void run() throws Exception;
    }

    static String outcome(Body b) {
        try {
            b.run();
            return "returned";
        } catch (Exception e) {
            return e.getClass().getSimpleName() + " " + e.getMessage()
                + " interrupted-after=" + Thread.currentThread().isInterrupted();
        }
    }

    /** Interrupts `target` once it is TIMED_WAITING (the long sleep). */
    static void interruptWhenSleeping(Thread target) {
        Thread.ofPlatform().daemon().start(() -> {
            while (target.getState() != Thread.State.TIMED_WAITING) {
                Thread.onSpinWait();
            }
            target.interrupt();
        });
    }

    static List<String> cases(String kind) throws Exception {
        List<String> out = new ArrayList<>();
        Thread me = Thread.currentThread();
        out.add(kind + " zero: " + outcome(() -> Thread.sleep(Duration.ZERO)));
        out.add(kind + " 1ns: " + outcome(() -> Thread.sleep(Duration.ofNanos(1))));
        try {
            Thread.sleep((Duration) null);
            out.add(kind + " null: returned");
        } catch (NullPointerException e) {
            out.add(kind + " null: NullPointerException");
        }
        out.add(kind + " pre-interrupted zero: " + outcome(() -> {
            me.interrupt();
            Thread.sleep(Duration.ZERO);
        }));
        out.add(kind + " max-seconds: " + outcome(() -> {
            interruptWhenSleeping(me);
            Thread.sleep(Duration.ofSeconds(Long.MAX_VALUE));
        }));
        out.add(kind + " forever: " + outcome(() -> {
            interruptWhenSleeping(me);
            Thread.sleep(ChronoUnit.FOREVER.getDuration());
        }));
        Thread quick = Thread.ofPlatform().start(() -> { });
        out.add(kind + " join-max: " + quick.join(Duration.ofSeconds(Long.MAX_VALUE)));
        CountDownLatch hold = new CountDownLatch(1);
        Thread live = Thread.ofPlatform().start(() -> {
            try {
                hold.await();
            } catch (InterruptedException e) {
                // not expected
            }
        });
        out.add(kind + " join-zero-live: " + live.join(Duration.ZERO));
        out.add(kind + " pre-interrupted join-1ns: " + outcome(() -> {
            me.interrupt();
            live.join(Duration.ofNanos(1));
        }));
        hold.countDown();
        live.join();
        return out;
    }

    public static void main(String[] args) throws Exception {
        cases("platform").forEach(System.out::println);
        List<String> v = new ArrayList<>();
        Thread vt = Thread.ofVirtual().start(() -> {
            try {
                v.addAll(cases("virtual"));
            } catch (Exception e) {
                v.add("virtual threw " + e);
            }
        });
        vt.join();
        v.forEach(System.out::println);
        System.out.println("done");
    }
}
