/**
 * Hot inherited trivial getters called from a compiled loop. Hibernate's flush
 * walk calls getters such as EntityEntryImpl.getPersister() and
 * ReactiveSessionImpl.getFactory() ~10^8 times; jit-method-stats reported them
 * "compile-failed reason=unrecorded" and they ran interpreted. Run with
 * CRATONVM_DBG=jit-method-stats and look for HriInheritedGetter$ rows.
 */
public class HriInheritedGetter {
    static abstract class Base {
        Object persister = "p";
        int status = 1;

        Object getPersister() {
            return persister;
        }

        int getStatus() {
            return status;
        }
    }

    static final class Impl extends Base {}

    static class Mid extends Base {}

    static final class Leaf extends Mid {}

    interface Entry {
        Object getPersister();
    }

    static final class Ent extends Base implements Entry {
        @Override
        public Object getPersister() {
            return super.getPersister();
        }
    }

    interface Rec {
        Object getPersister();

        Object[] getLoadedState();
    }

    static final class RecA implements Rec {
        final Object persister = "a";
        final Object[] state = new Object[1];

        public Object getPersister() {
            return persister;
        }

        public Object[] getLoadedState() {
            return state;
        }
    }

    static final class RecB implements Rec {
        final Object persister = "b";
        final Object[] state = new Object[2];

        public Object getPersister() {
            return persister;
        }

        public Object[] getLoadedState() {
            return state;
        }
    }

    static long driveRec(Rec[] rs, int rounds) {
        long n = 0;
        for (int r = 0; r < rounds; r++) {
            for (Rec x : rs) {
                if (x.getPersister() != null) n += x.getLoadedState().length;
            }
        }
        return n;
    }

    static long drive(Base[] bs, int rounds) {
        long n = 0;
        for (int r = 0; r < rounds; r++) {
            for (Base b : bs) {
                if (b.getPersister() != null) n += b.getStatus();
            }
        }
        return n;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        Base[] bs = {new Impl(), new Leaf(), new Ent(), new Impl()};
        long t0 = System.nanoTime();
        long n = 0;
        Rec[] rs = {new RecA(), new RecB(), new RecA()};
        for (int i = 0; i < 10; i++) n += drive(bs, rounds / 10);
        for (int i = 0; i < 10; i++) n += driveRec(rs, rounds / 10);
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("sum=" + n);
        System.err.println("ms=" + ms);
    }
}
