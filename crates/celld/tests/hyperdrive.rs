// Fork-local tests for Hyperdrive bindings. They need no database, Node, or
// esbuild: the projects set `no_bundle`, and the bucket is a local SQLite one.
// `hyperdrive_runtime.rs` covers the binding inside a running Worker.

use celld::control_plane::AgentDeployment;
use celld::deploy::{self, HyperdriveSource, Options, VarOverrides};
use celld::hyperdrive::HyperdriveConfig;
use celld::protocol::{self, Manifest};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

const SECRET: &str = "hunter2";

fn corpus() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hyperdrive/corpus.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn descriptor(config: &HyperdriveConfig) -> Value {
    serde_json::from_str(&config.descriptor_json()).unwrap()
}

#[test]
fn accepted_strings_yield_the_expected_fields() {
    for case in corpus()["accept"].as_array().unwrap() {
        let url = case["url"].as_str().unwrap();
        let config = HyperdriveConfig::parse("DB", url)
            .unwrap_or_else(|error| panic!("{url} was rejected: {error:#}"));
        let fields = descriptor(&config);
        assert_eq!(fields["connectionString"], url, "{url}");
        for key in [
            "host",
            "port",
            "user",
            "password",
            "database",
            "tlsRequired",
        ] {
            assert_eq!(fields[key], case[key], "{url}: {key}");
        }
        let port = case["port"].as_u64().unwrap();
        assert_eq!(
            config.redacted(),
            format!(
                "postgres://{}:{port}/{}",
                case["host"].as_str().unwrap(),
                case["database"].as_str().unwrap()
            ),
        );
        let password = case["password"].as_str().unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains(url), "{url}: Debug shows the string");
        if password.len() > 1 {
            assert!(!debug.contains(password), "{url}: Debug shows the password");
            assert!(
                !config.redacted().contains(password),
                "{url}: redacted() shows the password"
            );
        }
    }
}

#[test]
fn rejected_strings_name_the_problem_without_the_password() {
    for case in corpus()["reject"].as_array().unwrap() {
        let url = case["url"].as_str().unwrap();
        let expected = case["error"].as_str().unwrap();
        let error = match HyperdriveConfig::parse("DB", url) {
            Ok(config) => panic!("{url} was accepted: {config:?}"),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            error.contains(expected),
            "{url}: {error:?} lacks {expected:?}"
        );
        assert!(
            error.starts_with("Hyperdrive binding \"DB\": "),
            "{url}: {error:?}"
        );
        assert!(
            !error.contains(SECRET),
            "{url}: the error shows the password"
        );
    }
}

// A project directory with a Hyperdrive binding and an unbundled entry, so
// `deploy::build` runs without esbuild.
struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new(config: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.js"),
            "export default { fetch() { return new Response('ok'); } };\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("wrangler.json"), config.to_string()).unwrap();
        Self { dir }
    }

    fn worker(hyperdrive: Value) -> Self {
        Self::new(json!({
            "name": "hd-test",
            "main": "index.js",
            "compatibility_date": "2026-01-01",
            "no_bundle": true,
            "hyperdrive": hyperdrive,
        }))
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn build(
        &self,
        hyperdrive: HyperdriveSource,
        vars: VarOverrides,
    ) -> anyhow::Result<deploy::Built> {
        deploy::build(&Options {
            config: Some(self.path().to_path_buf()),
            bucket: None,
            endpoint: None,
            region: None,
            dry_run: true,
            json: false,
            vars,
            hyperdrive,
            local_images: false,
        })
    }
}

fn strings(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

const LOCAL: &str = "postgres://dev:devpw@localhost:5432/dev";
const PROD: &str = "postgres://app:hunter2@db.internal:5432/app";

fn binding_entry() -> Value {
    json!([{ "binding": "HYPERDRIVE", "id": "example", "localConnectionString": LOCAL }])
}

fn hyperdrive_metadata(manifest: &Manifest) -> Vec<&Value> {
    manifest.raw_metadata["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|binding| binding["type"] == "hyperdrive")
        .collect()
}

fn error_text(result: anyhow::Result<impl Sized>) -> String {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(error) => format!("{error:#}"),
    }
}

#[test]
fn production_requires_its_own_variable() {
    let project = Project::worker(binding_entry());
    let error = error_text(project.build(
        HyperdriveSource::Production(BTreeMap::new()),
        VarOverrides::None,
    ));
    assert!(
        error.contains("CELLD_HYPERDRIVE_CONNECTION_STRING_HYPERDRIVE"),
        "{error}"
    );

    let built = project
        .build(
            HyperdriveSource::Production(strings(&[("HYPERDRIVE", PROD)])),
            VarOverrides::None,
        )
        .unwrap();
    let bindings = hyperdrive_metadata(&built.manifest);
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0]["name"], "HYPERDRIVE");
    assert_eq!(bindings[0]["id"], "example");
    assert_eq!(bindings[0]["connection_string"], PROD);
    assert!(built
        .manifest
        .required_features
        .iter()
        .any(|feature| feature == protocol::FEATURE_HYPERDRIVE_V1));
    protocol::hyperdrive_bindings(&built.manifest).unwrap();
}

