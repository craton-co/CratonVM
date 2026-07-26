import reactor.core.publisher.BaseSubscriber;
import reactor.core.publisher.Flux;
import reactor.core.publisher.FluxSink;

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

// REACTOR-ADDCAP.1 / REACTOR-FLUXCREATE.1 repro: the original bug report
// describes a WebSocket-like send chain built on Flux.create(...) where
// requesting demand 1-at-a-time (driving Operators.addCap and
// FluxCreate$BaseSink.addCap / FluxCreate$BufferAsyncSink.drain repeatedly)
// stalls at 84/100 messages under JIT while the interpreter/HotSpot complete.
// This probe reproduces that shape directly against real reactor-core:
// Flux.create(...) emitting N items, a BaseSubscriber that requests exactly 1
// item at a time (forcing many addCap/drain calls per run), repeated across
// many independent pipeline instances to cross JIT invocation thresholds.
public class ReactorAddCapProbe {
    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        int itemsPerRun = args.length > 1 ? Integer.parseInt(args[1]) : 100;
        int failures = 0;

        for (int i = 0; i < iterations; i++) {
            final int n = itemsPerRun;
            AtomicInteger received = new AtomicInteger(0);
            AtomicLong lastSeen = new AtomicLong(-1);
            AtomicInteger outOfOrder = new AtomicInteger(0);
            CountDownLatch done = new CountDownLatch(1);

            Flux<Long> flux = Flux.create(sink -> {
                for (long v = 0; v < n; v++) {
                    sink.next(v);
                }
                sink.complete();
            }, FluxSink.OverflowStrategy.BUFFER);

            flux.subscribe(new BaseSubscriber<Long>() {
                @Override
                protected void hookOnSubscribe(org.reactivestreams.Subscription subscription) {
                    subscription.request(1);
                }

                @Override
                protected void hookOnNext(Long value) {
                    if (value <= lastSeen.get()) {
                        outOfOrder.incrementAndGet();
                    }
                    lastSeen.set(value);
                    received.incrementAndGet();
                    request(1);
                }

                @Override
                protected void hookOnComplete() {
                    done.countDown();
                }

                @Override
                protected void hookOnError(Throwable throwable) {
                    done.countDown();
                }
            });

            boolean completed = done.await(10, TimeUnit.SECONDS);
            if (!completed) {
                failures++;
                if (failures <= 5) {
                    System.out.println("STALL at i=" + i + " received=" + received.get() + "/" + n);
                }
            } else if (received.get() != n) {
                failures++;
                if (failures <= 5) {
                    System.out.println("COUNT MISMATCH at i=" + i + " received=" + received.get() + "/" + n);
                }
            } else if (outOfOrder.get() != 0) {
                failures++;
                if (failures <= 5) {
                    System.out.println("ORDER MISMATCH at i=" + i + " outOfOrder=" + outOfOrder.get());
                }
            }

            if (i % 25 == 0) {
                System.out.println("progress i=" + i);
                System.out.flush();
            }
        }
        System.out.println("DONE iterations=" + iterations + " failures=" + failures);
        if (failures > 0) {
            System.exit(1);
        }
    }
}
