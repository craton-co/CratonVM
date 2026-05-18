package craton.gpu.internal;

import craton.gpu.GpuExecutor;
import craton.gpu.GpuFuture;
import craton.gpu.GpuStream;
import craton.gpu.GpuCallable;
import craton.gpu.GpuFunction;
import craton.gpu.GpuRunnable;

/**
 * Low-level bridge to the GPU backend. These methods are bridged to Rust via the CratonVM
 * native-builtin registry, not JNI. Do not call directly from user code &mdash; use
 * {@link craton.gpu.GpuExecutor}, {@link craton.gpu.GpuFuture}, and
 * {@code craton.gpu.GpuArray} instead. The method names and signatures here form a stable
 * contract with the Rust side (see Phase 3 spec &sect;2.2): any change must be mirrored in
 * the Rust native-builtin registration.
 */
public final class Native {
    private Native() {}

    public static native GpuExecutor openExecutor(int deviceOrdinal);

    public static native <R> GpuFuture<R> submit(long execHandle, GpuCallable<R> task);
    public static native GpuFuture<Void>  launch(long execHandle, GpuRunnable task);

    /**
     * Phase 5: explicit named-method dispatch. Bypasses lambda
     * resolution by taking the target method by class + name +
     * descriptor directly. The {@code args} array holds the boxed
     * primitive scalars, primitive arrays, and
     * {@link craton.gpu.GpuArray} handles to pass to the kernel.
     */
    public static native <R> GpuFuture<R> submitMethod(
        long execHandle,
        String className,
        String methodName,
        String descriptor,
        Object[] args);

    /**
     * Phase 7 #3: like {@link #submit} but for {@code GpuFunction<T,R>}
     * lambdas that take a single SAM arg. The lambda's captures plus
     * {@code samArg} (appended last) become the kernel parameters.
     * Returns a {@code Failed} future if the lambda can't be resolved
     * or the kernel isn't eligible — callers (typically
     * {@code GpuFuture.thenApplyGpu}) may fall back to CPU evaluation
     * after observing the failure.
     */
    public static native <R> GpuFuture<R> submitWithArg(
        long execHandle,
        Object lambda,
        Object samArg);

    /**
     * Phase 8 #6: generalised {@link #submitWithArg} for SAMs of
     * any arity (BiFunction, TriFunction, custom @FunctionalInterface
     * types with multiple parameters). The lambda's captures
     * followed by {@code samArgs} (in declaration order) become the
     * kernel parameters. Returns a {@code Failed} future when the
     * lambda isn't a GPU-dispatchable static-method reference.
     */
    public static native <R> GpuFuture<R> submitWithArgs(
        long execHandle,
        Object lambda,
        Object[] samArgs);

    public static native GpuStream newStream(long execHandle);
    public static native void      closeStream(long streamHandle);

    public static native int     futureStatus(long futureHandle);
    public static native void    futureSynchronize(long futureHandle);
    public static native Object  futureGetResult(long futureHandle);
    public static native String  futureGetErrorMessage(long futureHandle);

    public static native long    arrayWrapInt(int[] host);
    public static native long    arrayWrapLong(long[] host);
    public static native long    arrayWrapFloat(float[] host);
    public static native long    arrayWrapDouble(double[] host);
    public static native Object  arrayToHost(long arrayHandle);
    public static native boolean arrayIsResident(long arrayHandle);

    public static native void    releaseFuture(long futureHandle);
    public static native void    releaseArray(long arrayHandle);
    public static native void    releaseExecutor(long execHandle);
}
