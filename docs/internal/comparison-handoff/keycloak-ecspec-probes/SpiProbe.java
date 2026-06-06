import java.lang.reflect.*;
import java.security.*;
import java.security.spec.ECGenParameterSpec;

// Feasibility: can we drive the real sun.security.ec.ECKeyPairGenerator SPI
// directly in DEFAULT (synthetic) mode? If yes, the synthetic KPG can delegate
// EC to it instead of returning bare keys.
public class SpiProbe {
    public static void main(String[] a) {
        try {
            Class<?> spiCls = Class.forName("sun.security.ec.ECKeyPairGenerator");
            System.err.println("loaded " + spiCls);
            Constructor<?> ctor = spiCls.getDeclaredConstructor();
            ctor.setAccessible(true);
            Object spi = ctor.newInstance();
            System.err.println("instantiated SPI");
            // KeyPairGeneratorSpi.initialize(AlgorithmParameterSpec, SecureRandom)
            Method init = spiCls.getMethod("initialize", java.security.spec.AlgorithmParameterSpec.class, SecureRandom.class);
            init.invoke(spi, new ECGenParameterSpec("secp256r1"), new SecureRandom());
            System.err.println("initialized SPI");
            Method gen = spiCls.getMethod("generateKeyPair");
            KeyPair kp = (KeyPair) gen.invoke(spi);
            System.err.println("pub class = " + kp.getPublic().getClass().getName());
            java.security.interfaces.ECPublicKey pub = (java.security.interfaces.ECPublicKey) kp.getPublic();
            System.err.println("params bits = " + pub.getParams().getCurve().getField().getFieldSize());
            System.err.println("RESULT OK");
        } catch (Throwable t) {
            System.err.println("RESULT FAIL: " + t);
            Throwable c = t.getCause(); int d = 0;
            while (c != null && d++ < 8) { System.err.println("  caused by: " + c); c = c.getCause(); }
        }
        System.err.flush();
    }
}
