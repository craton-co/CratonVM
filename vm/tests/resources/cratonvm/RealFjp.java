package cratonvm;

import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;

/**
 * Smoke test for the CRATONVM_REAL_FORKJOINPOOL real-path gate.
 *
 * Run under CRATONVM_REAL_FORKJOINPOOL=1 to verify that ForkJoinPool runs
 * real JDK bytecode instead of the synthetic overlay. The test submits a
 * trivial callable to the common pool and waits for the result.
 */
public class RealFjp {
    public static void main(String[] args) throws Exception {
        ForkJoinPool pool = ForkJoinPool.commonPool();
        Future<Integer> f = pool.submit(() -> 42);
        int result = f.get(10, TimeUnit.SECONDS);
        System.out.println("r:result=" + result);
        System.out.println("r:parallelism=" + (pool.getParallelism() > 0));
        // Second task to verify queueing works.
        int sum = ForkJoinPool.commonPool().submit(() -> 100 + 23).get(10, TimeUnit.SECONDS);
        System.out.println("r:sum=" + sum);
        System.out.println("REAL_FJP_OK 3");
    }
}
