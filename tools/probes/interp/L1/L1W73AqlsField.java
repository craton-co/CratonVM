// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 73, lane L1
// (docs/internal/fixed-bugs/interpreter-i72-L1-proposal-retire-the-aqls-side-table-DONE-20261106.md):
// the `CRATONVM_COMPAT_JDK_AQLS=0` arm's `AbstractQueuedLongSynchronizer`
// state stand-ins keep the word in the object's own `state` field (resolved on
// the DECLARING class) instead of a process-global side table. The rows here
// pin what the field placement decides:
//
// * a subclass that declares its OWN `state` field (a shadowing name) keeps it
//   apart from the synchronizer's word;
// * a serialized copy carries the word (the stream writes the field);
// * a subclass whose `readObject` restores through `setState` agrees with one
//   that relies on the default field restore.
//
// Run: `cratonvm L1W73AqlsField`, `cratonvm --compatible L1W73AqlsField`, and
// the stand-in arm `CRATONVM_COMPAT_JDK_AQLS=0 cratonvm --compatible L1W73AqlsField`.
// All three must print the block below. (Before wave 73 the `=0` arm printed
// `copy getState=0` on the two serialization rows.) The block is derived from
// the JDK 25 sources; refresh it from the host oracle before reading a
// difference.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// shadow: getState=9 own state=-3
// shadow after cas(9 -> 11): getState=11 own state=-3 cas=true
// shadow copy: getState=11 own state=-3
// restoring copy: getState=77 restored=true
// done
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.util.concurrent.locks.AbstractQueuedLongSynchronizer;

public class L1W73AqlsField {
    /** Declares a field with the synchronizer's private field's name. */
    static final class Shadow extends AbstractQueuedLongSynchronizer {
        private static final long serialVersionUID = 1L;
        long state = -3;

        long word() {
            return getState();
        }

        void word(long v) {
            setState(v);
        }

        boolean cas(long e, long n) {
            return compareAndSetState(e, n);
        }
    }

    /** Restores through `setState` after the default restore. */
    static final class Restoring extends AbstractQueuedLongSynchronizer {
        private static final long serialVersionUID = 1L;
        transient boolean restored;

        long word() {
            return getState();
        }

        void word(long v) {
            setState(v);
        }

        private void readObject(ObjectInputStream in) throws IOException, ClassNotFoundException {
            in.defaultReadObject();
            setState(getState());
            restored = true;
        }
    }

    @SuppressWarnings("unchecked")
    static <T> T roundTrip(T o) throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(o);
        }
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
            return (T) in.readObject();
        }
    }

    public static void main(String[] args) throws Exception {
        Shadow s = new Shadow();
        s.word(9);
        System.out.println("shadow: getState=" + s.word() + " own state=" + s.state);
        boolean cas = s.cas(9, 11);
        System.out.println("shadow after cas(9 -> 11): getState=" + s.word() + " own state=" + s.state + " cas=" + cas);
        Shadow sc = roundTrip(s);
        System.out.println("shadow copy: getState=" + sc.word() + " own state=" + sc.state);

        Restoring r = new Restoring();
        r.word(77);
        Restoring rc = roundTrip(r);
        System.out.println("restoring copy: getState=" + rc.word() + " restored=" + rc.restored);
        System.out.println("done");
    }
}
