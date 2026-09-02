import java.security.NoSuchAlgorithmException;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.spec.SecretKeySpec;
import java.util.HexFormat;
import sun.security.internal.spec.*;

/**
 * The five `SunTls*` `KeyGenerator` services, driven with fixed inputs.
 *
 * These were the last `KeyGenerator` names on HotSpot's SunJCE list that this
 * VM did not advertise, and the reason was structural rather than clerical:
 * they take `TlsKeyMaterialParameterSpec`-family specs that the engine's
 * two-field synthetic `KeyGenerator` cannot carry, so serving them meant
 * handing back a REAL `javax.crypto.KeyGenerator` over the platform's SPI and
 * teaching every native on that class to recognise a receiver it did not build.
 *
 * **A `getInstance` that resolves proves nothing here.** TLS 1.2's PRF and the
 * master-secret and key-material derivations are all DETERMINISTIC given
 * (secret, label, seed), so four of the five rows diff BYTE FOR BYTE against
 * HotSpot — which is what catches a generator that was routed to but never
 * initialised, or initialised with a spec whose fields were read in the wrong
 * order. `SunTlsRsaPremasterSecret` draws randomness, so only its length and
 * its two version bytes are fixed; those are printed instead.
 *
 * The specs live in `sun.security.internal.spec`, which `java.base` does not
 * export, so both VMs need
 *
 *   --add-exports java.base/sun.security.internal.spec=ALL-UNNAMED
 *
 * That is also the honest statement of who calls these: `sun.security.ssl`'s
 * own handshake, not application code.
 */
public final class JcaSunTlsVectors {
    static final HexFormat HEX = HexFormat.of();
    static final byte[] SECRET = HEX.parseHex(
            "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b" +
            "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
    static final byte[] SEED = HEX.parseHex("000102030405060708090a0b0c0d0e0f10111213");
    static final byte[] CR = HEX.parseHex("a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebf");
    static final byte[] SR = HEX.parseHex("c0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedf");

    public static void main(String[] args) {
        row("SunTlsPrf", () -> {
            KeyGenerator kg = KeyGenerator.getInstance("SunTlsPrf");
            kg.init(new TlsPrfParameterSpec(new SecretKeySpec(SECRET, "TlsPrf"),
                    "test label", SEED, 48, "SHA-256", 32, 64));
            return HEX.formatHex(kg.generateKey().getEncoded());
        });
        row("SunTls12Prf", () -> {
            KeyGenerator kg = KeyGenerator.getInstance("SunTls12Prf");
            kg.init(new TlsPrfParameterSpec(new SecretKeySpec(SECRET, "TlsPrf"),
                    "test label", SEED, 48, "SHA-256", 32, 64));
            return HEX.formatHex(kg.generateKey().getEncoded());
        });
        row("SunTlsMasterSecret", () -> {
            KeyGenerator kg = KeyGenerator.getInstance("SunTlsMasterSecret");
            SecretKey pre = new SecretKeySpec(HEX.parseHex(
                    "0303" + "00".repeat(46)), "TlsRsaPremasterSecret");
            kg.init(new TlsMasterSecretParameterSpec(pre, 3, 3, CR, SR, "SHA-256", 32, 64));
            SecretKey ms = kg.generateKey();
            return ms.getAlgorithm() + " " + HEX.formatHex(ms.getEncoded());
        });
        row("SunTlsKeyMaterial", () -> {
            KeyGenerator msg = KeyGenerator.getInstance("SunTlsMasterSecret");
            SecretKey pre = new SecretKeySpec(HEX.parseHex("0303" + "00".repeat(46)),
                    "TlsRsaPremasterSecret");
            msg.init(new TlsMasterSecretParameterSpec(pre, 3, 3, CR, SR, "SHA-256", 32, 64));
            SecretKey ms = msg.generateKey();
            KeyGenerator kg = KeyGenerator.getInstance("SunTlsKeyMaterial");
            // (masterSecret, major, minor, clientRandom, serverRandom,
            //  cipherAlgorithm, cipherKeyLength, expandedCipherKeyLength,
            //  ivLength, macKeyLength, prfHashAlg, prfHashLength, prfBlockSize)
            kg.init(new TlsKeyMaterialParameterSpec(ms, 3, 3, CR, SR,
                    "AES", 16, 0, 4, 32, "SHA-256", 32, 64));
            TlsKeyMaterialSpec km = (TlsKeyMaterialSpec) kg.generateKey();
            return "cw=" + HEX.formatHex(km.getClientCipherKey().getEncoded())
                 + " sw=" + HEX.formatHex(km.getServerCipherKey().getEncoded())
                 + " civ=" + HEX.formatHex(km.getClientIv().getIV())
                 + " siv=" + HEX.formatHex(km.getServerIv().getIV());
        });
        row("SunTlsRsaPremasterSecret", () -> {
            KeyGenerator kg = KeyGenerator.getInstance("SunTlsRsaPremasterSecret");
            kg.init(new TlsRsaPremasterSecretParameterSpec(3, 3));
            byte[] pms = kg.generateKey().getEncoded();
            // Randomised: only the length and the two version bytes are fixed.
            return "len=" + pms.length + " version=" + HEX.formatHex(
                    new byte[] {pms[0], pms[1]});
        });
    }

    interface Body { String run() throws Exception; }

    static void row(String label, Body b) {
        String v;
        try {
            v = b.run();
        } catch (Throwable t) {
            StringBuilder sb = new StringBuilder("ERROR " + t.getClass().getName() + ": " + t.getMessage());
            for (Throwable c = t.getCause(); c != null; c = c.getCause())
                sb.append(" <- ").append(c.getClass().getName()).append(": ").append(c.getMessage());
            v = sb.toString();
        }
        System.out.println(label + " | " + v);
    }
}
