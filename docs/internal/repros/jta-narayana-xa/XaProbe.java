import jakarta.transaction.*;
import javax.transaction.xa.*;

// Isolates Hibernate's JtaAwareConnectionProviderImpl flow:
//   TM.begin() -> TM.getTransaction() -> tx.enlistResource(xa) -> TM.commit()
// and pins WHERE the real Narayana 1PC completion breaks (static trace:
// BasicAction.End -> doOnePhase[TxControl.onePhase && pendingList.size()==1 &&
// isPermittedTopLevelOnePhaseCommit] -> onePhaseCommit ELSE prepare()).
//
// TX1 uses a NON-throwing prepare → cleanly reveals the path:
//   "@@1 XA.commit onePhase=true"           -> 1PC OK (expected; matches HotSpot)
//   "@@1 XA.prepare" + "XA.commit onePhase=false" -> 2PC taken (real Hibernate's
//                                                     prepare() THROWS => bug)
//   enlistResource=false / no callbacks     -> enlist or commit-walk gap (lock held)
//
// TX2 mimics Hibernate EXACTLY (prepare throws "this should never be called") so
// the run REPRODUCES the actual failure when 2PC is (wrongly) chosen.
public class XaProbe {
    static class XA implements XAResource {
        final String tag; final boolean throwInPrepare;
        XA(String tag, boolean throwInPrepare) { this.tag = tag; this.throwInPrepare = throwInPrepare; }
        public void commit(Xid xid, boolean onePhase) { System.out.println("@@" + tag + " XA.commit onePhase=" + onePhase); }
        public void rollback(Xid xid) { System.out.println("@@" + tag + " XA.rollback"); }
        public void start(Xid xid, int f) { System.out.println("@@" + tag + " XA.start flags=" + f); }
        public void end(Xid xid, int f) { System.out.println("@@" + tag + " XA.end flags=" + f); }
        public int prepare(Xid xid) throws XAException {
            System.out.println("@@" + tag + " XA.prepare" + (throwInPrepare ? " (THROW like Hibernate)" : ""));
            if (throwInPrepare) throw new RuntimeException("this should never be called");
            return XA_OK;
        }
        public void forget(Xid xid) { System.out.println("@@" + tag + " XA.forget"); }
        public int getTransactionTimeout() { return 0; }
        public boolean setTransactionTimeout(int s) { return true; }
        public boolean isSameRM(XAResource r) { return r == this; }
        public Xid[] recover(int f) { return new Xid[0]; }
    }

    static void runTx(TransactionManager tm, String tag, boolean throwInPrepare) throws Exception {
        tm.begin();
        Transaction t = tm.getTransaction();
        System.out.println("@@" + tag + " getTransaction=" + (t == null ? "NULL" : t.getClass().getName()) + " status=" + tm.getStatus());
        boolean enlisted = t.enlistResource(new XA(tag, throwInPrepare));
        System.out.println("@@" + tag + " enlistResource returned=" + enlisted);
        try {
            tm.commit();
            System.out.println("@@" + tag + " commit OK status=" + tm.getStatus());
        } catch (Throwable ex) {
            System.out.println("@@" + tag + " commit THREW " + ex.getClass().getName() + ": " + ex.getMessage() + " status=" + tm.getStatus());
        }
    }

    public static void main(String[] a) throws Exception {
        TransactionManager tm = org.hibernate.testing.jta.TestingJtaPlatformImpl.transactionManager();
        System.out.println("@@ tm=" + tm.getClass().getName());
        try {
            java.lang.reflect.Field f = Class.forName("com.arjuna.ats.arjuna.coordinator.TxControl").getDeclaredField("onePhase");
            f.setAccessible(true);
            System.out.println("@@ TxControl.onePhase(static)=" + f.getBoolean(null) + " (true=>1PC enabled)");
        } catch (Throwable ex) { System.out.println("@@ TxControl.onePhase read failed: " + ex); }
        runTx(tm, "1", false);  // reveal 1PC vs 2PC
        runTx(tm, "2", true);   // reproduce Hibernate (prepare throws)
        System.out.println("@@ DONE");
    }
}
