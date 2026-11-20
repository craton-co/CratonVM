    use super::{start, StartConnect};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::net::TcpListener;

    #[test]
    fn closed_loopback_port_reports_connection_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback probe");
        let addr = listener.local_addr().expect("loopback address");
        drop(listener);

        match start(&addr) {
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused),
            Ok(StartConnect::Connected(_)) => panic!("closed loopback port connected"),
            Ok(StartConnect::DeferredFailure(_, error)) => {
                assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
            }
            Ok(StartConnect::InProgress(_)) => {
                panic!("closed loopback port was not classified as a deferred refusal")
            }
        }
    }
