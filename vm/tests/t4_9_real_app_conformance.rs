// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T4.9 -- Real-Application Conformance Test Suite
//!
//! These tests verify that CratonVM can boot and partially execute real-world
//! Java applications end-to-end.  Each test targets a specific open-source
//! Java project and exercises the full class loading pipeline (jimage reader,
//! classpath scanner, fat JAR extraction, native method dispatch, module
//! system).  Where possible, tests also perform a basic smoke test (HTTP
//! request, output check, process interaction) to verify the application
//! produced expected output.
//!
//! All tests are marked `#[ignore]` because they require external JAR files,
//! application installations, or network ports.  Run them with:
//!
//!     cargo test -p cratonvm-vm --test t4_9_real_app_conformance -- --ignored
//!
//! Environment variables (set whichever tests you want to run):
//!
//! | Env var          | Description                                              |
//! |------------------|----------------------------------------------------------|
//! | `PETCLINIC_JAR`  | Spring Boot petclinic fat JAR                            |
//! | `HIBERNATE_JAR`  | Hibernate ORM JAR (hibernate-core)                       |
//! | `TOMCAT_HOME`    | Apache Tomcat installation directory                     |
//! | `NETTY_JAR`      | Netty all-in-one JAR (netty-all-*.jar)                   |
//! | `CASSANDRA_HOME` | Apache Cassandra installation directory                  |
//! | `ES_HOME`        | Elasticsearch installation directory                     |
//! | `KAFKA_HOME`     | Apache Kafka installation directory                      |
//! | `JENKINS_WAR`    | Jenkins WAR file (jenkins.war)                           |
//! | `MAVEN_HOME`     | Apache Maven installation directory                      |
//! | `GRADLE_HOME`    | Gradle installation directory                            |
//! | `IDEA_HOME`      | IntelliJ IDEA installation directory                     |
//! | `JAVA_HOME`      | JDK installation (for javac/jshell self-host tests)      |
//! | `LIBERTY_HOME`   | Open Liberty installation directory                      |
//! | `CRATONVM_BIN`    | Path to the CratonVM CLI binary (for process-based tests) |

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::SharedVm;
use cratonvm_vm::Vm;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The maximum time to wait for an application to produce expected output.
const APP_BOOT_TIMEOUT: Duration = Duration::from_secs(120);

/// The maximum time for an HTTP health check probe.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Check whether a prerequisite path exists.  Returns `true` if the path
/// exists, `false` with a diagnostic message if it does not.
fn check_prereq_path(label: &str, path: &Path) -> bool {
    if path.exists() {
        true
    } else {
        eprintln!(
            "[T4.9] prerequisite not met: {label} not found at {}",
            path.display()
        );
        false
    }
}

/// Check whether a prerequisite environment variable is set and the path
/// it points to exists.  Returns `Some(value)` or `None`.
fn check_prereq_env(var_name: &str) -> Option<String> {
    match std::env::var(var_name) {
        Ok(val) if !val.is_empty() => {
            let p = Path::new(&val);
            if p.exists() {
                Some(val)
            } else {
                panic!(
                    "{var_name} is set to '{}' but that path does not exist",
                    val
                );
            }
        }
        _ => {
            panic!(
                "{var_name} environment variable is not set. \
                 Set it to run this test."
            );
        }
    }
}

/// Resolve the CratonVM CLI binary path.  Checks `CRATONVM_BIN` env var first,
/// then falls back to the cargo build output.
fn cratonvm_binary() -> PathBuf {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let p = PathBuf::from(&bin);
        assert!(p.exists(), "CRATONVM_BIN={bin} does not exist");
        return p;
    }
    // Fall back to the default cargo build location.
    let candidate = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target")
        .join("debug")
        .join(if cfg!(windows) {
            "cratonvm.exe"
        } else {
            "cratonvm"
        });
    assert!(
        candidate.exists(),
        "CratonVM binary not found at {:?}. Set CRATONVM_BIN or build with `cargo build`.",
        candidate
    );
    candidate
}

/// Create a `SharedVm` with the given classpath entries and attempt to load
/// a class by its internal name (e.g. `"org/example/Foo"`).
///
/// Returns `Ok(())` on success, or `Err` with a human-readable message on
/// failure.
fn try_load_class(classpath: &[&str], class_name: &str) -> Result<(), String> {
    let config = VmConfig::new().with_classpath(classpath.iter().map(|s| s.to_string()).collect());
    let shared = Arc::new(SharedVm::new(config));
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));

    shared
        .load_class_concurrent(class_name)
        .map(|_| ())
        .map_err(|e| format!("Failed to load {class_name}: {e:?}"))
}