#[test]
fn dev_prefers_the_override_then_local_connection_string() {
    let project = Project::worker(binding_entry());
    let built = project
        .build(HyperdriveSource::Dev(BTreeMap::new()), VarOverrides::None)
        .unwrap();
    assert_eq!(
        hyperdrive_metadata(&built.manifest)[0]["connection_string"],
        LOCAL
    );

    let built = project
        .build(
            HyperdriveSource::Dev(strings(&[("HYPERDRIVE", PROD)])),
            VarOverrides::None,
        )
        .unwrap();
    assert_eq!(
        hyperdrive_metadata(&built.manifest)[0]["connection_string"],
        PROD
    );

    let project = Project::worker(json!([{ "binding": "HYPERDRIVE", "id": "example" }]));
    let error =
        error_text(project.build(HyperdriveSource::Dev(BTreeMap::new()), VarOverrides::None));
    assert!(error.contains("has no local connection string"), "{error}");
}

#[test]
fn config_shape_errors() {
    let cases = [
        (json!({}), "config `hyperdrive` must be an array"),
        (json!(["x"]), "entries must be objects"),
        (
            json!([{ "binding": "HYPERDRIVE", "id": "x", "origin": {} }]),
            "declares `origin`",
        ),
        (json!([{ "id": "x" }]), "has no `binding`"),
        (
            json!([{ "binding": "not valid", "id": "x" }]),
            "invalid hyperdrive binding name",
        ),
        (
            json!([{ "binding": "HYPERDRIVE" }]),
            "HYPERDRIVE has no `id`",
        ),
        (
            json!([{ "binding": "HYPERDRIVE", "id": "" }]),
            "HYPERDRIVE has no `id`",
        ),
        (
            json!([{ "binding": "HYPERDRIVE", "id": "x", "localConnectionString": 5 }]),
            "is not a string",
        ),
    ];
    for (hyperdrive, expected) in cases {
        let project = Project::worker(hyperdrive.clone());
        let error = error_text(project.build(
            HyperdriveSource::Production(strings(&[("HYPERDRIVE", PROD)])),
            VarOverrides::None,
        ));
        assert!(error.contains(expected), "{hyperdrive}: {error}");
    }

    let project = Project::worker(binding_entry());
    let error = error_text(project.build(
        HyperdriveSource::Production(strings(&[("HYPERDRIVE", "mysql://app:hunter2@db/app")])),
        VarOverrides::None,
    ));
    assert!(error.contains("MySQL is not supported"), "{error}");
    assert!(!error.contains(SECRET), "{error}");
}

#[test]
fn binding_names_must_be_unique() {
    let project = Project::new(json!({
        "name": "hd-test",
        "main": "index.js",
        "compatibility_date": "2026-01-01",
        "no_bundle": true,
        "hyperdrive": binding_entry(),
        "kv_namespaces": [{ "binding": "HYPERDRIVE", "id": "kv" }],
    }));
    let error = error_text(project.build(
        HyperdriveSource::Production(strings(&[("HYPERDRIVE", PROD)])),
        VarOverrides::None,
    ));
    assert!(error.contains("HYPERDRIVE"), "{error}");
}

#[test]
fn asset_only_projects_cannot_declare_hyperdrive() {
    let project = Project::new(json!({
        "name": "hd-test",
        "compatibility_date": "2026-01-01",
        "assets": { "directory": "./public" },
        "hyperdrive": binding_entry(),
    }));
    std::fs::create_dir(project.path().join("public")).unwrap();
    std::fs::write(project.path().join("public/index.html"), "hi").unwrap();
    let error = error_text(project.build(
        HyperdriveSource::Production(strings(&[("HYPERDRIVE", PROD)])),
        VarOverrides::None,
    ));
    assert!(
        error.contains("asset-only project cannot declare Worker bindings"),
        "{error}"
    );
}

#[test]
fn a_dotenv_entry_does_not_shadow_the_binding() {
    let project = Project::worker(binding_entry());
    let built = project
        .build(
            HyperdriveSource::Dev(BTreeMap::new()),
            VarOverrides::DotEnv(strings(&[("HYPERDRIVE", "shadow"), ("OTHER", "kept")])),
        )
        .unwrap();
    let bindings = built.manifest.raw_metadata["bindings"].as_array().unwrap();
    let named = |name: &str| {
        bindings
            .iter()
            .filter(|b| b["name"] == name)
            .collect::<Vec<_>>()
    };
    assert_eq!(named("HYPERDRIVE").len(), 1);
    assert_eq!(named("HYPERDRIVE")[0]["type"], "hyperdrive");
    assert_eq!(named("OTHER").len(), 1);
}

