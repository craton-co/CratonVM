import java.nio.file.*;
import java.io.*;
import java.security.*;
import java.security.spec.*;
import java.util.*;
import javax.crypto.*;
import javax.crypto.spec.*;

/** Generate fixtures with HotSpot, compare check stdout across VMs, then verify VM signatures with HotSpot. */
public class Avx512CryptoProbe {
    static final byte[] MSG = "CratonVM AVX-512 crypto parity 2026-10-31".getBytes(java.nio.charset.StandardCharsets.UTF_8);
    static Path root;
    static byte[] read(String name) throws Exception { return Files.readAllBytes(root.resolve(name)); }
    static void write(String name, byte[] b) throws Exception { Files.write(root.resolve(name), b); }
    static PrivateKey pri(String alg, String prefix) throws Exception { return KeyFactory.getInstance(alg).generatePrivate(new PKCS8EncodedKeySpec(read(prefix+".pri"))); }
    static PublicKey pub(String alg, String prefix) throws Exception { return KeyFactory.getInstance(alg).generatePublic(new X509EncodedKeySpec(read(prefix+".pub"))); }
    static byte[] sign(String alg, PrivateKey k) throws Exception { Signature s=Signature.getInstance(alg); s.initSign(k); s.update(MSG); return s.sign(); }
    static boolean verify(String alg, PublicKey k, byte[] sig) throws Exception { Signature s=Signature.getInstance(alg); s.initVerify(k); s.update(MSG); return s.verify(sig); }
    static byte[] agree(PrivateKey sk, PublicKey pk) throws Exception { KeyAgreement a=KeyAgreement.getInstance("ECDH"); a.init(sk); a.doPhase(pk,true); return a.generateSecret(); }
    static String hash(byte[] b) throws Exception { return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(b)); }
    static void save(String prefix, KeyPair p) throws Exception { write(prefix+".pri",p.getPrivate().getEncoded()); write(prefix+".pub",p.getPublic().getEncoded()); }
    static void prepare() throws Exception {
        for (int bits : new int[]{2048,4096}) { KeyPairGenerator g=KeyPairGenerator.getInstance("RSA"); g.initialize(bits); KeyPair k=g.generateKeyPair(); String p="rsa"+bits; save(p,k); write(p+".sig",sign("SHA256withRSA",k.getPrivate())); }
        KeyPairGenerator g=KeyPairGenerator.getInstance("EC"); g.initialize(new ECGenParameterSpec("secp256r1")); KeyPair a=g.generateKeyPair(), b=g.generateKeyPair(); save("ec",a); save("ec2",b); write("ec.sig",sign("SHA256withECDSA",a.getPrivate())); write("ec.secret",agree(a.getPrivate(),b.getPublic()));
    }
    static AlgorithmParameterSpec params(String mode, byte[] iv) { return mode.equals("GCM") ? new GCMParameterSpec(128,iv) : new IvParameterSpec(iv); }
    static Cipher cipher(String mode, int op, SecretKeySpec key, byte[] iv) throws Exception { Cipher c=Cipher.getInstance("AES/"+mode+"/"+(mode.equals("ECB")||mode.equals("CBC")?"PKCS5Padding":"NoPadding")); if(mode.equals("ECB")) c.init(op,key); else c.init(op,key,params(mode,iv)); return c; }
    static void check(String output) throws Exception {
        for (int bits : new int[]{2048,4096}) {
            String p="rsa"+bits; byte[] sig=sign("SHA256withRSA",pri("RSA",p));
            if (!Arrays.equals(sig,read(p+".sig")) || !verify("SHA256withRSA",pub("RSA",p),sig)) throw new AssertionError(p);
            sig[sig.length-1]^=1; if(verify("SHA256withRSA",pub("RSA",p),sig)) throw new AssertionError("bad RSA signature accepted");
            System.out.println(p+" "+hash(read(p+".sig")));
        }
        byte[] secret=agree(pri("EC","ec"),pub("EC","ec2"));
        if (!Arrays.equals(secret,read("ec.secret")) || !verify("SHA256withECDSA",pub("EC","ec"),read("ec.sig"))) throw new AssertionError("EC");
        byte[] sig=sign("SHA256withECDSA",pri("EC","ec")); if(!verify("SHA256withECDSA",pub("EC","ec"),sig)) throw new AssertionError("EC self verify");
        Files.createDirectories(Path.of(output)); Files.write(Path.of(output,"ec.sig"),sig);
        System.out.println("ec "+hash(secret)+" signature_verified");
        for(int bits:new int[]{128,192,256}) for(String mode:new String[]{"ECB","CBC","CTR","GCM"}) {
            byte[] kb=new byte[bits/8]; for(int i=0;i<kb.length;i++)kb[i]=(byte)(i*13+7); SecretKeySpec key=new SecretKeySpec(kb,"AES");
            byte[] iv=new byte[mode.equals("GCM")?12:16]; Arrays.fill(iv,(byte)0xf3);
            MessageDigest digest=MessageDigest.getInstance("SHA-256");
            for(int n:new int[]{0,1,15,16,17,63,64,65,127,128,129,1024,4097}) {
                byte[] data=new byte[n]; for(int i=0;i<n;i++)data[i]=(byte)(i*73+11);
                byte[] encrypted=cipher(mode,Cipher.ENCRYPT_MODE,key,iv).doFinal(data);
                byte[] decrypted=cipher(mode,Cipher.DECRYPT_MODE,key,iv).doFinal(encrypted); if(!Arrays.equals(data,decrypted))throw new AssertionError(mode+" decrypt "+n);
                Cipher stream=cipher(mode,Cipher.ENCRYPT_MODE,key,iv); ByteArrayOutputStream all=new ByteArrayOutputStream();
                int off=0; for(int step:new int[]{3,61,n}) { int take=Math.min(step,n-off); byte[] part=stream.update(data,off,take); if(part!=null)all.write(part); off+=take; }
                all.write(stream.doFinal()); if(!Arrays.equals(encrypted,all.toByteArray()))throw new AssertionError(mode+" stream "+n);
                Cipher alias=cipher(mode,Cipher.ENCRYPT_MODE,key,iv); byte[] buf=new byte[Math.max(n+3,alias.getOutputSize(n)+5)]; System.arraycopy(data,0,buf,3,n);
                int len=alias.doFinal(buf,3,n,buf,5); if(!Arrays.equals(encrypted,Arrays.copyOfRange(buf,5,5+len)))throw new AssertionError(mode+" alias "+n);
                digest.update(encrypted);
            }
            System.out.println("aes"+bits+" "+mode+" "+HexFormat.of().formatHex(digest.digest()));
        }
        System.out.println("AVX512_CRYPTO_OK");
    }
    public static void main(String[] args) throws Exception {
        root=Path.of(args[1]);
        if(args[0].equals("prepare")){ prepare(); return; }
        if(args[0].equals("verify")){ if(!verify("SHA256withECDSA",pub("EC","ec"),Files.readAllBytes(Path.of(args[2],"ec.sig"))))throw new AssertionError("cross VM ECDSA"); System.out.println("CROSS_VM_ECDSA_OK"); return; }
        check(args[2]);
    }
}
