public class SelfCallProbe {
    static class MyTL extends ThreadLocal<Object> {
        Object replace(Object v) {
            Object old = get();
            set(v);
            return old;
        }
        boolean checkSelfCall(Object o) {
            if (o == get()) {
                set(null);
                return false;
            }
            return true;
        }
    }

    static class Payload {
        int tag;
        Payload(int tag) { this.tag = tag; }
    }

    public static void main(String[] args) {
        MyTL tl = new MyTL();
        Payload obj = new Payload(42);
        System.out.println("STEP1 storing obj in ThreadLocal, identityHashCode=" + System.identityHashCode(obj));
        Object old = tl.replace(obj);
        System.out.println("STEP2 old=" + old);

        // Force allocation pressure / GC to potentially relocate obj.
        System.out.println("STEP3 allocating garbage + forcing GC...");
        Object[] garbage = new Object[3000];
        for (int i = 0; i < garbage.length; i++) {
            garbage[i] = new byte[50_000];
        }
        System.gc();
        System.gc();
        garbage = null;
        System.out.println("STEP4 GC forced. Same obj reference held in local var: identityHashCode=" + System.identityHashCode(obj));

        System.out.println("STEP5 calling checkSelfCall(obj) -- should detect self-call (obj == tl.get())");
        boolean genuinelyNew = tl.checkSelfCall(obj);
        boolean isSelfCall = !genuinelyNew;
        System.out.println("STEP6 isSelfCall=" + isSelfCall + " (expected true -- obj should == tl.get())");
        if (!isSelfCall) {
            System.out.println("BUG REPRODUCED: reference comparison failed to recognize the same object after GC");
        } else {
            System.out.println("OK: self-call correctly detected");
        }
        System.out.println("ALL DONE");
    }
}
