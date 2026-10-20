public class RejectThrow {
    // Throwing a local null-typed Throwable emits `athrow` directly,
    // with no `new` allocation, no method call, and no exception
    // handler entry. That isolates the Reject(Throw) path: the analyzer
    // walks past `aconst_null`, `astore_1`, `iload_0`, `ifge`, `aload_1`,
    // then hits `athrow` (0xBF) and bails with Reason::Throw.
    //
    // The `checkcast` on the way to athrow would itself trigger
    // Reject(TypeCheck), so we keep the local typed as RuntimeException
    // from the start to skip the cast.
    public static void maybeThrow(int x) {
        RuntimeException t = null;
        if (x < 0) {
            throw t;
        }
    }
}
