use super::*;

#[test]
fn handshake_bytes_are_correct() {
    assert_eq!(JDWP_HANDSHAKE.len(), 14);
    assert_eq!(JDWP_HANDSHAKE, b"JDWP-Handshake");
}

#[test]
fn handshake_roundtrip_over_tcp() {
    // Bind to an ephemeral port.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        perform_handshake(stream).unwrap()
    });

    // Client side: connect and send the handshake.
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    client.write_all(JDWP_HANDSHAKE).unwrap();
    client.flush().unwrap();
    let mut resp = [0u8; 14];
    client.read_exact(&mut resp).unwrap();
    assert_eq!(&resp, JDWP_HANDSHAKE);

    // Server thread should have succeeded.
    let _conn = server.join().unwrap();
}

#[test]
fn bad_handshake_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        perform_handshake(stream)
    });

    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    client.write_all(b"NOT-A-HANDSHAK").unwrap();
    client.flush().unwrap();

    let result = server.join().unwrap();
    assert!(result.is_err());
}
