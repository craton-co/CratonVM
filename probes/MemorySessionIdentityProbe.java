import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;

/**
 * W7-89 — is a segment's scope STABLE, and is it the arena's?
 *
 * <p>Two questions the behavioural probe cannot separate. A `scope()` that
 * mints a fresh always-open session on every call answers `isAlive() == true`
 * forever; so does one that returns a stable-but-wrong session. Identity is
 * what tells them apart, so every row here is an identity comparison rather
 * than a value.
 *
 * <pre>
 * javac -d out probes/MemorySessionIdentityProbe.java
 * java     --enable-native-access=ALL-UNNAMED -cp out MemorySessionIdentityProbe
 * cratonvm --enable-native-access=ALL-UNNAMED -cp out MemorySessionIdentityProbe
 * </pre>
 */
public final class MemorySessionIdentityProbe {

    private static void row(String key, Object value) {
        System.out.println(key + " = " + value);
        System.out.flush();
    }

    private static void arm(String name, Arena arena, boolean closeable) {
        MemorySegment a = arena.allocate(16);
        MemorySegment b = arena.allocate(16);
        row(name + ".arena.scope.stable", arena.scope() == arena.scope());
        row(name + ".seg.scope.stable", a.scope() == a.scope());
        row(name + ".seg.scope==arena.scope", a.scope() == arena.scope());
        row(name + ".segA.scope==segB.scope", a.scope() == b.scope());
        row(name + ".slice.scope==seg.scope", a.asSlice(0, 8).scope() == a.scope());
        row(name + ".seg.scope.isAlive", a.scope().isAlive());
        if (closeable) {
            arena.close();
            row(name + ".closed.arena.scope.isAlive", arena.scope().isAlive());
            row(name + ".closed.seg.scope.isAlive", a.scope().isAlive());
            row(name + ".closed.slice.scope.isAlive", a.asSlice(0, 8).scope().isAlive());
        }
    }

    public static void main(String[] args) {
        arm("confined", Arena.ofConfined(), true);
        arm("shared", Arena.ofShared(), true);
        arm("auto", Arena.ofAuto(), false);
        arm("global", Arena.global(), false);

        byte[] backing = new byte[16];
        MemorySegment heap = MemorySegment.ofArray(backing);
        row("heap.scope.stable", heap.scope() == heap.scope());
        row("heap.scope.isAlive", heap.scope().isAlive());
        row("heap.slice.scope==heap.scope", heap.asSlice(0, 8).scope() == heap.scope());
        row("NULL.scope.isAlive", MemorySegment.NULL.scope().isAlive());
        row("NULL.scope==global.scope", MemorySegment.NULL.scope() == Arena.global().scope());
    }
}