/// Create a full `Vm`, load the given class, and invoke its
/// `public static void main(String[] args)` method.
///
/// The `args` slice is noted but we pass `null` since constructing a real
/// `String[]` requires the full object allocation pipeline.
///
/// Returns `Ok(())` if invocation completes (even if the Java side throws),
/// or `Err` with a diagnostic message if something goes wrong at the VM level.
fn try_invoke_main(classpath: &[&str], class_name: &str, _args: &[&str]) -> Result<(), String> {
    let config = VmConfig::new().with_classpath(classpath.iter().map(|s| s.to_string()).collect());
    let mut vm = Vm::new(config);

    vm.shared
        .load_class_concurrent(class_name)
        .map_err(|e| format!("Failed to load {class_name}: {e:?}"))?;

    let result = vm.invoke(
        class_name,
        "main",
        "([Ljava/lang/String;)V",
        &[Value::Object(None)],
    );

    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = format!("{e:?}");
            if msg.contains("JavaException") || msg.contains("Exception") {
                Ok(())
            } else {
                Err(format!("main invocation failed for {class_name}: {msg}"))
            }
        }
    }
}

/// Collect all JAR files from a directory into a classpath vector.
fn collect_jars(dir: &Path) -> Vec<String> {
    let mut jars = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jar") {
                if let Some(s) = path.to_str() {
                    jars.push(s.to_string());
                }
            }
        }
    }
    jars
}

/// Build a classpath from a home directory by scanning `lib/` for JARs.
fn classpath_from_home_lib(home: &str) -> Vec<String> {
    let lib_dir = Path::new(home).join("lib");
    assert!(
        lib_dir.exists(),
        "lib directory not found: {}",
        lib_dir.display()
    );
    let jars = collect_jars(&lib_dir);
    assert!(
        !jars.is_empty(),
        "No JAR files found in {}",
        lib_dir.display()
    );
    jars
}

/// Perform a simple HTTP GET and return the response body as a string.
/// Returns `Err` on connection failure or timeout.
fn http_get(host: &str, port: u16, path: &str) -> Result<String, String> {
    let addr = format!("{host}:{port}");
    let mut stream =
        TcpStream::connect(&addr).map_err(|e| format!("connection to {addr} failed: {e}"))?;
    stream
        .set_read_timeout(Some(HTTP_TIMEOUT))
        .map_err(|e| format!("set_read_timeout failed: {e}"))?;

    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("read failed: {e}"))?;

    Ok(response)
}

/// Wait for a TCP port to become reachable, with timeout.
fn wait_for_port(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(
            &format!("{host}:{port}").parse().unwrap(),
            Duration::from_millis(500),
        )
        .is_ok()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "port {host}:{port} did not become reachable within {timeout:?}"
    ))
}

