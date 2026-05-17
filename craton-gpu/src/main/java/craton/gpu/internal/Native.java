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
