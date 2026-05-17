package craton.gpu.internal;

import craton.gpu.GpuStream;

/**
 * Package-private concrete implementation of {@link GpuStream}.
 * Created exclusively by {@link Native#newStream(long)}.
 *
 * <p>Registered with {@link StreamCleaner} so an abandoned stream
 * still gets {@link Native#closeStream(long)} called on it
 * eventually; {@link #close()} is the deterministic path.</p>
 */
final class GpuStreamImpl implements GpuStream {
    private final long handle;
    private volatile boolean closed = false;

    GpuStreamImpl(long handle) {
        this.handle = handle;
        StreamCleaner.register(this, handle, StreamCleaner.ResourceKind.STREAM);
    }

    @Override
    public long handle() {
        return handle;
    }

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        Native.closeStream(handle);
    }
}