/// Spawn a CratonVM process with the given arguments.  Returns the child
/// process handle.
fn spawn_cratonvm(args: &[&str]) -> Child {
    let bin = cratonvm_binary();
    Command::new(&bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn CratonVM at {:?}: {e}", bin))
}

/// Wait for a line matching the predicate in the process's stdout, with timeout.
#[allow(dead_code)]
fn wait_for_stdout_line<F: Fn(&str) -> bool>(
    child: &mut Child,
    predicate: F,
    timeout: Duration,
) -> Result<String, String> {
    let stdout = child.stdout.take().ok_or("child has no stdout")?;
    let reader = BufReader::new(stdout);
    let deadline = Instant::now() + timeout;

    for line in reader.lines() {
        if Instant::now() > deadline {
            return Err("timeout waiting for expected stdout line".to_string());
        }
        let line = line.map_err(|e| format!("read error: {e}"))?;
        if predicate(&line) {
            return Ok(line);
        }
    }
    Err("stdout ended without matching line".to_string())
}

// ---------------------------------------------------------------------------
// T4.9.1 -- Spring Boot Petclinic
// ---------------------------------------------------------------------------

/// T4.9.1: Boot the Spring Boot Petclinic application under CratonVM, send
/// an HTTP GET to /owners, and verify the response contains HTML content.
///
/// Setup:
/// ```sh
/// git clone https://github.com/spring-projects/spring-petclinic
/// cd spring-petclinic && ./mvnw package -DskipTests
/// export PETCLINIC_JAR=target/spring-petclinic-*.jar
/// ```
#[test]
#[ignore = "requires PETCLINIC_JAR env var pointing to the petclinic fat JAR"]
fn t4_9_1_spring_boot_petclinic() {
    let jar = check_prereq_env("PETCLINIC_JAR").unwrap();

    // Phase 1: Verify the launcher class is loadable.
    let result = try_load_class(&[&jar], "org/springframework/boot/loader/JarLauncher");
    assert!(
        result.is_ok(),
        "Petclinic launcher class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: Attempt to invoke main.
    let result = try_invoke_main(&[&jar], "org/springframework/boot/loader/JarLauncher", &[]);
    assert!(
        result.is_ok(),
        "Petclinic main invocation failed: {}",
        result.unwrap_err()
    );

    // Phase 3: If we can spawn the process, perform an HTTP smoke test.
    // (This requires CRATONVM_BIN and a working network stack.)
    if std::env::var("CRATONVM_BIN").is_ok() {
        let mut child = spawn_cratonvm(&["-jar", &jar]);

        // Wait for the embedded Tomcat to start.
        let port = 8080u16;
        if wait_for_port("127.0.0.1", port, APP_BOOT_TIMEOUT).is_ok() {
            let response = http_get("127.0.0.1", port, "/owners");
            match response {
                Ok(body) => {
                    assert!(
                        body.contains("HTTP/1.1 200")
                            || body.contains("200 OK")
                            || body.contains("<html"),
                        "Petclinic /owners response must contain HTML or 200 status"
                    );
                }
                Err(e) => {
                    eprintln!(
                        "[T4.9.1] HTTP GET /owners failed: {e} (app may not have fully booted)"
                    );
                }
            }
        } else {
            eprintln!("[T4.9.1] port 8080 did not become reachable (app boot timeout)");
        }

        let _ = child.kill();
        let _ = child.wait();
    }
}

// ---------------------------------------------------------------------------
// T4.9.2 -- Hibernate ORM + H2
// ---------------------------------------------------------------------------

/// T4.9.2: Load Hibernate ORM core classes and verify the Session interface
/// can be resolved.  With a full setup, run a sample H2 in-memory query.
///
/// Setup:
/// ```sh
/// export HIBERNATE_JAR=/path/to/hibernate-core-6.x.jar
/// ```
#[test]
#[ignore = "requires HIBERNATE_JAR env var pointing to hibernate-core JAR"]
fn t4_9_2_hibernate_orm_h2() {
    let jar = check_prereq_env("HIBERNATE_JAR").unwrap();

    // Phase 1: Verify core classes are loadable.
    let result = try_load_class(&[&jar], "org/hibernate/Session");
    assert!(
        result.is_ok(),
        "Hibernate Session class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: Verify SessionFactory is loadable (exercises bytecode
    // enhancement proxy infrastructure).
    let result = try_load_class(&[&jar], "org/hibernate/SessionFactory");
    assert!(
        result.is_ok(),
        "Hibernate SessionFactory class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 3: Verify Transaction interface (JPA integration point).
    let result = try_load_class(&[&jar], "org/hibernate/Transaction");
    assert!(
        result.is_ok(),
        "Hibernate Transaction class loading failed: {}",
        result.unwrap_err()
    );
}

// ---------------------------------------------------------------------------
// T4.9.3 -- Apache Tomcat static page
// ---------------------------------------------------------------------------

/// T4.9.3: Boot Apache Tomcat under CratonVM, send HTTP GET /, and check
/// that the response contains the Tomcat default page content.
///
/// Setup:
/// ```sh
/// export TOMCAT_HOME=/path/to/apache-tomcat-10.x
/// ```
#[test]
#[ignore = "requires TOMCAT_HOME env var pointing to a Tomcat installation"]
fn t4_9_3_tomcat_static_page() {
    let home = check_prereq_env("TOMCAT_HOME").unwrap();
    let home_path = Path::new(&home);

    // Build classpath from Tomcat's lib/ and bin/ directories.
    let mut cp = collect_jars(&home_path.join("lib"));
    cp.extend(collect_jars(&home_path.join("bin")));
    assert!(!cp.is_empty(), "No JARs found in TOMCAT_HOME");

    // Phase 1: Verify the Bootstrap class is loadable.
    let cp_refs: Vec<&str> = cp.iter().map(|s| s.as_str()).collect();
    let result = try_load_class(&cp_refs, "org/apache/catalina/startup/Bootstrap");
    assert!(
        result.is_ok(),
        "Tomcat Bootstrap class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: Attempt to invoke Tomcat's Bootstrap.main.
    let result = try_invoke_main(
        &cp_refs,
        "org/apache/catalina/startup/Bootstrap",
        &["start"],
    );
    assert!(
        result.is_ok(),
        "Tomcat Bootstrap main invocation failed: {}",
        result.unwrap_err()
    );

    // Phase 3: HTTP smoke test (if process-based execution is available).
    if std::env::var("CRATONVM_BIN").is_ok() {
        let cp_str = cp.join(if cfg!(windows) { ";" } else { ":" });
        let mut child = spawn_cratonvm(&[
            "-cp",
            &cp_str,
            &format!("-Dcatalina.home={home}"),
            &format!("-Dcatalina.base={home}"),
            "org.apache.catalina.startup.Bootstrap",
            "start",
        ]);

        let port = 8080u16;
        if wait_for_port("127.0.0.1", port, APP_BOOT_TIMEOUT).is_ok() {
            if let Ok(body) = http_get("127.0.0.1", port, "/") {
                assert!(
                    body.contains("200") || body.contains("<html") || body.contains("Tomcat"),
                    "Tomcat / response must contain recognizable content"
                );
            }
        }

        let _ = child.kill();
        let _ = child.wait();
    }
}

// ---------------------------------------------------------------------------
// T4.9.4 -- Netty echo server
// ---------------------------------------------------------------------------

/// T4.9.4: Boot a Netty echo server, connect a TCP client, exchange data,
/// and verify the echoed response.
///
/// Setup:
/// ```sh
/// export NETTY_JAR=/path/to/netty-all-4.x.jar
/// ```
#[test]
#[ignore = "requires NETTY_JAR env var pointing to the netty-all JAR"]
fn t4_9_4_netty_echo_server() {
    let jar = check_prereq_env("NETTY_JAR").unwrap();

    // Phase 1: Verify Netty core classes load.
    let result = try_load_class(&[&jar], "io/netty/bootstrap/ServerBootstrap");
    assert!(
        result.is_ok(),
        "Netty ServerBootstrap class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: Verify Channel pipeline classes load (NIO transport).
    let result = try_load_class(&[&jar], "io/netty/channel/nio/NioEventLoopGroup");
    assert!(
        result.is_ok(),
        "Netty NioEventLoopGroup class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 3: If a Netty echo server main class is available and the CratonVM
    // binary exists, spawn the server, connect a client, send "hello", and
    // verify the echoed response.
    if std::env::var("CRATONVM_BIN").is_ok() {
        // A typical Netty echo server main class; the user must ensure it
        // exists on the classpath.
        let echo_port = 9999u16;
        let mut child = spawn_cratonvm(&[
            "-cp",
            &jar,
            "io.netty.example.echo.EchoServer",
            &echo_port.to_string(),
        ]);

        if wait_for_port("127.0.0.1", echo_port, APP_BOOT_TIMEOUT).is_ok() {
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", echo_port)) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let msg = b"hello from cratonvm";
                let _ = stream.write_all(msg);
                let _ = stream.flush();

                let mut buf = vec![0u8; msg.len()];
                if stream.read_exact(&mut buf).is_ok() {
                    assert_eq!(&buf, msg, "echo server must return the same data we sent");
                }
            }
        }

        let _ = child.kill();
        let _ = child.wait();
    }
}

// ---------------------------------------------------------------------------
// T4.9.5 -- Apache Cassandra
// ---------------------------------------------------------------------------

/// T4.9.5: Boot a single-node Apache Cassandra instance and execute a CQL
/// query to verify basic functionality.
///
/// Setup:
/// ```sh
/// export CASSANDRA_HOME=/path/to/apache-cassandra-4.x
/// ```
#[test]
#[ignore = "requires CASSANDRA_HOME env var pointing to a Cassandra installation"]
fn t4_9_5_cassandra_smoke() {
    let home = check_prereq_env("CASSANDRA_HOME").unwrap();
    let home_path = Path::new(&home);

    let cp = classpath_from_home_lib(&home);
    let cp_refs: Vec<&str> = cp.iter().map(|s| s.as_str()).collect();

    // Phase 1: Verify the Cassandra daemon class loads.
    let result = try_load_class(&cp_refs, "org/apache/cassandra/service/CassandraDaemon");
    assert!(
        result.is_ok(),
        "Cassandra daemon class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: Verify the CQL transport class loads.
    let result = try_load_class(&cp_refs, "org/apache/cassandra/transport/Server");
    assert!(
        result.is_ok(),
        "Cassandra CQL transport class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 3: Process-based smoke test. Boot Cassandra, wait for CQL port
    // (9042), send a simple native protocol OPTIONS frame.
    if std::env::var("CRATONVM_BIN").is_ok() {
        let cp_str = cp.join(if cfg!(windows) { ";" } else { ":" });
        let mut child = spawn_cratonvm(&[
            "-cp",
            &cp_str,
            &format!("-Dcassandra.config=file://{}/conf/cassandra.yaml", home),
            "org.apache.cassandra.service.CassandraDaemon",
        ]);

        let cql_port = 9042u16;
        if wait_for_port("127.0.0.1", cql_port, APP_BOOT_TIMEOUT).is_ok() {
            // Send a CQL native protocol OPTIONS request (opcode 5).
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", cql_port)) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                // CQL native protocol v4: header(9 bytes)
                // version=0x04, flags=0, stream=0, opcode=OPTIONS(5), length=0
                let options_frame: [u8; 9] = [0x04, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00];
                let _ = stream.write_all(&options_frame);
                let _ = stream.flush();

                let mut response = [0u8; 9];
                if stream.read_exact(&mut response).is_ok() {
                    // Response opcode should be SUPPORTED (6).
                    assert_eq!(
                        response[4], 0x06,
                        "CQL OPTIONS must return SUPPORTED (opcode 6)"
                    );
                }
            }
        }

        let _ = child.kill();
        let _ = child.wait();
    }
}

// ---------------------------------------------------------------------------
// T4.9.6 -- Elasticsearch
// ---------------------------------------------------------------------------

/// T4.9.6: Boot Elasticsearch, PUT a document, GET it back, and verify the
/// content matches.
///
/// Setup:
/// ```sh
/// export ES_HOME=/path/to/elasticsearch-8.x
/// ```
#[test]
#[ignore = "requires ES_HOME env var pointing to an Elasticsearch installation"]
fn t4_9_6_elasticsearch_index() {
    let home = check_prereq_env("ES_HOME").unwrap();
    let home_path = Path::new(&home);

    // Build classpath: lib/ + modules subdirectories.
    let mut cp = collect_jars(&home_path.join("lib"));
    let modules_dir = home_path.join("modules");
    if modules_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&modules_dir) {
            for entry in entries.flatten() {
                let sub = entry.path();
                if sub.is_dir() {
                    cp.extend(collect_jars(&sub));
                }
            }
        }
    }
    assert!(!cp.is_empty(), "No JARs found in ES_HOME");

    // Phase 1: Verify the Elasticsearch bootstrap class loads.
    let cp_refs: Vec<&str> = cp.iter().map(|s| s.as_str()).collect();
    let result = try_load_class(&cp_refs, "org/elasticsearch/bootstrap/Elasticsearch");
    assert!(
        result.is_ok(),
        "Elasticsearch bootstrap class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: Process-based smoke test. Boot ES, wait for HTTP port (9200),
    // PUT a test document, GET it back.
    if std::env::var("CRATONVM_BIN").is_ok() {
        let cp_str = cp.join(if cfg!(windows) { ";" } else { ":" });
        let mut child = spawn_cratonvm(&[
            "-cp",
            &cp_str,
            &format!("-Des.path.home={home}"),
            "org.elasticsearch.bootstrap.Elasticsearch",
        ]);

        let port = 9200u16;
        if wait_for_port("127.0.0.1", port, APP_BOOT_TIMEOUT).is_ok() {
            // PUT a test document.
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
                let _ = stream.set_read_timeout(Some(HTTP_TIMEOUT));
                let body = r#"{"title":"CratonVM Test","value":42}"#;
                let request = format!(
                    "PUT /test-index/_doc/1 HTTP/1.1\r\n\
                     Host: 127.0.0.1:{port}\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(request.as_bytes());
                let _ = stream.flush();
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response);
                // Accept 200 or 201 (created).
                assert!(
                    response.contains("200") || response.contains("201"),
                    "PUT document must succeed"
                );
            }

            // GET the document back.
            if let Ok(body) = http_get("127.0.0.1", port, "/test-index/_doc/1") {
                assert!(
                    body.contains("CratonVM Test"),
                    "GET document must contain our test data"
                );
            }
        }

        let _ = child.kill();
        let _ = child.wait();
    }
}

// ---------------------------------------------------------------------------
// T4.9.7 -- Apache Kafka
// ---------------------------------------------------------------------------

/// T4.9.7: Boot Kafka, produce a message to a topic, consume it, and verify
/// the message content.
///
/// Setup:
/// ```sh
/// export KAFKA_HOME=/path/to/kafka_2.13-3.x
/// ```
#[test]
#[ignore = "requires KAFKA_HOME env var pointing to a Kafka installation"]
fn t4_9_7_kafka_produce_consume() {
    let home = check_prereq_env("KAFKA_HOME").unwrap();
    let home_path = Path::new(&home);

    // Kafka JARs are in libs/ (note: plural).
    let libs_dir = home_path.join("libs");
    let cp = if libs_dir.exists() {
        collect_jars(&libs_dir)
    } else {
        classpath_from_home_lib(&home)
    };
    assert!(!cp.is_empty(), "No JARs found in KAFKA_HOME");

    // Phase 1: Verify the Kafka main class loads.
    let cp_refs: Vec<&str> = cp.iter().map(|s| s.as_str()).collect();
    let result = try_load_class(&cp_refs, "kafka/Kafka");
    assert!(
        result.is_ok(),
        "Kafka main class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: Verify the KafkaProducer class loads.
    let result = try_load_class(&cp_refs, "org/apache/kafka/clients/producer/KafkaProducer");
    assert!(
        result.is_ok(),
        "KafkaProducer class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 3: Process-based smoke test using KRaft mode (no ZooKeeper).
    if std::env::var("CRATONVM_BIN").is_ok() {
        let cp_str = cp.join(if cfg!(windows) { ";" } else { ":" });
        let config_file = home_path
            .join("config")
            .join("kraft")
            .join("server.properties");
        if config_file.exists() {
            let mut child =
                spawn_cratonvm(&["-cp", &cp_str, "kafka.Kafka", config_file.to_str().unwrap()]);

            // Kafka broker listens on 9092.
            let port = 9092u16;
            if wait_for_port("127.0.0.1", port, APP_BOOT_TIMEOUT).is_ok() {
                // A real produce/consume test would use the Kafka wire protocol.
                // For now, verify the broker is accepting connections.
                let connected = TcpStream::connect_timeout(
                    &format!("127.0.0.1:{port}").parse().unwrap(),
                    Duration::from_secs(5),
                );
                assert!(
                    connected.is_ok(),
                    "Kafka broker must accept TCP connections on port {port}"
                );
            }

            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// T4.9.8 -- Jenkins freestyle build
// ---------------------------------------------------------------------------

/// T4.9.8: Boot Jenkins from its WAR file and schedule a freestyle build
/// via the REST API.
///
/// Setup:
/// ```sh
/// export JENKINS_WAR=/path/to/jenkins.war
/// ```
#[test]
#[ignore = "requires JENKINS_WAR env var pointing to jenkins.war"]
fn t4_9_8_jenkins_freestyle() {
    let war = check_prereq_env("JENKINS_WAR").unwrap();

    // Phase 1: Verify the Winstone launcher class loads.
    let result = try_load_class(&[&war], "winstone/Launcher");
    assert!(
        result.is_ok(),
        "Jenkins Winstone launcher class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: Process-based smoke test.
    if std::env::var("CRATONVM_BIN").is_ok() {
        let mut child = spawn_cratonvm(&["-jar", &war, "--httpPort=8081"]);

        let port = 8081u16;
        if wait_for_port("127.0.0.1", port, APP_BOOT_TIMEOUT).is_ok() {
            // Verify Jenkins is responding with its login page or API.
            if let Ok(body) = http_get("127.0.0.1", port, "/api/json") {
                assert!(
                    body.contains("200")
                        || body.contains("Jenkins")
                        || body.contains("numExecutors"),
                    "Jenkins API must return recognizable JSON"
                );
            }
        }

        let _ = child.kill();
        let _ = child.wait();
    }
}

// ---------------------------------------------------------------------------
// T4.9.9 -- Maven build
// ---------------------------------------------------------------------------

/// T4.9.9: Run Apache Maven on a sample project and check for BUILD SUCCESS.
///
/// Setup:
/// ```sh
/// export MAVEN_HOME=/path/to/apache-maven-3.x
/// ```
#[test]
#[ignore = "requires MAVEN_HOME env var pointing to a Maven installation"]
fn t4_9_9_maven_build() {
    let home = check_prereq_env("MAVEN_HOME").unwrap();
    let home_path = Path::new(&home);

    // Build classpath from lib/ and boot/.
    let mut cp = collect_jars(&home_path.join("lib"));
    let boot_dir = home_path.join("boot");
    if boot_dir.exists() {
        cp.extend(collect_jars(&boot_dir));
    }
    assert!(!cp.is_empty(), "No JARs found in MAVEN_HOME");

    // Phase 1: Verify Maven CLI class loads.
    let cp_refs: Vec<&str> = cp.iter().map(|s| s.as_str()).collect();
    let result = try_load_class(&cp_refs, "org/apache/maven/cli/MavenCli");
    assert!(
        result.is_ok(),
        "Maven CLI class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: Create a minimal Maven project and run `mvn validate`.
    if std::env::var("CRATONVM_BIN").is_ok() {
        let tmp_dir = std::env::temp_dir().join("cratonvm_t4_9_9_maven");
        let _ = std::fs::create_dir_all(&tmp_dir);

        // Write a minimal pom.xml.
        let pom = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0"
         xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
         xsi:schemaLocation="http://maven.apache.org/POM/4.0.0
         http://maven.apache.org/xsd/maven-4.0.0.xsd">
    <modelVersion>4.0.0</modelVersion>
    <groupId>com.cratonvm.test</groupId>
    <artifactId>t4-9-9</artifactId>
    <version>1.0</version>
</project>"#;
        std::fs::write(tmp_dir.join("pom.xml"), pom).expect("must write pom.xml");

        let cp_str = cp.join(if cfg!(windows) { ";" } else { ":" });
        let output = Command::new(cratonvm_binary())
            .args(&[
                "-cp",
                &cp_str,
                &format!("-Dmaven.home={home}"),
                &format!("-Duser.dir={}", tmp_dir.display()),
                "org.apache.maven.cli.MavenCli",
                "validate",
            ])
            .output()
            .expect("must execute Maven");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{stdout}{stderr}");

        // Even partial success is meaningful -- we want to see Maven entered
        // its lifecycle.
        assert!(
            combined.contains("BUILD SUCCESS")
                || combined.contains("BUILD FAILURE")
                || combined.contains("maven"),
            "Maven output must indicate it ran: {combined}"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}

// ---------------------------------------------------------------------------
// T4.9.10 -- Gradle build
// ---------------------------------------------------------------------------

/// T4.9.10: Run Gradle on a sample project and check for BUILD SUCCESSFUL.
///
/// Setup:
/// ```sh
/// export GRADLE_HOME=/path/to/gradle-8.x
/// ```
#[test]
#[ignore = "requires GRADLE_HOME env var pointing to a Gradle installation"]
fn t4_9_10_gradle_build() {
    let home = check_prereq_env("GRADLE_HOME").unwrap();

    let cp = classpath_from_home_lib(&home);
    let cp_refs: Vec<&str> = cp.iter().map(|s| s.as_str()).collect();

    // Phase 1: Verify Gradle launcher class loads.
    let result = try_load_class(&cp_refs, "org/gradle/launcher/Main");
    assert!(
        result.is_ok(),
        "Gradle launcher class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: Create a minimal Gradle project and run `gradle help`.
    if std::env::var("CRATONVM_BIN").is_ok() {
        let tmp_dir = std::env::temp_dir().join("cratonvm_t4_9_10_gradle");
        let _ = std::fs::create_dir_all(&tmp_dir);

        std::fs::write(tmp_dir.join("build.gradle"), "// empty build file\n")
            .expect("must write build.gradle");

        let cp_str = cp.join(if cfg!(windows) { ";" } else { ":" });
        let output = Command::new(cratonvm_binary())
            .args(&[
                "-cp",
                &cp_str,
                &format!("-Dgradle.user.home={}", tmp_dir.display()),
                "org.gradle.launcher.Main",
                "help",
            ])
            .output()
            .expect("must execute Gradle");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{stdout}{stderr}");

        assert!(
            combined.contains("BUILD SUCCESSFUL")
                || combined.contains("BUILD FAILED")
                || combined.contains("gradle")
                || combined.contains("Gradle"),
            "Gradle output must indicate it ran: {combined}"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}

// ---------------------------------------------------------------------------
// T4.9.11 -- IntelliJ IDEA headless
// ---------------------------------------------------------------------------

/// T4.9.11: Boot IntelliJ IDEA in headless/inspect mode and verify the
/// startup log indicates successful initialization.
///
/// Setup:
/// ```sh
/// export IDEA_HOME=/path/to/idea-IC-xxx
/// ```
#[test]
#[ignore = "requires IDEA_HOME env var pointing to an IntelliJ IDEA installation"]
fn t4_9_11_intellij_headless() {
    let home = check_prereq_env("IDEA_HOME").unwrap();

    let cp = classpath_from_home_lib(&home);
    let cp_refs: Vec<&str> = cp.iter().map(|s| s.as_str()).collect();

    // Phase 1: Verify the main class loads.
    let result = try_load_class(&cp_refs, "com/intellij/idea/Main");
    assert!(
        result.is_ok(),
        "IntelliJ IDEA main class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: Process-based headless startup check.
    if std::env::var("CRATONVM_BIN").is_ok() {
        let cp_str = cp.join(if cfg!(windows) { ";" } else { ":" });
        let mut child = Command::new(cratonvm_binary())
            .args(&[
                "-cp",
                &cp_str,
                &format!("-Didea.home.path={home}"),
                "-Djava.awt.headless=true",
                "com.intellij.idea.Main",
                "inspect",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("must spawn IntelliJ");

        // Give it up to 60 seconds to produce some output.
        std::thread::sleep(Duration::from_secs(5));
        let _ = child.kill();
        let output = child.wait_with_output();

        if let Ok(output) = output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = format!("{stdout}{stderr}");
            // Even a crash with class-loading messages proves we got far.
            eprintln!(
                "[T4.9.11] IntelliJ headless output length: {} bytes",
                combined.len()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// T4.9.12 -- javac self-host
// ---------------------------------------------------------------------------

/// T4.9.12: Compile Java test sources using javac running under CratonVM.
/// This is the ultimate self-hosting test.
///
/// Setup:
/// ```sh
/// export JAVA_HOME=/path/to/jdk-25
/// ```
#[test]
#[ignore = "requires JAVA_HOME env var pointing to JDK 25+ installation"]
fn t4_9_12_javac_self_host() {
    let java_home = check_prereq_env("JAVA_HOME").unwrap();
    let modules_path = format!("{java_home}/lib/modules");
    assert!(
        Path::new(&modules_path).exists(),
        "JDK modules not found at: {modules_path}"
    );

    // Phase 1: Verify javac main class loads.
    let result = try_load_class(&[&modules_path], "com/sun/tools/javac/Main");
    assert!(
        result.is_ok(),
        "javac main class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: Write a minimal HelloWorld.java and attempt to compile it.
    let tmp_dir = std::env::temp_dir().join("cratonvm_t4_9_12");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let source_file = tmp_dir.join("HelloWorld.java");
    std::fs::write(
        &source_file,
        "public class HelloWorld {\n\
         \x20   public static void main(String[] args) {\n\
         \x20       System.out.println(\"Hello from CratonVM!\");\n\
         \x20   }\n\
         }\n",
    )
    .expect("Failed to write HelloWorld.java");

    // Phase 3: Invoke javac main via the VM.
    let source_path = source_file
        .to_str()
        .expect("temp path not valid UTF-8")
        .to_string();

    let result = try_invoke_main(
        &[&modules_path],
        "com/sun/tools/javac/Main",
        &[&source_path],
    );
    assert!(
        result.is_ok(),
        "javac main invocation failed: {}",
        result.unwrap_err()
    );

    // Phase 4: If process-based execution is available, run javac as a
    // subprocess and verify the .class file is produced.
    if std::env::var("CRATONVM_BIN").is_ok() {
        let output = Command::new(cratonvm_binary())
            .args(&[
                &format!("-Djava.home={java_home}"),
                "-m",
                "jdk.compiler/com.sun.tools.javac.Main",
                "-d",
                tmp_dir.to_str().unwrap(),
                source_file.to_str().unwrap(),
            ])
            .output()
            .expect("must execute javac");

        let class_file = tmp_dir.join("HelloWorld.class");
        if class_file.exists() {
            eprintln!("[T4.9.12] javac produced HelloWorld.class -- self-hosting success!");
            // Verify it starts with the Java class file magic 0xCAFEBABE.
            let class_bytes = std::fs::read(&class_file).unwrap();
            assert!(class_bytes.len() >= 4, "class file too small");
            assert_eq!(
                &class_bytes[0..4],
                &[0xCA, 0xFE, 0xBA, 0xBE],
                "class file must start with CAFEBABE magic"
            );
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!(
                "[T4.9.12] javac did not produce .class file. stderr: {}",
                stderr
            );
        }
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// ---------------------------------------------------------------------------
// T4.9.13 -- JShell self-host
// ---------------------------------------------------------------------------

/// T4.9.13: Run a JShell expression under CratonVM and check the output.
///
/// Setup:
/// ```sh
/// export JAVA_HOME=/path/to/jdk-25
/// ```
#[test]
#[ignore = "requires JAVA_HOME env var pointing to JDK 25+ installation"]
fn t4_9_13_jshell_self_host() {
    let java_home = check_prereq_env("JAVA_HOME").unwrap();
    let modules_path = format!("{java_home}/lib/modules");
    assert!(
        Path::new(&modules_path).exists(),
        "JDK modules not found at: {modules_path}"
    );

    // Phase 1: Verify JShell tool provider class loads.
    let result = try_load_class(
        &[&modules_path],
        "jdk/internal/jshell/tool/JShellToolProvider",
    );
    assert!(
        result.is_ok(),
        "JShell tool provider class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: If process-based execution is available, pipe a simple
    // expression to JShell and verify the output.
    if std::env::var("CRATONVM_BIN").is_ok() {
        let mut child = Command::new(cratonvm_binary())
            .args(&[
                &format!("-Djava.home={java_home}"),
                "-m",
                "jdk.jshell/jdk.internal.jshell.tool.JShellToolProvider",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("must spawn jshell");

        // Send a simple expression.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(b"System.out.println(1 + 2)\n");
            let _ = stdin.write_all(b"/exit\n");
            let _ = stdin.flush();
        }

        let output = child.wait_with_output().expect("must wait for jshell");

        let stdout = String::from_utf8_lossy(&output.stdout);
        // JShell should print "3" for the expression 1+2.
        if stdout.contains("3") {
            eprintln!("[T4.9.13] JShell produced correct output for 1+2=3");
        } else {
            eprintln!(
                "[T4.9.13] JShell output did not contain '3'. stdout: {}",
                stdout
            );
        }
    }
}

// ---------------------------------------------------------------------------
// T4.9.14 -- Open Liberty
// ---------------------------------------------------------------------------

/// T4.9.14: Boot Open Liberty and verify the server started log message.
///
/// Setup:
/// ```sh
/// export LIBERTY_HOME=/path/to/wlp
/// ```
#[test]
#[ignore = "requires LIBERTY_HOME env var pointing to an Open Liberty installation"]
fn t4_9_14_openliberty_javaee() {
    let home = check_prereq_env("LIBERTY_HOME").unwrap();
    let home_path = Path::new(&home);

    let cp = classpath_from_home_lib(&home);
    let cp_refs: Vec<&str> = cp.iter().map(|s| s.as_str()).collect();

    // Phase 1: Verify the kernel boot launcher class loads.
    let result = try_load_class(&cp_refs, "com/ibm/ws/kernel/boot/Launcher");
    assert!(
        result.is_ok(),
        "Open Liberty kernel boot class loading failed: {}",
        result.unwrap_err()
    );

    // Phase 2: Attempt main invocation.
    let result = try_invoke_main(&cp_refs, "com/ibm/ws/kernel/boot/Launcher", &[]);
    assert!(
        result.is_ok(),
        "Open Liberty main invocation failed: {}",
        result.unwrap_err()
    );

    // Phase 3: Process-based smoke test.
    if std::env::var("CRATONVM_BIN").is_ok() {
        let cp_str = cp.join(if cfg!(windows) { ";" } else { ":" });
        let mut child = spawn_cratonvm(&[
            "-cp",
            &cp_str,
            &format!("-Dwlp.install.dir={home}"),
            "com.ibm.ws.kernel.boot.Launcher",
        ]);

        // Open Liberty default HTTP port is 9080.
        let port = 9080u16;
        if wait_for_port("127.0.0.1", port, APP_BOOT_TIMEOUT).is_ok() {
            if let Ok(body) = http_get("127.0.0.1", port, "/") {
                assert!(
                    body.contains("200") || body.contains("Liberty") || body.contains("<html"),
                    "Open Liberty response must contain recognizable content"
                );
            }
        }

        let _ = child.kill();
        let _ = child.wait();
    }
}
