package craton.gpu;

/**
 * A handle to a CUDA asynchronous stream. Streams provide ordered execution of
 * kernel launches and memory operations relative to each other; operations on
 * different streams may execute concurrently. The native CUDA stream pointer is
 * exposed via {@link #handle()} for advanced interop. Streams must be closed
 * to release the underlying native handle.
 */
public interface GpuStream extends AutoCloseable {

    /**
     * Returns the raw native CUDA stream handle (a {@code CUstream}) as a
     * 64-bit value. Intended for interop with native code; ordinary user code
     * should prefer {@link GpuExecutor#launch(String, GpuStream, Object...)}.
     *
     * @return the native stream handle
     */
    long handle();

    /**
     * Releases the underlying native CUDA stream. Calling {@code close()} on an
     * already-closed stream is a no-op.
     */
    @Override
    void close();
}
