public class ColdThrowArg {
    boolean c;
    boolean isCommitted() { return c; }
    static String sm(String k) {
        if (k == null) throw new IllegalArgumentException("key may not have a null value");
        return k + "!";
    }
    void check(String k) {
        if (isCommitted()) throw new IllegalStateException(sm(k));
    }
    void reset() { check("coyoteResponse.resetBuffer.ise"); }
    public static void main(String[] a) {
        ColdThrowArg o = new ColdThrowArg();
        int bad = 0, good = 0;
        for (int round = 0; round < 20; round++) {
            o.c = false;
            for (int i = 0; i < 200000; i++) o.reset();
            o.c = true;
            for (int i = 0; i < 2000; i++) {
                try { o.reset(); bad++; }
                catch (IllegalStateException e) { good++; }
                catch (IllegalArgumentException e) { bad++; }
            }
        }
        System.out.println("good=" + good + " bad=" + bad);
    }
}
