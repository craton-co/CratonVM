import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.reflect.Method;

/**
 * W7-89 — a single-predicate read of "does the VM consider this session one it
 * modelled?", with no arena and no segment in the way.
 *
 * <p>The chain from {@code arena.scope()} to the model has five predicates and
 * an unstable {@code scope()} only says that ONE of them failed. This asks the
 * last one on its own: take a session object, call {@code close()} DIRECTLY on
 * it, and ask {@code isAlive()}. CratonVM's {@code isAlive} is
 * {@code !modelled || state == 1}, so:
 *
 * <ul>
 *   <li>{@code false} after close ⇒ the state word round-trips and the session
 *       IS modelled — the failure is upstream, in resolving the session;
 *   <li>{@code true} after close ⇒ the session is not modelled, i.e. the state
 *       word does not read back as an int.
 * </ul>
 *
 * <p>On HotSpot both rows are simply the correct answers, which is what makes
 * the probe legible rather than CratonVM-only: a live session is alive, a
 * closed one is not.
 *
 * <pre>
 * javac -d out probes/MemorySessionModelledProbe.java
 * java     --enable-native-access=ALL-UNNAMED \
 *          --add-exports java.base/jdk.internal.foreign=ALL-UNNAMED \
 *          -cp out MemorySessionModelledProbe
 * </pre>
 */
public final class MemorySessionModelledProbe {

    private static void row(String key, Object value) {
        System.out.println(key + " = " + value);
        System.out.flush();
    }

    private static String call(Method m, Object target) {
        try {
            m.invoke(target);
            return "ok";
        } catch (Throwable t) {
            Throwable c = t.getCause() == null ? t : t.getCause();
            return c.getClass().getName() + ":" + c.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        Class<?> impl = Class.forName("jdk.internal.foreign.MemorySessionImpl");
        Method close = impl.getMethod("close");
        Method isAlive = impl.getMethod("isAlive");

        Arena arena = Arena.ofConfined();
        MemorySegment seg = arena.allocate(16);
        Object scope = seg.scope();
        row("scope.class", scope.getClass().getName());
        row("beforeClose.isAlive", isAlive.invoke(scope));
        row("close", call(close, scope));
        row("afterClose.isAlive", isAlive.invoke(scope));

        // `justClose()` is package-private and ABSTRACT on JDK 25
        // (`MemorySessionImpl.java:244`), so it is deliberately not probed:
        // `getMethod` cannot see it and reaching it would need a concrete
        // subclass this VM never instantiates.

        // And on the arena's own session, reached without going through a
        // segment at all.
        Object arenaScope = arena.scope();
        row("arenaScope.beforeClose.isAlive", isAlive.invoke(arenaScope));
        row("arenaScope.close", call(close, arenaScope));
        row("arenaScope.afterClose.isAlive", isAlive.invoke(arenaScope));
    }
}
