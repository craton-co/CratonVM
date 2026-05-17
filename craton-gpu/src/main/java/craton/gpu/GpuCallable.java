package craton.gpu;

/**
 * A unit of work submitted to a {@link GpuExecutor} that produces a result.
 * Analogous to {@link java.util.concurrent.Callable} but specifically intended
 * for GPU-offloadable work; the implementation may execute either as a kernel
 * launch on the device or as a host-side coordinating call, depending on what
 * the underlying executor supports.
 *
 * @param <R> the type of the produced result
 */
@FunctionalInterface
public interface GpuCallable<R> {

    /**
     * Performs the computation and returns its result.
     *
     * @return the computed result
     * @throws Exception if the computation failed for any reason
     */
    R call() throws Exception;
}
