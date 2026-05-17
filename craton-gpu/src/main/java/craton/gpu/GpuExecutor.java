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
     * Submits a void-returning unit of work for asynchronous execution.
     * Identical to {@link #submit} except the work is described by a
     * {@link GpuRunnable} (no return value).
     *
     * @param task the work to launch
     * @return a future that completes when device-side work has finished
     */
    GpuFuture<Void> launch(GpuRunnable task);

    /**
     * Phase 5: explicit named-method dispatch. Bypasses lambda
     * resolution by taking the target method by class + name +
     * descriptor directly. {@code args} accepts: primitive boxed
     * scalars (Integer, Long, Float, Double), primitive arrays
     * (int[], long[], float[], double[]), and {@link GpuArray}
     * handles for device-resident data.
     *
     * <p>The method's last array parameter is treated as the
     * output sink (matching the existing Phase 1 convention).
     * After the kernel completes, its contents are copied back
     * into the original Java array — the caller can read the
     * result there once {@code future.get()} returns.</p>
     *
     * @param className  internal name, e.g. {@code "com/example/Pipeline"}
     * @param methodName e.g. {@code "vectorAdd"}
     * @param descriptor JVM descriptor, e.g. {@code "([I[I[I)V"}
     * @param args       boxed-primitive scalars + primitive arrays
     *                   + GpuArray handles
     * @param <R>        the future's result type (Void for void
     *                   kernels)
     */
    default <R> GpuFuture<R> submit(
        String className,
        String methodName,
        String descriptor,
        Object... args
    ) {
        return craton.gpu.internal.Native.submitMethod(
            handleForDispatch(),
            className,
            methodName,
            descriptor,
            args
        );
    }

    /**
     * Internal: returns the native handle this executor wraps. The
     * default {@link #submit(String, String, String, Object...)}
     * implementation needs the handle to call
     * {@link craton.gpu.internal.Native#submitMethod}. Concrete
     * impls override.
     */
    long handleForDispatch();

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
