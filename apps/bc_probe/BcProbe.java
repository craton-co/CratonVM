import java.security.Security;
import org.bouncycastle.jce.provider.BouncyCastleProvider;
import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;

public class BcProbe {
    public static void main(String[] args) throws Exception {
        Security.addProvider(new BouncyCastleProvider());
        System.out.println("bc.added providers=" + Security.getProviders().length);

        KeyGenerator kg = KeyGenerator.getInstance("AES", "BC");
        kg.init(128);
        SecretKey key = kg.generateKey();
        System.out.println("key.algo=" + key.getAlgorithm() + " len=" + key.getEncoded().length);

        Cipher c = Cipher.getInstance("AES/ECB/PKCS7Padding", "BC");
        c.init(Cipher.ENCRYPT_MODE, key);
        byte[] ct = c.doFinal("hello-bouncycastle".getBytes("UTF-8"));
        System.out.println("ct.len=" + ct.length);
        System.out.println("BcProbe: PASS");
    }
}
