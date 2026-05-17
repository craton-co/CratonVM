package craton.gpu;

import craton.gpu.internal.Native;
import craton.gpu.internal.StreamCleaner;
import craton.gpu.internal.StreamCleaner.ResourceKind;

import java.util.concurrent.CompletableFuture;

/**
 * A handle to a primitive array that is resident (or eligible to be made resident)
 * on a GPU device. Instances are created via one of the typed {@code wrap(...)}
 * factories and own a native residency-tracker handle that is released when the
 * Java object becomes unreachable (via {@link StreamCleaner}).
 *
 * <h2>Type erasure caveat</h2>
 *
 * The generic parameter {@code T} is purely a compile-time convenience. Due to
 * Java's type erasure, the runtime cannot recover {@code T} from a {@code GpuArray}
 * instance; the element type is tracked separately via {@link #elementType}.
 *
 * <p>In particular, the host-side array returned by {@link #toHost()} is the raw
 * primitive array type that was originally wrapped:
 *
 * <pre>{@code
 * int[] host = { 1, 2, 3 };
 * GpuArray<int[]> a = GpuArray.wrap(host);
 * int[] back = a.toHost().get(); // returns int[] — must cast or use the typed
 *                                // factory return to avoid an unchecked cast.
 * }</pre>
 *
 * <p>Mixing element types (e.g. assigning a {@code GpuArray<int[]>} reference to a
 * {@code GpuArray<float[]>} variable through raw types) will produce a
 * {@link ClassCastException} when {@link #toHost()} resolves.
 *
 * <h2>Lifecycle</h2>
 *
 * <ul>
 *   <li>The native handle is allocated by the appropriate
 *       {@code Native.arrayWrap*} call inside a typed factory.</li>
 *   <li>The instance is registered with {@link StreamCleaner} so that
 *       {@link Native#releaseArray(long)} runs when this object is GC'd.</li>
 *   <li>{@link #isResident()} reports whether the device-side mirror is currently
 *       allocated. Residency may be revoked by the residency tracker even while
 *       this Java handle is still live; the next kernel launch will re-stage.</li>
 * </ul>
 *
 * @param <T> the host-side primitive array type (e.g. {@code int[]}, {@code float[]})
 */
public final class GpuArray<T> {

    private final long handle;
    private final Class<?> elementType;

    private GpuArray(long handle, Class<?> elementType) {
        this.handle = handle;
        this.elementType = elementType;
    }

    /**
     * Wraps a host {@code int[]} into a {@code GpuArray}. The native side records
     * the host pointer; transfer to device is deferred until first kernel use.
     *
     * @param host non-null host array
     * @return a {@code GpuArray} with element type {@code int}
     */
    public static GpuArray<int[]> wrap(int[] host) {
        long h = Native.arrayWrapInt(host);
        GpuArray<int[]> a = new GpuArray<>(h, int.class);
        StreamCleaner.register(a, h, ResourceKind.ARRAY);
        return a;
    }

    /**
     * Wraps a host {@code long[]} into a {@code GpuArray}.
     *
     * @param host non-null host array
     * @return a {@code GpuArray} with element type {@code long}
     */
    public static GpuArray<long[]> wrap(long[] host) {
        long h = Native.arrayWrapLong(host);
        GpuArray<long[]> a = new GpuArray<>(h, long.class);
        StreamCleaner.register(a, h, ResourceKind.ARRAY);
        return a;
    }

    /**
     * Wraps a host {@code float[]} into a {@code GpuArray}.
     *
     * @param host non-null host array
     * @return a {@code GpuArray} with element type {@code float}
     */
    public static GpuArray<float[]> wrap(float[] host) {
        long h = Native.arrayWrapFloat(host);
        GpuArray<float[]> a = new GpuArray<>(h, float.class);
        StreamCleaner.register(a, h, ResourceKind.ARRAY);
        return a;
    }

    /**
     * Wraps a host {@code double[]} into a {@code GpuArray}.
     *
     * @param host non-null host array
     * @return a {@code GpuArray} with element type {@code double}
     */
    public static GpuArray<double[]> wrap(double[] host) {
        long h = Native.arrayWrapDouble(host);
        GpuArray<double[]> a = new GpuArray<>(h, double.class);
        StreamCleaner.register(a, h, ResourceKind.ARRAY);
        return a;
    }

    /**
     * Returns a {@link GpuFuture} that, when complete, yields the host-side copy
     * of this array. In Phase 3 the transfer back is synchronous and the returned
     * future is already done.
     *
     * <p>The runtime type of the returned value is the same primitive-array type
     * passed to the original {@code wrap(...)} factory. Callers using a typed
     * reference (e.g. {@code GpuArray<int[]>}) need not cast; callers that have
     * widened the type via raw types should cast at their own risk.
     *
     * @return a completed {@link GpuFuture} carrying the host array
     */
    @SuppressWarnings("unchecked")
    public GpuFuture<T> toHost() {
        Object host = Native.arrayToHost(handle);
        return new CompletedFuture<>((T) host);
    }

    /**
     * Reports whether the device-side mirror is currently allocated.
     *
     * <p>Residency is managed by the native residency tracker. A {@code false}
     * result here does not invalidate this {@code GpuArray}; the next kernel
     * launch that uses it will re-stage the data.
     *
     * @return {@code true} if the device-side mirror is currently allocated
     */
    public boolean isResident() {
        return Native.arrayIsResident(handle);
    }

    /**
     * Returns the opaque native residency-tracker handle backing this array.
     * Intended for internal use by the kernel-launch path.
     *
     * @return the native handle
     */
    public long handle() {
        return handle;
    }

    /**
     * Returns the primitive element type ({@code int.class}, {@code long.class},
     * {@code float.class}, or {@code double.class}) recorded at construction.
     *
     * @return the element {@link Class} token
     */
    public Class<?> elementType() {
        return elementType;
    }

    /**
     * Trivial {@link GpuFuture} that is already done. Used by Phase 3 where
     * {@link #toHost()} performs a synchronous transfer and immediately
     * publishes the host array.
     */
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
