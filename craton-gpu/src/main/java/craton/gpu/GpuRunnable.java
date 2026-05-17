package craton.gpu;

/**
 * A unit of GPU-offloadable work that produces no result. Analogous to
 * {@link Runnable} but permitted to throw checked exceptions so that
 * device-side failures propagate naturally back to the host caller through
 * {@link GpuFuture#get()}.
 */
@FunctionalInterface
public interface GpuRunnable {

    /**
     * Performs the work.
     *
     * @throws Exception if the work failed for any reason
     */
    void run() throws Exception;
}
