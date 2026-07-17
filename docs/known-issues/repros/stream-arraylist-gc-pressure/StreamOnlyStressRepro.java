import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.stream.Collectors;
import java.util.stream.Stream;

/**
 * Reconstructed repro for docs/internal/fixed-suite-bugs/stream-arraylist-gc-pressure-heap-corruption-FIXED.md.
 * 24 threads x 3000 iterations; each iteration builds a fresh 30-element ArrayList<P> then:
 *   1) particles.stream().map(alloc).flatMap(alloc -> Stream.of(3 strings)).collect(toUnmodifiableList())
 *      -- expects size 90 (30 * 3); a size mismatch throws IllegalStateException("bad size N").
 *   2) particles.stream().map(alloc).filter(alloc).findFirst()
 *
 * Run with a small heap to force heavy GC pressure, e.g.:
 *   <cratonvm> --java-home <jdk25> -Xmx32m -cp <dir> StreamOnlyStressRepro
 * Compare against a larger heap:
 *   <cratonvm> --java-home <jdk25> -Xmx512m -cp <dir> StreamOnlyStressRepro
 */
public class StreamOnlyStressRepro {
    static final class P {
        final int id;
        P(int id) { this.id = id; }
    }

    public static void main(String[] args) throws Exception {
        int threads = 24;
        int iterations = 3000;
        AtomicInteger badSize = new AtomicInteger(0);
        AtomicInteger errors = new AtomicInteger(0);
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int tid = t;
            ts[t] = new Thread(() -> {
                for (int it = 0; it < iterations; it++) {
                    ArrayList<P> particles = new ArrayList<>();
                    for (int i = 0; i < 30; i++) {
                        particles.add(new P(i));
                    }
                    try {
                        List<String> collected = particles.stream()
                            .map(p -> new P(p.id * 2))
                            .flatMap(p2 -> Stream.of("a" + p2.id, "b" + p2.id, "c" + p2.id))
                            .collect(Collectors.toUnmodifiableList());
                        if (collected.size() != 90) {
                            badSize.incrementAndGet();
                            throw new IllegalStateException("bad size " + collected.size());
                        }
                    } catch (IllegalStateException e) {
                        errors.incrementAndGet();
                        System.err.println("[thread " + tid + " iter " + it + "] " + e.getMessage());
                    }

                    Optional<P> first = particles.stream()
                        .map(p -> new P(p.id + 1))
                        .filter(p2 -> p2.id % 7 == 0)
                        .findFirst();
                    if (first.isPresent() && first.get().id % 7 != 0) {
                        errors.incrementAndGet();
                        System.err.println("[thread " + tid + " iter " + it + "] bad findFirst id=" + first.get().id);
                    }
                }
            }, "stress-" + t);
        }
        long start = System.nanoTime();
        for (Thread th : ts) th.start();
        for (Thread th : ts) th.join();
        long elapsedMs = (System.nanoTime() - start) / 1_000_000;
        System.out.println("DONE elapsed_ms=" + elapsedMs + " badSize=" + badSize.get() + " errors=" + errors.get());
        if (errors.get() > 0) {
            System.out.println("RESULT=FAIL");
            System.exit(1);
        } else {
            System.out.println("RESULT=OK");
        }
    }
}
