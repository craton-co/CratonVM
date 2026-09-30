import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharsetEncoder;
import java.nio.charset.StandardCharsets;
import java.security.KeyFactory;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.PrivateKey;
import java.security.PublicKey;
import java.security.Signature;
import java.security.spec.PKCS8EncodedKeySpec;
import java.security.spec.X509EncodedKeySpec;

/**
 * gc-common w17-b: churn the objects whose native side-table rows now go with
 * them through the lock-key registry. Each round creates:
 *
 * - a UTF-16 CharsetEncoder, which must write its BOM exactly once;
 * - a Signature, used once to sign and once to verify;
 * - a pair of re-imported RSA key objects (`RSA_REALKEY_MAP` rows).
 *
 * Every result is checked, so a dead object's row that a new object inherits
 * fails loudly: a missing BOM, or a signature under the wrong key. The "used"
 * column should stay flat after the first report, on every backend and on
 * HotSpot.
 *
 * Ends on its own. Arg 0: round count (default 4000).
 */
public final class SigBomChurnProbe {
	public static void main(String[] a) throws Exception {
		int n = a.length > 0 ? Integer.parseInt(a[0]) : 4000;
		KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
		kpg.initialize(2048);
		KeyPair kp = kpg.generateKeyPair();
		byte[] privDer = kp.getPrivate().getEncoded();
		byte[] pubDer = kp.getPublic().getEncoded();
		KeyFactory kf = KeyFactory.getInstance("RSA");
		byte[] msg = "gc-common w17-b".getBytes(StandardCharsets.US_ASCII);
		Runtime rt = Runtime.getRuntime();
		long first = -1, last = -1;
		int bad = 0;
		for (int i = 1; i <= n; i++) {
			CharsetEncoder enc = StandardCharsets.UTF_16.newEncoder();
			ByteBuffer out = ByteBuffer.allocate(64);
			enc.encode(CharBuffer.wrap("ab"), out, false);
			enc.encode(CharBuffer.wrap("cd"), out, true);
			out.flip();
			// FE FF, then four big-endian chars: exactly one BOM.
			if (out.remaining() != 10 || (out.get(0) & 0xff) != 0xFE || (out.get(1) & 0xff) != 0xFF
					|| (out.get(2) & 0xff) == 0xFE) {
				bad++;
				if (bad <= 5) System.out.println("BOM wrong at round " + i + ": " + out.remaining() + " bytes");
			}

			PrivateKey priv = kf.generatePrivate(new PKCS8EncodedKeySpec(privDer));
			PublicKey pub = kf.generatePublic(new X509EncodedKeySpec(pubDer));
			Signature s = Signature.getInstance("SHA256withRSA");
			s.initSign(priv);
			s.update(msg);
			byte[] sig = s.sign();
			Signature v = Signature.getInstance("SHA256withRSA");
			v.initVerify(pub);
			v.update(msg);
			if (!v.verify(sig)) {
				bad++;
				if (bad <= 5) System.out.println("verify failed at round " + i);
			}

			if (i % 500 == 0) {
				System.gc();
				long used = rt.totalMemory() - rt.freeMemory();
				if (first < 0) first = used;
				last = used;
				System.out.println("round " + i + " used=" + (used >> 10) + "K");
			}
		}
		System.out.println("first=" + (first >> 10) + "K last=" + (last >> 10) + "K bad=" + bad);
		System.out.println(bad == 0 ? "PASS" : "FAIL");
	}
}
