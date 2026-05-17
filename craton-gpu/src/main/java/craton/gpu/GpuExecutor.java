package craton.gpu;

/**
 * Primary entry point for submitting work to a CUDA-capable GPU device from
 * Java code. A {@code GpuExecutor} owns the lifetime of a CUDA context bound to
 * a specific device ordinal: it is acquired via one of the {@link #open()}
 * factories, used to submit {@link GpuCallable} tasks or launch precompiled
 * kernels through {@link GpuStream}s, and must be released via {@link #close()}.
 * Implementations are expected to be thread-safe; the native context handle is
 * resolved by {@code craton.gpu.internal.Native#openExecutor(int)}.
 */
public interface GpuExecutor extends AutoCloseable {

    /**
     * Opens a {@code GpuExecutor} bound to the default device (ordinal 0).
     * Equivalent to {@code open(0)}.
     *
     * @return a new executor bound to device 0
     */
    static GpuExecutor open() {
        return open(0);
    }

    /**
     * Opens a {@code GpuExecutor} bound to the device with the given ordinal.
     * Delegates to the native bridge which performs CUDA context creation.
     *
     * @param deviceOrdinal zero-based CUDA device index
     * @return a new executor bound to the requested device
     */
    static GpuExecutor open(int deviceOrdinal) {
        return craton.gpu.internal.Native.openExecutor(deviceOrdinal);
    }

    /**
     * Submits a host-callable unit of work for asynchronous execution on this
     * executor's device. The returned future completes when device-side work
     * for the task has finished.
     *
     * @param task the work to submit
     * @param <R>  the type of the task's result
     * @return a future representing pending completion of the task
     */
    <R> GpuFuture<R> submit(GpuCallable<R> task);

    /**
     * Launches a precompiled kernel by name on the given stream with the
     * supplied launch arguments. Argument marshalling is implementation
     * defined; primitive values and {@link GpuArray} handles are supported.
     *
     * @param kernelName the symbolic name of the entry point to launch
     * @param stream     the stream on which to enqueue the launch
     * @param args       launch arguments (grid/block configuration plus kernel
     *                   parameters)
     * @return a future representing pending completion of the launch
     */
    GpuFuture<Void> launch(String kernelName, GpuStream stream, Object... args);

    /**
     * Allocates a new asynchronous stream on this executor's device. Streams
     * own no host resources besides the native handle and must be closed when
     * no longer needed.
     *
     * @return a freshly created stream
     */
    GpuStream newStream();

    /**
     * Releases the underlying CUDA context and all resources owned by this
     * executor. Calling {@code close()} on an already-closed executor is a
     * no-op.
     */
    @Override
    void close();
}
