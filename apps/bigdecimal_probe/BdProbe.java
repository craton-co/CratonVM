// KC16 RKC16N.14 — BigDecimal/BigInteger clinit cascade smoke test.
//
// CratonVM's BigInteger.<clinit> can fail in real-JDK mode (an unsupported
// intrinsic candidate, an early Math.log side-effect, etc.).  When that
// happens the static fields ZERO/ONE/TWO/TEN remain null, and the very
// next class to <clinit> — BigDecimal — does
//   getstatic BigInteger.ZERO
//   new BigDecimal(BigInteger;JII)V
// whose ctor reads inVal.signum, producing the cascade NPE that surfaced
// during KC16 boot.
//
// The fix lives in vm/src/vm/vm_util.rs::post_clinit_fixup which now
// populates the BigInteger and BigDecimal constants when their <clinit>
// is silent-swallowed.  This probe verifies the round-trip:
//   BigInteger.TWO.multiply(BigInteger.TEN) == 20
//   BigDecimal.ONE.add(BigDecimal.TEN) == 11
//
// Expected on success:
//   11
//   20
//   OK
//
// rc=0 means the BigDecimal silent-swallow line is gone from the boot map.

public class BdProbe {
    public static void main(String[] args) {
        System.out.println(java.math.BigDecimal.ONE.add(java.math.BigDecimal.TEN));
        System.out.println(java.math.BigInteger.TWO.multiply(java.math.BigInteger.TEN));
        System.out.println("OK");
    }
}
