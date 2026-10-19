// Fork-local tests of the Hyperdrive binding inside a running Worker. Each
// test starts `celld dev` on a copy of `tests/fixtures/hyperdrive/worker`.
//
// Prerequisites, once:
//
//     npm install --prefix crates/celld/tests/fixtures/hyperdrive/worker
//
// That also installs the pinned esbuild; CELLD_ESBUILD overrides it. Without
// the dependencies the tests skip; CELLD_TEST_STRICT=1 makes a skip fail.
//
// The PostgreSQL test runs when CELLD_TEST_POSTGRES_URL names a database whose
// server has TLS on with a certificate celld does not trust, for example:
//
//     docker run -d --rm --name celld-test-pg -p 127.0.0.1:55432:5432 \
//       -p '[::1]:55432:5432' -e POSTGRES_USER=celld \
//       -e POSTGRES_PASSWORD=hunter2 -e POSTGRES_DB=celld postgres:17 \
//       -c ssl=on -c ssl_cert_file=/etc/ssl/certs/ssl-cert-snakeoil.pem \
//       -c ssl_key_file=/etc/ssl/private/ssl-cert-snakeoil.key
//     CELLD_TEST_POSTGRES_URL=postgres://celld:hunter2@127.0.0.1:55432/celld \
//       cargo +1.94.1 test -p celld --test hyperdrive_runtime

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BINDINGS: [&str; 4] = ["HYPERDRIVE", "HD_TLS", "HD_CAFE", "HD_V6"];
const LOCAL_PREFIX: &str = "CLOUDFLARE_HYPERDRIVE_LOCAL_CONNECTION_STRING_";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hyperdrive/worker")
}

// Print why the test does not run, or fail when a skip is not acceptable.
fn skip(why: &str) {
    if std::env::var_os("CELLD_TEST_STRICT").is_some_and(|value| value == "1") {
        panic!("CELLD_TEST_STRICT=1 and {why}");
    }
    eprintln!("skipped: {why}");
}

fn esbuild() -> Option<PathBuf> {
    let candidate = std::env::var_os("CELLD_ESBUILD")
        .map(PathBuf::from)
        .unwrap_or_else(|| fixture().join("node_modules/.bin/esbuild"));
    Command::new(&candidate)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
        .then_some(candidate)
}

fn prerequisites() -> Option<PathBuf> {
    if !fixture().join("node_modules/pg/package.json").exists() {
        skip(&format!(
            "the test Worker's dependencies are not installed; run npm install --prefix {}",
            fixture().display()
        ));
        return None;
    }
    let esbuild = esbuild();
    if esbuild.is_none() {
        skip("no esbuild: set CELLD_ESBUILD or run npm install in the test Worker");
    }
    esbuild
}

// A `celld dev` process on a private copy of the test Worker. Dropping it stops
// the process; the node child dies with it through its parent-death signal.
struct Server {
    child: Child,
    port: u16,
    output: Arc<Mutex<String>>,
    _project: tempfile::TempDir,
}

