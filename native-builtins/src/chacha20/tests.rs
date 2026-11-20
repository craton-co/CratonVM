use super::*;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hexstr(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn key32() -> [u8; 32] {
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = i as u8;
    }
    k
}

/// RFC 8439 §2.3.2 — the block function's own test vector.
#[test]
fn rfc8439_2_3_2_block_function() {
    let nonce = hex("000000090000004a00000000");
    let mut n = [0u8; 12];
    n.copy_from_slice(&nonce);
    let out = chacha20_block(&key32(), &n, 1);
    assert_eq!(
        hexstr(&out),
        "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e\
         d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
    );
}

/// RFC 8439 §2.4.2 — the full encryption vector, counter starting at 1.
///
/// NOTE the nonce: §2.4.2's is `00:00:00:00:00:00:00:4a:00:00:00:00`, which
/// is NOT §2.3.2's `00:00:00:09:...`. Getting that wrong here produced a
/// ciphertext that matched SunJCE exactly (both were asked the same wrong
/// question) while failing the RFC — which is the argument for keeping BOTH
/// oracles: a cross-VM diff agrees on a shared misreading, a published
/// vector does not.
#[test]
fn rfc8439_2_4_2_encryption() {
    let mut n = [0u8; 12];
    n.copy_from_slice(&hex("000000000000004a00000000"));
    let pt = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
    let ct = chacha20_apply(&key32(), &n, 1, pt);
    assert_eq!(
        hexstr(&ct),
        "6e2e359a2568f98041ba0728dd0d6981e97e7aec1d4360c20a27afccfd9fae0b\
         f91b65c5524733ab8f593dabcd62b3571639d624e65152ab8f530c359f0861d8\
         07ca0dbf500d6a6156a38e088a22b65e52bc514d16ccf806818ce91ab7793736\
         5af90bbf74a35be6b40b8eedf2785e42874d"
    );
    // Decryption is the same operation.
    let back = chacha20_apply(&key32(), &n, 1, &ct);
    assert_eq!(back, pt.to_vec());
}

/// RFC 8439 §2.5.2 — Poly1305 on its own.
#[test]
fn rfc8439_2_5_2_poly1305() {
    let mut k = [0u8; 32];
    k.copy_from_slice(&hex(
        "85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b",
    ));
    let msg = b"Cryptographic Forum Research Group";
    assert_eq!(
        hexstr(&poly1305(&k, msg)),
        "a8061dc1305136c6c22b8baf0c0127a9"
    );
}

/// RFC 8439 §2.6.2 — the AEAD's one-time key generation.
#[test]
fn rfc8439_2_6_2_poly1305_key_gen() {
    let mut k = [0u8; 32];
    k.copy_from_slice(&hex(
        "808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f",
    ));
    let mut n = [0u8; 12];
    n.copy_from_slice(&hex("000000000001020304050607"));
    assert_eq!(
        hexstr(&poly1305_key_gen(&k, &n)),
        "8ad5a08b905f81cc815040274ab29471a833b637e3fd0da508dbb8e2fdd1a646"
    );
}

fn aead_vector() -> ([u8; 32], [u8; 12], Vec<u8>, Vec<u8>) {
    let mut k = [0u8; 32];
    k.copy_from_slice(&hex(
        "808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f",
    ));
    let mut n = [0u8; 12];
    n.copy_from_slice(&hex("070000004041424344454647"));
    let aad = hex("50515253c0c1c2c3c4c5c6c7");
    let pt = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.".to_vec();
    (k, n, aad, pt)
}

/// RFC 8439 §2.8.2 — the AEAD vector, ciphertext AND tag.
#[test]
fn rfc8439_2_8_2_aead() {
    let (k, n, aad, pt) = aead_vector();
    let (ct, tag) = chacha20_poly1305_encrypt(&k, &n, &aad, &pt);
    assert_eq!(
        hexstr(&ct),
        "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6\
         3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36\
         92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc\
         3ff4def08e4b7a9de576d26586cec64b6116"
    );
    assert_eq!(hexstr(&tag), "1ae10b594f09e26a7e902ecbd0600691");
    assert_eq!(
        chacha20_poly1305_decrypt(&k, &n, &aad, &ct, &tag).unwrap(),
        pt
    );
}

