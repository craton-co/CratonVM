package craton.gpu.internal;

import craton.gpu.GpuCallable;
import craton.gpu.GpuException;
import craton.gpu.GpuExecutor;
import craton.gpu.GpuFuture;
import craton.gpu.GpuRunnable;
import craton.gpu.GpuStream;

/**
 * Package-private concrete implementation of {@link GpuExecutor}.
 * Instances are created exclusively by {@link Native#openExecutor(int)}
 * and identified by a Rust-side native handle.
 *
 * <p>Lifecycle: registered with {@link StreamCleaner} on construction
 * so an abandoned executor still gets its native resources released
 * by the cleaner thread. {@link #close()} is idempotent; calling it
 * is preferred to relying on the cleaner.</p>
 */
final class GpuExecutorImpl implements GpuExecutor {
    private final long handle;
    private volatile boolean closed = false;

    /** Called only from {@link Native#openExecutor(int)} — package-private. */
    GpuExecutorImpl(long handle) {
        this.handle = handle;
        StreamCleaner.register(this, handle, StreamCleaner.ResourceKind.EXECUTOR);
    }

    @Override
    public <R> GpuFuture<R> submit(GpuCallable<R> task) {
        if (closed) throw new GpuException("executor is closed");
        return Native.submit(handle, task);
    }

    @Override
    public GpuFuture<Void> launch(GpuRunnable task) {
        if (closed) throw new GpuException("executor is closed");
        return Native.launch(handle, task);
    }

    @Override
    public GpuStream newStream() {
        if (closed) throw new GpuException("executor is closed");
        return Native.newStream(handle);
    }

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        Native.releaseExecutor(handle);
    }

    long handle() {
        return handle;
    }

    @Override
    public long handleForDispatch() {
        if (closed) throw new GpuException("executor is closed");
        return handle;
    }
}
