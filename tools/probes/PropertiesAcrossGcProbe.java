import java.security.spec.InvalidKeySpecException;
import java.util.Properties;
import javax.crypto.SecretKeyFactory;
import javax.crypto.spec.PBEKeySpec;

/**
 * gc-common w15-b: the native `Properties` side map and the
 * `SecretKeyFactory` PRF / algorithm tables are keyed through the lock-key
 * registry, which is swept after every collection. A dead object's rows go at
 * that sweep, and a live one's slot is re-addressed. This probe holds a
 * `Properties` object and two PBKDF2 factories across collections that churn
 * thousands of short-lived `Properties` objects and factories. It then checks
 * that the held ones keep their state: the property, the derived keys, and
 * `getAlgorithm()`. The churn also exercises the weak-track queue's periodic
 * drain.
 *
 * It then prints the PBKDF2 edge cases w15-b changed, which are compared
 * against HotSpot:
 * - a `PBEKeySpec` with no salt (`InvalidKeySpecException`);
 * - a password with a supplementary character;
 * - a password with a lone surrogate.
 *
 * Prints one line per round and a final verdict. The output should match
 * HotSpot's. Ends on its own. Arg 0: short-lived objects per round (default
 * 20000).
 */
public final class PropertiesAcrossGcProbe {
	static volatile Object sink;

	static String hex(byte[] b) {
		StringBuilder s = new StringBuilder();
		for (byte x : b) s.append(String.format("%02x", x & 0xff));
		return s.toString();
	}

	static byte[] salt() throws Exception {
		return "salt".getBytes("US-ASCII");
	}

	static String derive(SecretKeyFactory f, String pw, int iters, int bits) throws Exception {
		return hex(f.generateSecret(new PBEKeySpec(pw.toCharArray(), salt(), iters, bits)).getEncoded());
	}

	public static void main(String[] a) throws Exception {
		int n = a.length > 0 ? Integer.parseInt(a[0]) : 20000;
		Properties held = new Properties();
		held.setProperty("hibernate.connection.password", "secret");
		SecretKeyFactory sha1 = SecretKeyFactory.getInstance("PBKDF2WithHmacSHA1");
		SecretKeyFactory sha256 = SecretKeyFactory.getInstance("PBKDF2WithHmacSHA256");
		String k1 = derive(sha1, "password", 2, 160);
		String k256 = derive(sha256, "password", 1, 256);
		System.out.println("sha1   " + k1);
		System.out.println("sha256 " + k256);
		int bad = 0;
		for (int round = 1; round <= 5; round++) {
			long total = 0;
			for (int i = 0; i < n; i++) {
				Properties p = new Properties();
				p.setProperty("k", "v" + i);
				total += p.getProperty("k").length();
				if ((i & 1023) == 0) {
					SecretKeyFactory f = SecretKeyFactory.getInstance(
							(i & 2048) == 0 ? "PBKDF2WithHmacSHA512" : "PBKDF2WithHmacSHA384");
					total += f.getAlgorithm().length();
				}
			}
			sink = total;
			System.gc();
			int before = bad;
			if (!"secret".equals(held.getProperty("hibernate.connection.password"))) bad++;
			if (!k1.equals(derive(sha1, "password", 2, 160))) bad++;
			if (!k256.equals(derive(sha256, "password", 1, 256))) bad++;
			if (!"PBKDF2WithHmacSHA1".equals(sha1.getAlgorithm())) bad++;
			if (!"PBKDF2WithHmacSHA256".equals(sha256.getAlgorithm())) bad++;
			System.out.println("round " + round + (bad == before ? ": held state intact" : ": HELD STATE LOST"));
		}
		try {
			sha1.generateSecret(new PBEKeySpec("pw".toCharArray()));
			System.out.println("no salt: no exception");
		} catch (InvalidKeySpecException e) {
			System.out.println("no salt: InvalidKeySpecException: " + e.getMessage());
		} catch (RuntimeException e) {
			System.out.println("no salt: " + e.getClass().getName());
		}
		System.out.println("supplementary " + derive(sha256, "a😀", 1, 256));
		System.out.println("lone surrogate " + derive(sha256, "\uD83Da", 1, 256));
		System.out.println(bad == 0 ? "PropertiesAcrossGcProbe: OK" : "PropertiesAcrossGcProbe: FAIL " + bad);
	}
}