/// The property the whole change exists for: a modified ciphertext, a
/// modified tag or a modified AAD must FAIL, and must yield no plaintext.
#[test]
fn aead_rejects_every_tampering() {
    let (k, n, aad, pt) = aead_vector();
    let (ct, tag) = chacha20_poly1305_encrypt(&k, &n, &aad, &pt);

    let mut bad_ct = ct.clone();
    bad_ct[0] ^= 1;
    assert!(chacha20_poly1305_decrypt(&k, &n, &aad, &bad_ct, &tag).is_err());

    let mut bad_tag = tag;
    bad_tag[15] ^= 1;
    assert!(chacha20_poly1305_decrypt(&k, &n, &aad, &ct, &bad_tag).is_err());

    let mut bad_aad = aad.clone();
    bad_aad[0] ^= 1;
    assert!(chacha20_poly1305_decrypt(&k, &n, &bad_aad, &ct, &tag).is_err());

    // Truncating the AAD must not be silently equivalent to padding it —
    // this is what the explicit length block in `aead_mac_data` prevents.
    assert!(chacha20_poly1305_decrypt(&k, &n, &aad[..aad.len() - 1], &ct, &tag).is_err());

    // A different nonce is a different keystream AND a different one-time key.
    let mut other_nonce = n;
    other_nonce[0] ^= 1;
    assert!(chacha20_poly1305_decrypt(&k, &other_nonce, &aad, &ct, &tag).is_err());
}

/// Empty plaintext and empty AAD are both legal and both authenticated.
#[test]
fn aead_handles_empty_inputs() {
    let (k, n, _, _) = aead_vector();
    let (ct, tag) = chacha20_poly1305_encrypt(&k, &n, &[], &[]);
    assert!(ct.is_empty());
    assert_eq!(
        chacha20_poly1305_decrypt(&k, &n, &[], &ct, &tag).unwrap(),
        Vec::<u8>::new()
    );
    // …and the tag still authenticates: flip it and the decrypt fails.
    let mut bad = tag;
    bad[0] ^= 0x80;
    assert!(chacha20_poly1305_decrypt(&k, &n, &[], &ct, &bad).is_err());
}

/// Poly1305 edge cases the 26-bit limb reduction gets wrong if the final
/// carry/select is missed: an accumulator that lands exactly on 2^130 - 5,
/// and one that must wrap past 2^128 when `s` is added.
#[test]
fn poly1305_reduction_edges() {
    // RFC 8439 §A.3 test vector #2: r = 0 means the tag is just s.
    let mut k = [0u8; 32];
    k[16..].copy_from_slice(&hex("36e5f6b5c5e06070f0efca96227a863e"));
    let msg = hex(
        "416e79207375626d697373696f6e20746f20746865204945544620696e74656e6465642062792074686520\
         436f6e7472696275746f7220666f72207075626c69636174696f6e20617320616c6c206f722070617274206\
         f6620616e204945544620496e7465726e65742d4472616674206f722052464320616e6420616e7920737461\
         74656d656e74206d6164652077697468696e2074686520636f6e74657874206f6620616e20494554462061\
         6374697669747920697320636f6e7369646572656420616e20224945544620436f6e747269627574696f6e\
         222e20537563682073746174656d656e747320696e636c756465206f72616c2073746174656d656e747320\
         696e20494554462073657373696f6e732c2061732077656c6c206173207772697474656e20616e6420656c\
         656374726f6e696320636f6d6d756e69636174696f6e73206d61646520617420616e792074696d65206f72\
         20706c6163652c207768696368206172652061646472657373656420746f",
    );
    assert_eq!(
        hexstr(&poly1305(&k, &msg)),
        "36e5f6b5c5e06070f0efca96227a863e"
    );

    // §A.3 #3: s = 0, so the tag is the reduced accumulator alone.
    let mut k3 = [0u8; 32];
    k3[..16].copy_from_slice(&hex("36e5f6b5c5e06070f0efca96227a863e"));
    assert_eq!(
        hexstr(&poly1305(&k3, &msg)),
        "f3477e7cd95417af89a6b8794c310cf0"
    );
}