impl Server {
    fn start(esbuild: &Path, connection_strings: &[(&str, String)]) -> Self {
        let mut last_output = String::new();
        // A free port can be taken between the probe and the bind; retry.
        for _ in 0..3 {
            let project = tempfile::tempdir().unwrap();
            for file in ["index.js", "package.json", "wrangler.jsonc"] {
                std::fs::copy(fixture().join(file), project.path().join(file)).unwrap();
            }
            std::os::unix::fs::symlink(
                fixture().join("node_modules"),
                project.path().join("node_modules"),
            )
            .unwrap();
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();

            let mut command = Command::new(env!("CARGO_BIN_EXE_celld"));
            command
                .arg("dev")
                .arg(project.path())
                .args(["--port", &port.to_string(), "--no-watch"])
                .env("CELLD_ESBUILD", esbuild)
                .env("CELLD_MAX_STATELESS_ISOLATES", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for (name, _) in std::env::vars() {
                if name.starts_with(LOCAL_PREFIX) || name.starts_with("CELLD_HYPERDRIVE_") {
                    command.env_remove(name);
                }
            }
            for (binding, value) in connection_strings {
                command.env(format!("{LOCAL_PREFIX}{binding}"), value);
            }
            let mut child = command.spawn().unwrap();
            let output = Arc::new(Mutex::new(String::new()));
            for stream in [
                Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
                Box::new(child.stderr.take().unwrap()),
            ] {
                let output = output.clone();
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut buffer = [0; 4096];
                    while let Ok(read) = stream.read(&mut buffer) {
                        if read == 0 {
                            break;
                        }
                        output
                            .lock()
                            .unwrap()
                            .push_str(&String::from_utf8_lossy(&buffer[..read]));
                    }
                });
            }
            let mut server = Server {
                child,
                port,
                output,
                _project: project,
            };
            if server.wait_ready() {
                return server;
            }
            last_output = server.output();
            if !last_output.contains("in use") {
                break;
            }
        }
        panic!("celld dev did not start:\n{last_output}");
    }

    // Ready once the fixture's own route answers; a build takes a while.
    fn wait_ready(&mut self) -> bool {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(90) {
            if self.child.try_wait().unwrap().is_some() {
                return false;
            }
            if let Ok((200, body)) = self.request("/token") {
                if body.contains("\"token\"") {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }

    fn output(&self) -> String {
        self.output.lock().unwrap().clone()
    }

    // HTTP/1.0, so the body is never chunked.
    fn request(&self, path: &str) -> std::io::Result<(u16, String)> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        write!(
            stream,
            "GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{}\r\n\r\n",
            self.port
        )?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        Ok((status, body.to_string()))
    }

    fn get(&self, path: &str) -> Value {
        let (status, body) = self
            .request(path)
            .unwrap_or_else(|error| panic!("{path}: {error}\n{}", self.output()));
        assert_eq!(status, 200, "{path}: {body}\n{}", self.output());
        serde_json::from_str(&body).unwrap_or_else(|_| panic!("{path}: {body}"))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // SIGTERM lets the supervisor stop its node; SIGKILL is the fallback.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(40) {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn corpus() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hyperdrive/corpus.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

// The fields `HyperdriveConfig` reports for `url`, from the shared corpus.
fn expected_fields(url: &str) -> Value {
    let config = celld::hyperdrive::HyperdriveConfig::parse("DB", url).unwrap();
    let mut fields: Value = serde_json::from_str(&config.descriptor_json()).unwrap();
    fields.as_object_mut().unwrap().remove("tlsRequired");
    fields
}

#[test]
fn binding_in_a_worker() {
    let Some(esbuild) = prerequisites() else {
        return;
    };
    let server = Server::start(&esbuild, &[]);

    // The members of the Cloudflare binding, as getters on the prototype.
    let shape = server.get("/shape");
    assert_eq!(shape["ownKeys"], serde_json::json!([]), "{shape}");
    assert_eq!(shape["frozen"], true);
    assert_eq!(shape["ip"], Value::Null);
    let members = &shape["members"];
    for name in ["connectionString", "host", "user", "password", "database"] {
        assert_eq!(members[name], "string", "{name}: {shape}");
    }
    assert_eq!(members["port"], "number");
    assert_eq!(members["connect"], "function");
    assert_eq!(shape["hostAfterAssignment"], "127.0.0.1", "{shape}");
    for binding in BINDINGS {
        let fields = &shape["fields"][binding];
        let expected = expected_fields(fields["connectionString"].as_str().unwrap());
        for key in [
            "connectionString",
            "host",
            "port",
            "user",
            "password",
            "database",
        ] {
            assert_eq!(fields[key], expected[key], "{binding}.{key}");
        }
    }
    assert_eq!(shape["fields"]["HD_V6"]["host"], "[::1]");
    assert_eq!(shape["fields"]["HD_CAFE"]["database"], "café");

    // No copy of the binding carries its values.
    let copies = server.get("/copies");
    assert_eq!(copies["leaks"], false, "{copies}");
    for copy in ["spread", "json", "assign"] {
        assert_eq!(copies["copies"][copy], "{}", "{copies}");
    }
    assert_eq!(copies["copies"]["entries"], "[]", "{copies}");

    // A Worker Loader refuses the binding, but passes its string.
    let loader = server.get("/loader");
    for outcome in ["direct", "nested", "array"] {
        assert_eq!(
            loader["outcomes"][outcome]["error"], "DataCloneError",
            "{outcome}: {loader}"
        );
    }
    assert_eq!(loader["wrappedLeaks"], false, "{loader}");
    assert_eq!(loader["stringArrived"], true, "{loader}");

    // The string does not help a loaded Worker whose egress is denied.
    let egress = server.get("/egress");
    assert_eq!(egress["allowed"], "opened", "{egress}");
    assert!(
        egress["denied"].as_str().unwrap().starts_with("refused"),
        "{egress}"
    );

    // An RPC argument either fails or arrives without the values.
    let rpc = server.get("/rpc");
    assert_eq!(rpc["leaks"], false, "{rpc}");

    let connect = server.get("/connect");
    assert_eq!(connect["required"]["error"], "TypeError", "{connect}");
    assert_eq!(connect["plain"]["startTls"], "function", "{connect}");

    // The bundled, pinned driver reads every accepted string as the binding
    // does. Only the documented differences are normalized: the driver strips
    // IPv6 brackets, gives the port as a string, and omits a missing password.
    for case in corpus()["accept"].as_array().unwrap() {
        let url = case["url"].as_str().unwrap();
        let driver = server.get(&format!("/agree?url={}", encode(url)));
        let host = case["host"].as_str().unwrap();
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        assert_eq!(driver["host"], host, "{url}: {driver}");
        let port = match &driver["port"] {
            Value::Null => 5432,
            Value::String(port) if port.is_empty() => 5432,
            Value::String(port) => port.parse().unwrap(),
            other => other.as_u64().unwrap(),
        };
        assert_eq!(port, case["port"].as_u64().unwrap(), "{url}: {driver}");
        assert_eq!(driver["user"], case["user"], "{url}: {driver}");
        let password = driver["password"].as_str().unwrap_or("");
        assert_eq!(password, case["password"], "{url}: {driver}");
        assert_eq!(driver["database"], case["database"], "{url}: {driver}");
    }

    // With one isolate allowed, module state persists across requests.
    let first = server.get("/token");
    let second = server.get("/token");
    assert_eq!(first["token"], second["token"]);
    assert!(second["served"].as_u64() > first["served"].as_u64());
}

// The connection strings for the fixture's bindings, derived from one URL.
fn database_strings(base: &str) -> Vec<(&'static str, String)> {
    let url = url::Url::parse(base).expect("CELLD_TEST_POSTGRES_URL is not a URL");
    assert!(
        url.query().is_none(),
        "CELLD_TEST_POSTGRES_URL must not have a query"
    );
    let mut strings = vec![
        ("HYPERDRIVE", base.to_string()),
        ("HD_TLS", format!("{base}?sslmode=require")),
    ];
    let mut cafe = url.clone();
    cafe.set_path("/caf%C3%A9");
    strings.push(("HD_CAFE", cafe.to_string()));
    let mut v6 = url.clone();
    v6.set_host(Some("[::1]")).unwrap();
    strings.push(("HD_V6", v6.to_string()));
    strings
}

#[test]
fn binding_against_postgres() {
    let Some(esbuild) = prerequisites() else {
        return;
    };
    let Ok(base) = std::env::var("CELLD_TEST_POSTGRES_URL") else {
        skip("CELLD_TEST_POSTGRES_URL is not set");
        return;
    };
    let strings = database_strings(&base);
    let user = url::Url::parse(&base).unwrap().username().to_string();
    let database = url::Url::parse(&base).unwrap().path()[1..].to_string();
    let server = Server::start(&esbuild, &strings);

    // Both connection paths, SCRAM, and a second request in the same isolate.
    let token = server.get("/token")["token"].clone();
    for mode in ["url", "fields"] {
        for _ in 0..2 {
            let row = server.get(&format!("/pg?binding=HYPERDRIVE&mode={mode}"));
            assert_eq!(row["user"], user.as_str(), "{mode}: {row}");
            assert_eq!(row["database"], database.as_str(), "{mode}: {row}");
            assert_eq!(row["password_encryption"], "scram-sha-256", "{mode}: {row}");
        }
    }
    assert_eq!(server.get("/token")["token"], token, "the isolate changed");

    // An escaped database name, through both paths.
    server.get("/pg/setup");
    for mode in ["url", "fields"] {
        let row = server.get(&format!("/pg?binding=HD_CAFE&mode={mode}"));
        assert_eq!(row["database"], "café", "{mode}: {row}");
    }

    // A bracketed IPv6 host works through the fields. The connection string
    // does not, as the service doc says: pg-cloudflare drops the brackets.
    let row = server.get("/pg?binding=HD_V6&mode=fields");
    assert_eq!(row["user"], user.as_str(), "{row}");

    // A failed connect does not poison the next one.
    let recover = server.get("/pg/recover");
    assert!(recover["failed"]["error"].is_string(), "{recover}");
    assert_eq!(recover["recovered"]["user"], user.as_str(), "{recover}");

    // connect() is a plaintext socket to the database itself.
    let socket = server.get("/pg/socket");
    assert!(
        matches!(socket["answer"].as_str(), Some("S" | "N")),
        "{socket}"
    );

    // The server's certificate is not publicly trusted, so TLS fails closed.
    let untrusted = server.get("/pg/untrusted");
    let message = untrusted["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("UnknownIssuer") || message.contains("CaUsedAsEndEntity"),
        "{untrusted}"
    );
}
