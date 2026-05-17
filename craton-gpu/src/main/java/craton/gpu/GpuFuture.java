package craton.gpu;

import java.util.concurrent.CompletableFuture;
import java.util.function.Function;

/**
 * Represents the pending result of asynchronous GPU work submitted through a
 * {@link GpuExecutor}. A {@code GpuFuture} composes naturally with other GPU
 * work via {@link #thenApplyGpu(GpuFunction)} (the continuation runs on the
 * same device without round-tripping through host code) and can be bridged to
 * the standard {@link java.util.concurrent} API via
 * {@link #toCompletableFuture()}.
 *
 * @param <T> the type produced by the underlying computation
 */
public interface GpuFuture<T> {

    /**
     * Returns {@code true} if the underlying GPU work has completed (either
     * successfully or with an error).
     *
     * @return whether the future has completed
     */
    boolean isDone();

    /**
     * Blocks the calling host thread until the underlying GPU work completes
     * and returns its result. If the work failed, the original cause is wrapped
     * in a {@link GpuException}.
     *
     * @return the computed result
     * @throws GpuException     if the underlying computation failed
     * @throws InterruptedException if the host thread was interrupted while waiting
     */
    T get() throws GpuException, InterruptedException;

    /**
     * Returns the result immediately if the future is already complete,
     * otherwise returns {@code valueIfAbsent}. Never blocks.
     *
     * @param valueIfAbsent fallback value if the future is not yet done
     * @return the computed result or {@code valueIfAbsent}
     */
    T getNow(T valueIfAbsent);

    /**
     * Chains a GPU-resident continuation onto this future. The function is
     * applied on the device when this future completes; no host synchronization
     * is performed between the two stages.
     *
     * @param fn  the continuation to apply
     * @param <R> the type produced by the continuation
     * @return a future representing the chained computation
     */
    <R> GpuFuture<R> thenApplyGpu(GpuFunction<T, R> fn);

    /**
     * Adapts this {@code GpuFuture} to a standard {@link CompletableFuture}.
     * The returned future completes on a host thread once the device work is
     * finished and any pending memcpy back to host is complete.
     *
     * @return a {@link CompletableFuture} mirroring this future's completion
     */
    CompletableFuture<T> toCompletableFuture();
}
