//! Manual interop validation for T-CBC.1 (see
//! rustls-cbc-cipher-suites-not-supported.md).
//!
//! Not a `#[test]` on purpose: it needs an external, independent TLS peer
//! (the system `openssl` CLI) to prove the key-schedule/record-layer
//! implementation is actually RFC5246-compliant, not merely self-consistent
//! with itself. Run with:
//!
//!   cargo run --example cbc_interop_server -- <cert.pem> <key.pem> <port>
//!
//! then, from another shell:
//!
//!   echo hello | openssl s_client -connect 127.0.0.1:<port> \
//!       -cipher ECDHE-RSA-AES128-SHA256 -tls1_2 -quiet

use std::io::{BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

use rustls::{ServerConfig, ServerConnection, SupportedCipherSuite};

fn main() {
    let mut args = std::env::args().skip(1);
    let cert_path = args.next().expect("usage: cert.pem key.pem port");
    let key_path = args.next().expect("usage: cert.pem key.pem port");
    let port: u16 = args
        .next()
        .expect("usage: cert.pem key.pem port")
        .parse()
        .expect("port must be a number");

    let certs = rustls_pemfile::certs(&mut BufReader::new(
        std::fs::File::open(&cert_path).expect("open cert.pem"),
    ))
    .collect::<Result<Vec<_>, _>>()
    .expect("parse cert.pem");
    let key = rustls_pemfile::private_key(&mut BufReader::new(
        std::fs::File::open(&key_path).expect("open key.pem"),
    ))
    .expect("parse key.pem")
    .expect("key.pem contained no private key");

    let mut provider = rustls::crypto::ring::default_provider();
    provider.cipher_suites = vec![SupportedCipherSuite::from(
        &cratonvm_native_builtins::t27_tls_cbc::TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256,
    )];

    let config = ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS12])
        .expect("TLS1.2 usable with the CBC suite")
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("with_single_cert");

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    eprintln!("[cbc_interop_server] listening on 127.0.0.1:{port}, waiting for one connection");
    let (mut stream, peer) = listener.accept().expect("accept");
    eprintln!("[cbc_interop_server] accepted connection from {peer}");

    let mut conn = ServerConnection::new(Arc::new(config)).expect("ServerConnection::new");
    let mut tls = rustls::Stream::new(&mut conn, &mut stream);

    let mut buf = [0u8; 1024];
    let n = tls.read(&mut buf).expect("read from client");
    eprintln!(
        "[cbc_interop_server] received {} bytes: {:?}",
        n,
        String::from_utf8_lossy(&buf[..n])
    );

    tls.write_all(b"hello from cratonvm TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256 server\n")
        .expect("write to client");
    tls.flush().expect("flush");

    eprintln!(
        "[cbc_interop_server] negotiated protocol version: {:?}, cipher suite: {:?}",
        conn.protocol_version(),
        conn.negotiated_cipher_suite().map(|cs| cs.suite())
    );
    eprintln!("[cbc_interop_server] OK: handshake + data exchange succeeded against a real peer");
}
