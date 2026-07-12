import java.util.Arrays;
import javax.crypto.Cipher;
import javax.crypto.SecretKey;
import javax.crypto.spec.SecretKeySpec;

/**
 * Narrow runtime regression probe for Keycloak Elytron's A128KW path.
 *
 * Run with a CratonVM build and a real JDK home, for example:
 * {@code cratonvm --java-home <jdk> -cp . AesWrap128Probe}.
 */
public final class AesWrap128Probe {
    private static final byte[] KEK = {
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
        0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f
    };
    private static final byte[] CEK = {
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
        (byte) 0x88, (byte) 0x99, (byte) 0xaa, (byte) 0xbb,
        (byte) 0xcc, (byte) 0xdd, (byte) 0xee, (byte) 0xff
    };
    private static final byte[] RFC3394_WRAPPED = {
        0x1f, (byte) 0xa6, (byte) 0x8b, 0x0a, (byte) 0x81, 0x12, (byte) 0xb4, 0x47,
        (byte) 0xae, (byte) 0xf3, 0x4b, (byte) 0xd8, (byte) 0xfb, 0x5a, 0x7b, (byte) 0x82,
        (byte) 0x9d, 0x3e, (byte) 0x86, 0x23, 0x71, (byte) 0xd2, (byte) 0xcf, (byte) 0xe5
    };

    public static void main(String[] args) throws Exception {
        SecretKey kek = new SecretKeySpec(KEK, "AES");
        SecretKey cek = new SecretKeySpec(CEK, "AES");

        Cipher wrapper = Cipher.getInstance("AESWrap_128");
        wrapper.init(Cipher.WRAP_MODE, kek);
        byte[] wrapped = wrapper.wrap(cek);
        if (!Arrays.equals(wrapped, RFC3394_WRAPPED)) {
            throw new AssertionError("AESWrap_128 output differs from RFC 3394");
        }

        Cipher unwrapper = Cipher.getInstance("AESWrap_128");
        unwrapper.init(Cipher.UNWRAP_MODE, kek);
        SecretKey restored = (SecretKey) unwrapper.unwrap(wrapped, "AES", Cipher.SECRET_KEY);
        if (!Arrays.equals(restored.getEncoded(), CEK)) {
            throw new AssertionError("AESWrap_128 unwrap did not restore the CEK");
        }

        System.out.println("AES_WRAP_128_OK");
    }
}