/// The counter is honoured, and it is what makes ChaCha20 seekable: the
/// keystream at counter N is the tail of the keystream from counter 0.
#[test]
fn counter_selects_the_keystream_block() {
    let mut n = [0u8; 12];
    n.copy_from_slice(&hex("000000090000004a00000000"));
    let from0 = chacha20_apply(&key32(), &n, 0, &[0u8; 192]);
    let from1 = chacha20_apply(&key32(), &n, 1, &[0u8; 128]);
    assert_eq!(&from0[64..], &from1[..]);
    // A counter of 1 is NOT the same as a counter of 0 — the defect this
    // module replaces discarded the nonce and the counter alike.
    assert_ne!(&from0[..64], &from1[..64]);
}

/// DIFFERENTIAL: this module against the `chacha20poly1305` crate, which
/// has been a normal dependency of `native-builtins` the whole time
/// (`Cargo.toml`, `[dependencies]`) and which `t2_6_5_chacha20_poly1305_
/// round_trip` already exercises — directly, never through
/// `javax.crypto.Cipher`, which is why that acceptance test stayed green
/// while `Cipher` was doing AES-256-ECB.
///
/// Hand-written Poly1305 is exactly where a subtle carry bug hides, and the
/// RFC vectors above only pin the inputs the RFC chose. This crosses the
/// two implementations over inputs the RFC does not cover: every length
/// around the 16-byte MAC block and 64-byte keystream boundaries, with and
/// without AAD.
#[test]
fn agrees_with_the_chacha20poly1305_crate() {
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

    let key = key32();
    let mut n = [0u8; 12];
    n.copy_from_slice(&hex("070000004041424344454647"));
    let reference = ChaCha20Poly1305::new(Key::from_slice(&key));
    let nonce = Nonce::from_slice(&n);

    for len in [
        0usize, 1, 15, 16, 17, 31, 32, 63, 64, 65, 127, 128, 129, 200,
    ] {
        let pt: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
        for aad_len in [0usize, 1, 15, 16, 17, 48] {
            let aad: Vec<u8> = (0..aad_len).map(|i| (i * 11 + 5) as u8).collect();
            let (ct, tag) = chacha20_poly1305_encrypt(&key, &n, &aad, &pt);
            let mut ours = ct.clone();
            ours.extend_from_slice(&tag);
            let theirs = reference
                .encrypt(
                    nonce,
                    Payload {
                        msg: &pt,
                        aad: &aad,
                    },
                )
                .expect("reference encrypt");
            assert_eq!(
                hexstr(&ours),
                hexstr(&theirs),
                "pt_len={len} aad_len={aad_len}"
            );
            // …and ours decrypts what theirs produced.
            let split = theirs.len() - 16;
            let mut t = [0u8; 16];
            t.copy_from_slice(&theirs[split..]);
            assert_eq!(
                chacha20_poly1305_decrypt(&key, &n, &aad, &theirs[..split], &t).unwrap(),
                pt,
                "pt_len={len} aad_len={aad_len}"
            );
        }
    }
}

/// A message that is not a multiple of 64 bytes must use only as much of
/// the last keystream block as it needs.
#[test]
fn partial_final_block() {
    let mut n = [0u8; 12];
    n.copy_from_slice(&hex("000000090000004a00000000"));
    let full = chacha20_apply(&key32(), &n, 1, &[0u8; 64]);
    let part = chacha20_apply(&key32(), &n, 1, &[0u8; 5]);
    assert_eq!(&full[..5], &part[..]);
    assert_eq!(part.len(), 5);
}
