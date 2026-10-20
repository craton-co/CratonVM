// Interpreter round i1 wave 49, lane L1: a thread in `Thread.sleep` reads
// TIMED_WAITING for its whole sleep. CratonVM's sleep natives
// (`native_thread_sleep`, `native_thread_sleep_nanos`) left their blocking
// region between two 10 ms slices, so a sampler caught the sleeper RUNNABLE
// now and then -- and a debugger's `ThreadReference.Status` caught it RUNNING
// (`L1W47JdiBlockedBeforeAttach`'s `durationSleeper`, intermittent under
// `--compatible`); they now stay in one region for the whole sleep.
//
// Two platform threads sleep for ten minutes, one in `Thread.sleep(long)`, one
// in `Thread.sleep(Duration)`; the main thread samples `getState()` of each
// for about two seconds (the sleepers cross ~200 slice boundaries each) and
// prints the set of states it saw, then interrupts both.
//
// Expected (HotSpot 25.0.3, the Windows box; default flags):
//
//   millis states: [TIMED_WAITING]
//   duration states: [TIMED_WAITING]
//   millis interrupted: true
//   duration interrupted: true
//
// CratonVM before wave 49: a set that sometimes also held RUNNABLE (the
// window is a few microseconds per slice without a debugger, wider with
// one attached).
import java.time.Duration;
import java.util.EnumSet;
import java.util.Set;
import java.util.concurrent.CountDownLatch;

public class L1W49SleepStateStaysTimedWaiting {
    static volatile boolean millisInterrupted;
    static volatile boolean durationInterrupted;

    public static void main(String[] args) throws Exception {
        CountDownLatch started = new CountDownLatch(2);
        Thread millis = new Thread(() -> {
            started.countDown();
            try {
                Thread.sleep(600_000L);
            } catch (InterruptedException stop) {
                millisInterrupted = true;
            }
        }, "millis");
        Thread duration = new Thread(() -> {
            started.countDown();
            try {
                Thread.sleep(Duration.ofMinutes(10));
            } catch (InterruptedException stop) {
                durationInterrupted = true;
            }
        }, "duration");
        millis.start();
        duration.start();
        started.await();
        long until = System.currentTimeMillis() + 10_000;
        while ((millis.getState() != Thread.State.TIMED_WAITING
                || duration.getState() != Thread.State.TIMED_WAITING)
                && System.currentTimeMillis() < until) {
            Thread.sleep(1);
        }
        Set<Thread.State> seenMillis = EnumSet.noneOf(Thread.State.class);
        Set<Thread.State> seenDuration = EnumSet.noneOf(Thread.State.class);
        long end = System.nanoTime() + 2_000_000_000L;
        while (System.nanoTime() < end) {
            seenMillis.add(millis.getState());
            seenDuration.add(duration.getState());
        }
        System.out.println("millis states: " + seenMillis);
        System.out.println("duration states: " + seenDuration);
        millis.interrupt();
        duration.interrupt();
        millis.join(10_000);
        duration.join(10_000);
        System.out.println("millis interrupted: " + millisInterrupted);
        System.out.println("duration interrupted: " + durationInterrupted);
    }
}
