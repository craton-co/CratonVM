package craton.gpu.internal;

import craton.gpu.GpuException;
import craton.gpu.GpuFunction;
import craton.gpu.GpuFuture;

import java.util.concurrent.CompletableFuture;

/**
 * Package-private concrete implementation of {@link GpuFuture}.
 * Instances are created by {@link Native#submit(long, craton.gpu.GpuCallable)}
 * and {@link Native#launch(long, craton.gpu.GpuRunnable)}; both return a
 * native submission handle that this wrapper queries via the
 * {@code futureStatus} / {@code futureGetResult} / {@code futureGetErrorMessage}
 * trio.
 *
 * <p>Status codes from {@link Native#futureStatus(long)}:
 * <ul>
 *   <li>0 — running</li>
 *   <li>1 — completed</li>
 *   <li>2 — failed (call {@link Native#futureGetErrorMessage(long)})</li>
 * </ul></p>
 */
final class GpuFutureImpl<T> implements GpuFuture<T> {
    private final long handle;

    GpuFutureImpl(long handle) {
        this.handle = handle;
        StreamCleaner.register(this, handle, StreamCleaner.ResourceKind.FUTURE);
    }

    @Override
    public boolean isDone() {
        return Native.futureStatus(handle) != 0;
    }

    @Override
    @SuppressWarnings("unchecked")
    public T get() throws InterruptedException, GpuException {
        Native.futureSynchronize(handle);
        int s = Native.futureStatus(handle);
        if (s == 2) {
            String msg = Native.futureGetErrorMessage(handle);
            throw new GpuException(msg != null ? msg : "kernel failed");
        }
        return (T) Native.futureGetResult(handle);
    }

    @Override
    public T getNow(T fallback) {
        if (!isDone()) return fallback;
        try {
            return get();
        } catch (Exception e) {
            return fallback;
        }
    }

    @Override
    public <U> GpuFuture<U> thenApplyGpu(GpuFunction<? super T, ? extends U> fn) {
        try {
            T value = get();
            // Phase 7 #3: try GPU dispatch first. If the lambda is
            // a static-method reference whose target is GPU-eligible,
            // Native.submitWithArg returns a real submission. If
            // the lambda can't be resolved or the kernel isn't
            // eligible, the returned future immediately reports
            // Failed and we fall back to evaluating `fn` on the CPU
            // so existing user code keeps working.
            //
            // execHandle=0 because dispatch_method_from_native
            // doesn't actually consult the handle today (it uses
            // OffloadCacheRegistry with the default device ordinal).
            @SuppressWarnings("unchecked")
            GpuFuture<U> gpuResult = (GpuFuture<U>) Native.submitWithArg(0L, fn, value);
            if (gpuResult != null) {
                // futureStatus: 0=running, 1=completed, 2=failed.
                // On Failed at submission time we know the GPU path
                // didn't take. On Running / Completed we hand the
                // future back; the user's get() will wait.
                long h = ((GpuFutureImpl<?>) gpuResult).handle();
                int status = Native.futureStatus(h);
                if (status != 2) {
                    return gpuResult;
                }
                // Release the synthetic Failed future since we're
                // discarding it. (StreamCleaner will GC-release
                // eventually anyway; explicit is tidier.)
                Native.releaseFuture(h);
            }
            // CPU fallback — original Phase 3.5 behavior.
            U result = fn.apply(value);
            return new CompletedFuture<>(result);
        } catch (Exception e) {
            throw new GpuException("thenApplyGpu failed", e);
        }
    }

    @Override
    public CompletableFuture<T> toCompletableFuture() {
        return CompletableFuture.supplyAsync(() -> {
            try {
                return get();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                throw new GpuException("interrupted");
            }
        });
    }

    long handle() {
        return handle;
    }

    /** Trivial already-done future, used internally by thenApplyGpu. */
    private static final class CompletedFuture<T> implements GpuFuture<T> {
        private final T value;

        CompletedFuture(T value) {
            this.value = value;
        }

        @Override
        public boolean isDone() {
            return true;
        }

        @Override
        public T get() {
            return value;
        }

        @Override
        public T getNow(T fallback) {
            return value;
        }

        @Override
        public <U> GpuFuture<U> thenApplyGpu(GpuFunction<? super T, ? extends U> fn) {
            try {
                return new CompletedFuture<>(fn.apply(value));
            } catch (Exception e) {
                throw new GpuException("thenApplyGpu failed", e);
            }
        }

        @Override
        public CompletableFuture<T> toCompletableFuture() {
            return CompletableFuture.completedFuture(value);
        }
    }
}
