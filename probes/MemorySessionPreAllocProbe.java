import java.lang.foreign.Arena;

/**
 * W7-89 — is the session dead on ARRIVAL, or killed by the first allocation?
 *
 * <p>Two candidate causes produce the identical symptom in
 * {@code MemorySessionIdentityProbe} (an unstable {@code scope()}), and that
 * probe cannot separate them because it allocates before it asks:
 *
 * <ol>
 *   <li>the session's state word never survived the write at all — the model
 *       stores an {@code Int} into a slot the real {@code MemorySessionImpl}
 *       declares as a reference; or
 *   <li>{@code pe_arena_allocate_impl} reads the FOUR-slot arena layout on a
 *       TWO-slot arena, so it treats the session sitting at slot 1 as the
 *       allocation-id array and writes an element into it.
 * </ol>
 *
 * <p>Asking BEFORE any {@code allocate()} decides it: stable-then-unstable
 * indicts (2), unstable-from-the-start indicts (1).
 *
 * <pre>
 * javac -d out probes/MemorySessionPreAllocProbe.java
 * java     --enable-native-access=ALL-UNNAMED -cp out MemorySessionPreAllocProbe
 * cratonvm --enable-native-access=ALL-UNNAMED -cp out MemorySessionPreAllocProbe
 * </pre>
 */
public final class MemorySessionPreAllocProbe {

    private static void row(String key, Object value) {
        System.out.println(key + " = " + value);
        System.out.flush();
    }

    public static void main(String[] args) {
        Arena a = Arena.ofConfined();
        row("beforeAllocate.scope.stable", a.scope() == a.scope());
        row("beforeAllocate.scope.isAlive", a.scope().isAlive());
        a.allocate(16);
        row("afterAllocate.scope.stable", a.scope() == a.scope());
        a.close();
        row("afterClose.scope.isAlive", a.scope().isAlive());

        // Same question with no allocation anywhere in the arena's life.
        Arena b = Arena.ofConfined();
        row("neverAllocated.scope.stable", b.scope() == b.scope());
        b.close();
        row("neverAllocated.closed.isAlive", b.scope().isAlive());
    }
}
