package craton.gpu;

/**
 * Unchecked exception raised when a GPU operation fails. Wraps lower-level
 * driver errors (CUDA error codes, PTX/JIT compile failures, out-of-memory on
 * the device, kernel launch failures, etc.) into a single host-side type that
 * callers can catch without depending on the native bridge.
 */
public class GpuException extends RuntimeException {

    private static final long serialVersionUID = 1L;

    /**
     * Constructs a {@code GpuException} with the given message.
     *
     * @param message the detail message
     */
    public GpuException(String message) {
        super(message);
    }

    /**
     * Constructs a {@code GpuException} with the given message and cause.
     *
     * @param message the detail message
     * @param cause   the underlying cause
     */
    public GpuException(String message, Throwable cause) {
        super(message, cause);
    }
}
