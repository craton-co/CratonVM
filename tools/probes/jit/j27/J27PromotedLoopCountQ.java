import java.io.BufferedReader;
import java.io.FileReader;
import java.lang.management.CompilationMXBean;
import java.lang.management.ManagementFactory;

/** Quiesced main-thread instruction-count workload for the JR21 promoted-loop rows. */
public final class J27PromotedLoopCountQ {
    static int counter, other;
    static volatile long sink;
    static int scale = 3;
    static volatile R20PLoopStaticBarrier RUNNER = new R20PLoopStaticBarrier();

    static long runStatic(int n, int kk, int b) {
        int v = 0;
        long w = 0;
        for (int i = 0; i < n; i++) {
            v += kk * scale + i;
            w += b;
        }
        return ((long) v << 32) ^ w;
    }

    static int counterInt(int n) {
        R20PLoopStaticCells.counterInt(n);
        return R20PLoopStaticCells.COUNTER;
    }

    static int twoCells(int n) {
        R20PLoopStaticCells.twoCells(n);
        return R20PLoopStaticCells.COUNTER * 7 + R20PLoopStaticCells.OTHER;
    }

    static long call(int kernel, int trips) {
        return switch (kernel) {
            case 0 -> {
                R20PLoopStaticBarrier.Acc a = RUNNER.run(trips);
                yield ((long) a.v << 32) ^ a.w;
            }
            case 1 -> runStatic(trips, 17, 11);
            case 2 -> counterInt(trips);
            case 3 -> twoCells(trips);
            default -> 0;
        };
    }

    static long workRun(int trips, int calls) {
        long checksum = 0;
        for (int i = 0; i < calls; i++) {
            R20PLoopStaticBarrier.Acc a = RUNNER.run(trips);
            checksum += ((long) a.v << 32) ^ a.w;
        }
        return checksum;
    }

    static long workStatic(int trips, int calls) {
        long checksum = 0;
        for (int i = 0; i < calls; i++) checksum += runStatic(trips, 17, 11);
        return checksum;
    }

    static long workCounter(int trips, int calls) {
        long checksum = 0;
        for (int i = 0; i < calls; i++) checksum += counterInt(trips);
        return checksum;
    }

    static long workTwoCells(int trips, int calls) {
        long checksum = 0;
        for (int i = 0; i < calls; i++) checksum += twoCells(trips);
        return checksum;
    }

    static long work(int kernel, int trips, int calls) {
        return switch (kernel) {
            case 0 -> workRun(trips, calls);
            case 1 -> workStatic(trips, calls);
            case 2 -> workCounter(trips, calls);
            case 3 -> workTwoCells(trips, calls);
            default -> throw new IllegalArgumentException("unknown kernel: " + kernel);
        };
    }

    static void quiesce() throws InterruptedException {
        CompilationMXBean bean = ManagementFactory.getCompilationMXBean();
        if (bean == null || !bean.isCompilationTimeMonitoringSupported())
            throw new IllegalStateException("compilation time monitoring unavailable");
        long previous = bean.getTotalCompilationTime();
        int stable = 0;
        for (int i = 0; i < 225 && stable < 6; i++) {
            Thread.sleep(400);
            long current = bean.getTotalCompilationTime();
            stable = current == previous ? stable + 1 : 0;
            previous = current;
        }
        if (stable < 6) throw new IllegalStateException("compilation did not quiesce");
    }

    public static void main(String[] args) throws Exception {
        String row = args[0];
        String name = row.endsWith("-osr") ? row.substring(0, row.length() - 4) : row;
        int kernel = switch (name) {
            case "run" -> 0;
            case "runStatic" -> 1;
            case "counterInt" -> 2;
            case "twoCells" -> 3;
            default -> throw new IllegalArgumentException("unknown row: " + row);
        };
        int trips = Integer.parseInt(args[1]);
        int warmCalls = trips == 1_000_000 ? 12 : 20_000;
        sink = work(kernel, trips, warmCalls);
        quiesce();
        try (BufferedReader go = new BufferedReader(new FileReader(args[2]))) {
            System.out.println("ready " + ProcessHandle.current().pid() + " " + row);
            for (int window = 1; window <= 2; window++) {
                if (!"go".equals(go.readLine())) throw new IllegalStateException("missing window signal");
                int calls = window == 1 ? Integer.parseInt(args[3]) : 3 * Integer.parseInt(args[3]);
                long checksum = work(kernel, trips, calls);
                sink = checksum;
                System.out.println("window " + window + " calls " + calls + " checksum " + checksum);
            }
        }
    }
}