fn valid_manifest() -> Manifest {
    Project::worker(binding_entry())
        .build(
            HyperdriveSource::Production(strings(&[("HYPERDRIVE", PROD)])),
            VarOverrides::None,
        )
        .unwrap()
        .manifest
}

fn set_connection_string(manifest: &mut Manifest, value: Value) {
    for binding in manifest.raw_metadata["bindings"].as_array_mut().unwrap() {
        if binding["type"] == "hyperdrive" {
            match value {
                Value::Null => {
                    binding.as_object_mut().unwrap().remove("connection_string");
                }
                ref value => binding["connection_string"] = value.clone(),
            }
        }
    }
}

fn ungate(manifest: &mut Manifest) {
    manifest
        .required_features
        .retain(|feature| feature != protocol::FEATURE_HYPERDRIVE_V1);
}

// The manifests a node or the control plane must refuse, each with the error.
fn invalid_manifests() -> Vec<(&'static str, Manifest, &'static str)> {
    let mut cases = Vec::new();

    let mut manifest = valid_manifest();
    ungate(&mut manifest);
    cases.push(("ungated", manifest, "does not require hyperdrive-v1"));

    let mut manifest = valid_manifest();
    set_connection_string(&mut manifest, json!("postgres://app:hunter2@h/app?ssl=%ZZ"));
    cases.push(("bad escape", manifest, "two-digit escape"));

    let mut manifest = valid_manifest();
    set_connection_string(
        &mut manifest,
        json!("postgres://app:hunter2@h/app?ssl=true"),
    );
    cases.push(("ssl key", manifest, "query key \"ssl\" is not supported"));

    let mut manifest = valid_manifest();
    set_connection_string(&mut manifest, Value::Null);
    cases.push(("missing string", manifest, "has no connection string"));

    let mut manifest = valid_manifest();
    for binding in manifest.raw_metadata["bindings"].as_array_mut().unwrap() {
        if binding["type"] == "hyperdrive" {
            binding["id"] = json!("");
        }
    }
    cases.push(("empty id", manifest, "has no `id`"));

    let mut manifest = valid_manifest();
    for binding in manifest.raw_metadata["bindings"].as_array_mut().unwrap() {
        if binding["type"] == "hyperdrive" {
            binding["name"] = json!("not valid");
        }
    }
    cases.push(("bad name", manifest, "invalid environment name"));

    let mut manifest = valid_manifest();
    manifest.raw_metadata["bindings"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "type": "plain_text", "name": "HYPERDRIVE", "text": "x" }));
    cases.push((
        "collision",
        manifest,
        "shares its name with another binding",
    ));

    cases
}

#[test]
fn the_validator_refuses_invalid_manifests() {
    protocol::hyperdrive_bindings(&valid_manifest()).unwrap();
    for (label, manifest, expected) in invalid_manifests() {
        let error = error_text(protocol::hyperdrive_bindings(&manifest));
        assert!(error.contains(expected), "{label}: {error}");
        assert!(!error.contains(SECRET), "{label}: {error}");
    }
}

// Local-bucket operations run on celld's host runtime, which a process
// installs once; every test shares it.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        celld::asyncrt::set_host_handle(runtime.handle().clone());
        runtime
    })
}

struct LocalBucket {
    _dir: tempfile::TempDir,
    bucket: celld::bucket::Bucket,
}

fn local_bucket() -> LocalBucket {
    runtime();
    let dir = tempfile::tempdir().unwrap();
    let bucket = celld::dev::open_local_bucket(&dir.path().join("bucket.sqlite3")).unwrap();
    LocalBucket { _dir: dir, bucket }
}

// Every key with its body, to show that a refused deployment changed nothing.
async fn snapshot(bucket: &celld::bucket::Bucket) -> BTreeMap<String, Vec<u8>> {
    let mut objects = BTreeMap::new();
    for meta in bucket.list("").await.unwrap() {
        let key = meta.location.to_string();
        let (body, _) = bucket.get(&key).await.unwrap().unwrap();
        objects.insert(key, body.to_vec());
    }
    objects
}

fn pointer_for(manifest: &Manifest) -> Value {
    json!({
        "script_name": manifest.script_name,
        "version": manifest.version,
        "prefix": format!("deploy/{}/{}", manifest.script_name, manifest.version),
        "rollout": { "percent": 100 },
    })
}

