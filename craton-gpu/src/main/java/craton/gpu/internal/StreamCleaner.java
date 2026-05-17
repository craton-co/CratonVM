package craton.gpu.internal;

import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Background cleaner that releases native handles owned by GpuExecutor,
 * GpuFuture, GpuStream, GpuArray when the Java wrappers become
 * unreachable. Mirrors the standard java.lang.ref cleanup pattern.
 */
public final class StreamCleaner {
    public enum ResourceKind { EXECUTOR, FUTURE, ARRAY, STREAM }

    private static final ReferenceQueue<Object> QUEUE = new ReferenceQueue<>();
    private static final ConcurrentHashMap<PhantomReference<?>, Long> HANDLES = new ConcurrentHashMap<>();
    private static final ConcurrentHashMap<PhantomReference<?>, ResourceKind> KINDS = new ConcurrentHashMap<>();

    private static final Thread WORKER;
    static {
        WORKER = new Thread(StreamCleaner::run, "craton-gpu-cleaner");
        WORKER.setDaemon(true);
        WORKER.start();
    }

    /** Register {@code owner} so that when it becomes unreachable, the
     * given native handle is released. */
    public static void register(Object owner, long handle, ResourceKind kind) {
        PhantomReference<Object> ref = new PhantomReference<>(owner, QUEUE);
        HANDLES.put(ref, handle);
        KINDS.put(ref, kind);
    }

    private static void run() {
        while (true) {
            try {
                Reference<?> ref = QUEUE.remove();
                Long handle = HANDLES.remove(ref);
                ResourceKind kind = KINDS.remove(ref);
                if (handle == null || kind == null) continue;
                switch (kind) {
                    case EXECUTOR: Native.releaseExecutor(handle); break;
                    case FUTURE:   Native.releaseFuture(handle); break;
                    case ARRAY:    Native.releaseArray(handle); break;
                    case STREAM:   Native.closeStream(handle); break;
                }
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return;
            } catch (Throwable t) {
                // Don't let one bad release kill the cleaner.
            }
        }
    }

    private StreamCleaner() {}
}
