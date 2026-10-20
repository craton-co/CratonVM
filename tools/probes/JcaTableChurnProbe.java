import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.KeyFactory;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.MessageDigest;
import java.security.PublicKey;
import java.security.Signature;
import java.security.spec.PKCS8EncodedKeySpec;
import java.security.spec.X509EncodedKeySpec;
import java.util.Arrays;
import javax.crypto.Cipher;
import javax.crypto.spec.GCMParameterSpec;
import javax.crypto.spec.SecretKeySpec;

/**
 * gc-common w18-e: churn the JCA objects whose native side-table rows now go
 * with them through the lock-key registry, and check every result, so a dead
 * object's row that a new one inherits fails loudly.
 *
 * Each round:
 * - a SHA-256 MessageDigest fed through update(byte[]), update(byte) (the
 *   JIT fast path once compiled) and update(ByteBuffer), then cloned; both
 *   digests must equal the reference;
 * - an AES/GCM Cipher pair with the SAME key and IV every round: a new
 *   Cipher that inherited a dead one's row would refuse the reused nonce, or
 *   decrypt with stale state; the decrypt goes through doFinal(ByteBuffer,
 *   ByteBuffer);
 * - every 250 rounds, a Signature initialised with a key object that is then
 *   dropped and collected before sign() (the key-store handle must survive).
 *
 * The "used" column should stay flat after the first report, on every backend
 * and on HotSpot. Ends on its own. Arg 0: round count (default 4000).
 */
public final class JcaTableChurnProbe {
	public static void main(String[] a) throws Exception {
		int n = a.length > 0 ? Integer.parseInt(a[0]) : 4000;
		byte[] head = "gc-common w18-e ".getBytes(StandardCharsets.US_ASCII);
		byte[] tail = "jca table churn".getBytes(StandardCharsets.US_ASCII);
		MessageDigest ref = MessageDigest.getInstance("SHA-256");
		ref.update(head);
		for (int b = 0; b < 64; b++) ref.update((byte) b);
		ref.update(tail);
		byte[] expected = ref.digest();

		byte[] key = new byte[16];
		byte[] iv = new byte[12];
		for (int i = 0; i < 16; i++) key[i] = (byte) (i * 7 + 1);
		for (int i = 0; i < 12; i++) iv[i] = (byte) (i * 3 + 2);
		SecretKeySpec ks = new SecretKeySpec(key, "AES");
		byte[] plain = "the same key and nonce, every round".getBytes(StandardCharsets.US_ASCII);

		KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
		kpg.initialize(2048);
		KeyPair kp = kpg.generateKeyPair();
		byte[] privDer = kp.getPrivate().getEncoded();
		byte[] pubDer = kp.getPublic().getEncoded();
		kp = null;
		KeyFactory kf = KeyFactory.getInstance("RSA");

		Runtime rt = Runtime.getRuntime();
		long first = -1, last = -1;
		int bad = 0;
		for (int i = 1; i <= n; i++) {
			MessageDigest md = MessageDigest.getInstance("SHA-256");
			md.update(head);
			for (int b = 0; b < 64; b++) md.update((byte) b);
			md.update(ByteBuffer.wrap(tail));
			MessageDigest copy = (MessageDigest) md.clone();
			if (!Arrays.equals(md.digest(), expected) || !Arrays.equals(copy.digest(), expected)) {
				bad++;
				if (bad <= 5) System.out.println("digest wrong at round " + i);
			}

			Cipher enc = Cipher.getInstance("AES/GCM/NoPadding");
			enc.init(Cipher.ENCRYPT_MODE, ks, new GCMParameterSpec(128, iv));
			byte[] ct = enc.doFinal(plain);
			Cipher dec = Cipher.getInstance("AES/GCM/NoPadding");
			dec.init(Cipher.DECRYPT_MODE, ks, new GCMParameterSpec(128, iv));
			ByteBuffer out = ByteBuffer.allocate(plain.length + 16);
			dec.doFinal(ByteBuffer.wrap(ct), out);
			out.flip();
			byte[] back = new byte[out.remaining()];
			out.get(back);
			if (!Arrays.equals(back, plain)) {
				bad++;
				if (bad <= 5) System.out.println("cipher round trip wrong at round " + i);
			}

			if (i % 250 == 0) {
				Signature s = Signature.getInstance("SHA256withRSA");
				s.initSign(kf.generatePrivate(new PKCS8EncodedKeySpec(privDer)));
				System.gc();
				s.update(plain);
				byte[] sig = s.sign();
				PublicKey pub = kf.generatePublic(new X509EncodedKeySpec(pubDer));
				Signature v = Signature.getInstance("SHA256withRSA");
				v.initVerify(pub);
				v.update(plain);
				if (!v.verify(sig)) {
					bad++;
					if (bad <= 5) System.out.println("signature with a collected key failed at round " + i);
				}
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
