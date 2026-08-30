/** Which invokespecial callees lose their receiver null check once warm?
 *  Same warming shape as NullReceiverCachedProbe; only the callee body varies. */
public class NrpVariants {
  static class Impl {
    int x = 7;
    private int constBody()   { return 3; }          // no use of `this`
    private int fieldBody()   { return x; }          // dereferences `this`
    private int callBody()    { return helper(); }   // another private call
    private int helper()      { return 5; }
    static int viaConst(Impl t) { return t.constBody(); }
    static int viaField(Impl t) { return t.fieldBody(); }
    static int viaCall(Impl t)  { return t.callBody(); }
  }
  static void check(String label, java.util.function.IntSupplier s) {
    try { System.out.println(label + "=NO-THROW(" + s.getAsInt() + ")"); }
    catch (NullPointerException e) { System.out.println(label + "=NPE"); }
    catch (Throwable t) { System.out.println(label + "=OTHER(" + t.getClass().getName() + ")"); }
  }
  public static void main(String[] a) {
    Impl real = new Impl();
    int sink = 0;
    for (int i = 0; i < 50000; i++) {
      sink += Impl.viaConst(real); sink += Impl.viaField(real); sink += Impl.viaCall(real);
    }
    if (sink == 0) System.out.println("unreachable");
    check("warm-special-constBody", () -> Impl.viaConst(null));
    check("warm-special-fieldBody", () -> Impl.viaField(null));
    check("warm-special-callBody",  () -> Impl.viaCall(null));
    System.out.println("OK");
  }
}
