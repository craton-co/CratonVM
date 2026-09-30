// Exercises the deferred-`new` retry across a class-load boundary.
//
// The `new Late(...)` sits on a branch phase 1 never takes, so the class is
// still unloaded when the hot method is compiled and the IR builder bails on
// the `0xbb` arm, arming the retry memo. Phase 2 loads the class by another
// route. Phase 3 keeps the method hot.
//
// A retry spent BLIND in phase 1 is spent on an attempt that bails exactly as
// the first did; a retry HELD until phase 2 is still there when it can succeed.
public class DeferredNewReoffer {
    static final class Late {
        final int v;
        Late(int v) { this.v = v; }
        int get() { return v; }
    }

    static int sink;
    static volatile int magic = -1; // never equals the loop's `b` in phase 1

    static int make(int a, int b) {
        if (b == magic) {
            Late l = new Late(a);   // unreachable in phase 1: class stays unloaded
            return l.get();
        }
        return (a * 31) ^ b;
    }

    static int hot(int rounds) {
        int acc = 0;
        for (int i = 0; i < rounds; i++) acc += make(i, acc & 0xff);
        return acc;
    }

    // Loads `Late` without going through `make`.
    static void touch() { sink = new Late(1).get(); }

    public static void main(String[] args) {
        int acc = hot(400_000);
        touch();
        acc += hot(2_000_000);
        System.out.println("checksum " + acc);
    }
}
