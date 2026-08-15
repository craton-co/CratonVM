import java.math.BigInteger;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.Signature;
import java.security.interfaces.RSAPublicKey;

/**
 * `NONEwithRSA` — the one RSA signature name SunJCE serves rather than
 * SunRsaSign, because it is implemented there by encrypting under the private
 * key.
 *
 * A ROUND TRIP CANNOT CHECK THIS. Sign-then-verify inside one VM passes for any
 * padding scheme that is self-consistent, including a wrong one, which is the
 * trap `an-engine-that-validates-a-name-then-dispatches-on-something-else`
 * records. So this probe recovers the ENCODED MESSAGE the signature actually
 * carries — `EM = S^e mod n`, computed with `BigInteger` and therefore
 * identical arithmetic on both VMs — and prints it.
 *
 * `EM` depends only on the modulus SIZE and the payload, never on the key, so
 * the line is byte-identical between two VMs that generated different key
 * pairs. That is what makes this a cross-check rather than a round trip: a
 * padding scheme that differs from SunJCE's shows up as a different `em=` line
 * even though both VMs verified their own signature happily.
 */
public final class NoneWithRsaProbe {

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder(b.length * 2);
        for (byte x : b) {
            sb.append(Character.forDigit((x >> 4) & 0xf, 16)).append(Character.forDigit(x & 0xf, 16));
        }
        return sb.toString();
    }

    /** `S^e mod n`, left-padded to the modulus length. */
    static byte[] recover(RSAPublicKey pub, byte[] sig) {
        BigInteger n = pub.getModulus();
        int k = (n.bitLength() + 7) / 8;
        byte[] raw = new BigInteger(1, sig).modPow(pub.getPublicExponent(), n).toByteArray();
        byte[] out = new byte[k];
        int copy = Math.min(raw.length, k);
        System.arraycopy(raw, raw.length - copy, out, k - copy, copy);
        return out;
    }

    static void line(String key, Object value) {
        System.out.println(key + "=" + value);
    }

    public static void main(String[] a) throws Exception {
        byte[] payload = "32-bytes-of-caller-chosen-digest".getBytes("UTF-8");

        Signature s;
        try {
            s = Signature.getInstance("NONEwithRSA");
        } catch (Throwable t) {
            line("getInstance", t.getClass().getName());
            return;
        }
        line("getInstance", "OK");
        line("provider", s.getProvider() == null ? "null" : s.getProvider().getName());
        line("algorithm", s.getAlgorithm());

        KeyPairGenerator g = KeyPairGenerator.getInstance("RSA");
        g.initialize(2048);
        KeyPair kp = g.generateKeyPair();

        s.initSign(kp.getPrivate());
        s.update(payload);
        byte[] sig = s.sign();
        line("sig.len", sig.length);
        // The whole point: the ENCODED MESSAGE, which is key-independent.
        line("em", hex(recover((RSAPublicKey) kp.getPublic(), sig)));

        Signature v = Signature.getInstance("NONEwithRSA");
        v.initVerify(kp.getPublic());
        v.update(payload);
        line("verify.good", v.verify(sig));

        v = Signature.getInstance("NONEwithRSA");
        v.initVerify(kp.getPublic());
        v.update("a different payload here!!!!!!!!".getBytes("UTF-8"));
        line("verify.wrongPayload", v.verify(sig));

        v = Signature.getInstance("NONEwithRSA");
        v.initVerify(kp.getPublic());
        v.update(payload);
        byte[] forged = sig.clone();
        forged[100] ^= 0x01;
        try {
            line("verify.forged", v.verify(forged));
        } catch (Throwable t) {
            line("verify.forged", t.getClass().getName());
        }

        // k - 11 is the largest payload PKCS#1 v1.5 block type 1 can carry
        // under a 256-byte modulus; one more byte must be refused, not signed.
        for (int len : new int[] {245, 246}) {
            Signature x = Signature.getInstance("NONEwithRSA");
            x.initSign(kp.getPrivate());
            x.update(new byte[len]);
            try {
                line("sign.len" + len, "OK len=" + x.sign().length);
            } catch (Throwable t) {
                line("sign.len" + len, t.getClass().getName());
            }
        }
        System.out.println("NoneWithRsaProbe done");
    }
}