fn agent_deployment(manifest: &Manifest) -> AgentDeployment {
    let modules = manifest
        .modules
        .iter()
        .map(|module| {
            json!({
                "name": module.name,
                "sha256": format!("{:0<64}", module.sha256),
                // Nothing listens here, so a deployment that passes
                // validation fails at its first download.
                "download_url": "http://127.0.0.1:9/module",
            })
        })
        .collect::<Vec<_>>();
    serde_json::from_value(json!({
        "script_name": manifest.script_name,
        "version": manifest.version,
        "manifest": manifest,
        "pointer": pointer_for(manifest),
        "modules": modules,
    }))
    .unwrap()
}

#[test]
fn the_control_plane_writes_nothing_for_an_invalid_manifest() {
    let client = reqwest::Client::new();
    runtime().block_on(async {
        // A bucket with an active deployment, whose pointers a refusal keeps.
        let apply = |manifest: Manifest| {
            let client = client.clone();
            async move {
                let local = local_bucket();
                let pointer = pointer_for(&valid_manifest()).to_string();
                for key in ["deploy/current.json", "deploy/hd-test/current.json"] {
                    local
                        .bucket
                        .put(key, pointer.clone().into_bytes())
                        .await
                        .unwrap();
                }
                let before = snapshot(&local.bucket).await;
                let result = celld::control_plane::apply_deployment(
                    &client,
                    "token",
                    &local.bucket,
                    &agent_deployment(&manifest),
                )
                .await;
                (result, before, snapshot(&local.bucket).await)
            }
        };

        // The control: a valid manifest gets past validation to the download,
        // so the refusals below come from the Hyperdrive check.
        let (result, _, _) = apply(valid_manifest()).await;
        let error = error_text(result);
        assert!(!error.contains("Hyperdrive"), "{error}");

        for (label, manifest, expected) in invalid_manifests() {
            let (result, before, after) = apply(manifest).await;
            let error = error_text(result);
            assert!(error.contains(expected), "{label}: {error}");
            assert_eq!(after, before, "{label}: the bucket changed");
        }
    });
}

#[test]
fn the_fleet_loader_refuses_an_invalid_manifest() {
    runtime().block_on(async {
        let publish = |manifest: Manifest| async move {
            let local = local_bucket();
            let pointer = pointer_for(&manifest);
            let prefix = pointer["prefix"].as_str().unwrap().to_string();
            local
                .bucket
                .put(
                    &format!("{prefix}/manifest.json"),
                    serde_json::to_vec(&manifest).unwrap(),
                )
                .await
                .unwrap();
            local
                .bucket
                .put(
                    "deploy/hd-test/current.json",
                    pointer.to_string().into_bytes(),
                )
                .await
                .unwrap();
            let result =
                celld::fleet::load_named_worker(&local.bucket, "hd-test", "node".into()).await;
            result.map(|_| ())
        };

        // The control: the modules were never written, so a manifest that
        // passes validation fails when the loader fetches them.
        let error = error_text(publish(valid_manifest()).await);
        assert!(error.contains("no such key"), "{error}");

        for (label, manifest, expected) in invalid_manifests() {
            let error = error_text(publish(manifest).await);
            assert!(error.contains(expected), "{label}: {error}");
        }
    });
}

// The report and `--json` come from the `celld deploy` command itself, and it
// reads the connection strings from its environment.
fn deploy_dry_run(project: &Project, json: bool, env: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_celld"));
    command.arg("deploy").arg(project.path()).arg("--dry-run");
    if json {
        command.arg("--json");
    }
    for (name, _) in std::env::vars() {
        if name.starts_with(deploy::HYPERDRIVE_PRODUCTION_PREFIX)
            || name.starts_with(deploy::HYPERDRIVE_LOCAL_PREFIX)
            || name == "CELLD_BUCKET"
        {
            command.env_remove(name);
        }
    }
    command.envs(env.iter().copied());
    command.output().unwrap()
}

#[test]
fn the_deploy_command_reads_only_the_production_variable() {
    let project = Project::worker(binding_entry());
    let production = "CELLD_HYPERDRIVE_CONNECTION_STRING_HYPERDRIVE";
    let local = "CLOUDFLARE_HYPERDRIVE_LOCAL_CONNECTION_STRING_HYPERDRIVE";

    let output = deploy_dry_run(&project, false, &[(local, PROD)]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(production), "{stderr}");

    for json in [false, true] {
        let output = deploy_dry_run(&project, json, &[(production, PROD), (local, LOCAL)]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(
            stderr.contains("postgres://db.internal:5432/app"),
            "the report lacks the redacted endpoint: {stderr}"
        );
        for secret in [PROD, SECRET, LOCAL, "devpw"] {
            assert!(
                !stdout.contains(secret),
                "stdout shows {secret:?}: {stdout}"
            );
            assert!(
                !stderr.contains(secret),
                "stderr shows {secret:?}: {stderr}"
            );
        }
        if json {
            let row: Value = serde_json::from_str(stdout.trim()).unwrap();
            assert_eq!(row["dry_run"], true, "{stdout}");
        }
    }
}
