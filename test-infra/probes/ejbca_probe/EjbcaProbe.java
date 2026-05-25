import org.bouncycastle.jce.provider.BouncyCastleProvider;
import org.bouncycastle.asn1.x500.X500Name;
import java.security.Security;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.Provider;
public class EjbcaProbe {
    public static void main(String[] args) throws Exception {
        // EJBCA ships its own BouncyCastle build; verify it can be
        // registered + used to generate a real key pair + an X500 name.
        BouncyCastleProvider bc = new BouncyCastleProvider();
        Security.addProvider(bc);
        System.out.println("BC provider: " + bc.getName() + " v" + bc.getVersionStr());

        // Build an X500 name (used by EJBCA for every subject/issuer DN).
        X500Name dn = new X500Name("CN=Probe, O=CratonVM, C=US");
        System.out.println("X500: " + dn);
        if (!dn.toString().contains("CN=Probe")) {
            System.out.println("FAIL: DN format"); System.exit(1);
        }

        // Generate a 1024-bit RSA pair via BC.
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA", bc);
        kpg.initialize(1024);
        KeyPair kp = kpg.generateKeyPair();
        System.out.println("RSA pubkey algo: " + kp.getPublic().getAlgorithm() + " format: " + kp.getPublic().getFormat());
        if (!"RSA".equals(kp.getPublic().getAlgorithm())) {
            System.out.println("FAIL: pubkey algo"); System.exit(1);
        }

        System.out.println("OK");
        System.exit(0);
    }
}
