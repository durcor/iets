//! Locally built input-addressed outputs, pinned as raw input sources, must
//! reach an independent builder without substituters or their build closures.
#![cfg(target_os = "linux")]

mod e2e;
mod hermetic;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;

use e2e::{Builder, Store, ok, stdout};
use serde_json::{Value, json};

fn path_info(s: &Store, path: &str, b: Option<&Builder>) -> Value {
    let mut c = s.nix("nix");
    c.arg("path-info");
    if let Some(b) = b {
        c.args(["--store", &b.store()]);
    }
    let info: Value = serde_json::from_slice(&ok(c.args(["--json", path])).stdout).unwrap();
    match info {
        Value::Array(mut a) => {
            assert_eq!(a.len(), 1);
            a.remove(0)
        }
        Value::Object(mut m) => m.remove(path).unwrap(),
        _ => panic!("unexpected path-info: {info}"),
    }
}

/// Registration time is destination-local; ultimate must not travel with a copy.
fn identity(info: &Value) -> Value {
    ["ca", "narHash", "narSize", "references", "deriver", "signatures"]
        .into_iter()
        .map(|key| (key.to_string(), info[key].clone()))
        .collect()
}

fn valid(s: &Store, path: &str, b: Option<&Builder>) -> bool {
    let mut c = s.nix("nix-store");
    if let Some(b) = b {
        c.args(["--store", &b.store()]);
    }
    c.args(["--check-validity", path])
        .output()
        .unwrap()
        .status
        .success()
}

fn sorted(paths: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut paths: Vec<_> = paths.into_iter().collect();
    paths.sort();
    paths
}

fn references(info: &Value) -> Vec<String> {
    sorted(
        info["references"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_string()),
    )
}

fn closure(s: &Store, path: &str, b: &Builder) -> Vec<String> {
    let out = ok(s
        .nix("nix-store")
        .args(["--store", &b.store(), "-q", "--requisites", path]));
    sorted(stdout(&out).lines().map(str::to_string))
}

struct Source {
    path: String,
    bytes: Vec<u8>,
    info: Value,
}

impl Source {
    fn read(s: &Store, path: String) -> Self {
        Self {
            bytes: fs::read(&path).unwrap(),
            info: path_info(s, &path, None),
            path,
        }
    }

    fn assert_copied(&self, s: &Store, b: &Builder) {
        assert!(valid(s, &self.path, Some(b)), "{}", self.path);
        assert_eq!(fs::read(b.real(&self.path)).unwrap(), self.bytes);
        let copied = path_info(s, &self.path, Some(b));
        assert_eq!(identity(&copied), identity(&self.info), "{}", self.path);
        assert_ne!(copied["ultimate"], true, "copied paths are not ultimate");
    }
}

fn plain_consumer(input: &str) -> String {
    format!(
        r#"let input = builtins.storePath "{input}"; in derivation {{
  name = "@NAME@";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" ''
    while IFS= read -r line; do printf '%s\n' "$line"; done < ${{input}} > "$out"
  '' ];
}}
"#
    )
}

struct Fixture {
    sources: Vec<Source>,
    forbidden: Vec<String>,
    input: String,
    consumer_body: String,
    expected: Vec<u8>,
    output_refs: Vec<String>,
}

impl Fixture {
    fn new(s: &Store, transitive: bool) -> Self {
        let producers = s.write(
            "producers.nix",
            r#"let
  buildOnly = builtins.toFile "source-transfer-build-only.txt" "producer-only prerequisite\n";
  mk = name: attrs: derivation ({
    inherit name buildOnly;
    salt = "@NONCE@";
    system = "@SYSTEM@";
    builder = "@SHELL@";
  } // attrs);
  leaf = mk "source-transfer-leaf.yaml" {
    args = [ "-c" ''
      test -r "$buildOnly"
      printf 'enabled: true\nmode: dual-function\n' > "$out"
    '' ];
  };
  yaml = mk "source-transfer-linked.yaml" {
    inherit leaf;
    args = [ "-c" ''
      test -r "$buildOnly"
      printf 'dependency: %s\nself: %s\n' "$leaf" "$out" > "$out"
    '' ];
  };
in { inherit leaf yaml buildOnly; }
"#,
        );
        let mut forbidden = Vec::new();
        let mut outputs = Vec::new();
        let attrs = if transitive {
            &["leaf", "yaml"][..]
        } else {
            &["leaf"][..]
        };
        for attr in attrs {
            let drv = stdout(&ok(s
                .nix("nix-instantiate")
                .arg(&producers)
                .args(["-A", attr])));
            outputs.push(s.nix_store(&["-q", "--outputs", &drv]));
            forbidden.push(drv);
        }
        let build_only: String = serde_json::from_slice(
            &ok(s.nix("nix-instantiate").arg(&producers).args([
                "--eval", "--strict", "--json", "-A", "buildOnly",
            ]))
            .stdout,
        )
        .unwrap();
        forbidden.push(build_only);
        let built = stdout(&ok(s.nix("nix-build").arg(&producers).args([
            "--no-out-link",
            "-A",
            if transitive { "yaml" } else { "leaf" },
        ])));
        assert_eq!(built, *outputs.last().unwrap());
        let mut sources: Vec<_> = outputs.into_iter().map(|p| Source::read(s, p)).collect();
        for (source, drv) in sources.iter().zip(&forbidden) {
            assert_eq!(source.info["deriver"], *drv);
        }

        // The valid runtime closure survives without its producer derivations
        // and producer-only input. Neither side can repair this by rebuilding it.
        let mut delete = s.nix("nix-store");
        delete.arg("--delete").args(&forbidden);
        ok(&mut delete);
        for path in &forbidden {
            assert!(!valid(s, path, None), "fixture still has {path}");
        }
        assert_eq!(sources[0].bytes, b"enabled: true\nmode: dual-function\n");
        for source in &sources {
            assert!(source.info["ca"].is_null(), "must be input-addressed");
            assert!(
                source.info["signatures"].as_array().is_none_or(Vec::is_empty),
                "the trusted import must work without signatures"
            );
            assert!(valid(s, &source.path, None));
            assert_eq!(
                identity(&path_info(s, &source.path, None)),
                identity(&source.info),
                "removing producer build inputs must not change runtime metadata"
            );
        }
        assert!(references(&sources[0].info).is_empty());
        if !transitive {
            return Self {
                input: sources[0].path.clone(),
                expected: sources[0].bytes.clone(),
                consumer_body: plain_consumer(&sources[0].path),
                sources,
                forbidden,
                output_refs: Vec::new(),
            };
        }

        let leaf = sources[0].path.clone();
        let yaml = sources[1].path.clone();
        assert_eq!(
            sources[1].bytes,
            format!("dependency: {leaf}\nself: {yaml}\n").as_bytes()
        );
        assert_eq!(references(&sources[1].info), sorted([leaf.clone(), yaml.clone()]));
        let consumer_body = r#"let
  pinned = builtins.storePath "@YAML@";
  input = builtins.toFile "source-transfer-input.txt" "${pinned}\n";
in derivation {
  name = "@NAME@";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" ''
    {
      printf 'manifest: %s\n' '${input}'
      IFS= read -r yaml < '${input}'
      printf 'source: %s\n' "$yaml"
      leaf=
      while IFS=' ' read -r key value; do
        printf '%s %s\n' "$key" "$value"
        case "$key" in dependency:) leaf="$value";; esac
      done < "$yaml"
      test -n "$leaf"
      while IFS= read -r line; do printf '%s\n' "$line"; done < "$leaf"
    } > "$out"
  '' ];
}
"#
        .replace("@YAML@", &yaml);
        // Nix is only the fixture oracle. The transfer and the consumer build
        // below use iets; the eval-time text must be registered after its refs.
        let oracle = s.write("oracle-consumer.nix", &consumer_body.replace("@NAME@", "oracle-consumer"));
        let drv = stdout(&ok(s.nix("nix-instantiate").arg(&oracle)));
        let input = s.nix_store(&["-q", "--references", &drv]);
        assert!(!input.contains('\n'), "one raw input source, not producer inputDrvs");
        sources.push(Source::read(s, input.clone()));
        assert_eq!(sources[2].bytes, format!("{yaml}\n").as_bytes());
        assert_eq!(references(&sources[2].info), [yaml.clone()]);
        let expected = format!(
            "manifest: {input}\nsource: {yaml}\ndependency: {leaf}\nself: {yaml}\nenabled: true\nmode: dual-function\n"
        )
        .into_bytes();
        Self {
            output_refs: sorted([input.clone(), leaf, yaml]),
            input,
            sources,
            forbidden,
            consumer_body,
            expected,
        }
    }

    fn consumer(&self, s: &Store, name: &str) -> Consumer {
        let file = s.write(
            &format!("{name}.nix"),
            &self.consumer_body.replace("@NAME@", name),
        );
        let drv = stdout(&ok(s.nix("nix-instantiate").arg(&file)));
        // This must be a raw pinned source, never an edge to its producer.
        assert_eq!(s.nix_store(&["-q", "--references", &drv]), self.input);
        let out = s.nix_store(&["-q", "--outputs", &drv]);
        let drv_bytes = fs::read(&drv).unwrap();
        Consumer {
            file,
            drv,
            drv_bytes,
            out,
        }
    }

    fn assert_destination(&self, s: &Store, b: &Builder) {
        for source in &self.sources {
            source.assert_copied(s, b);
        }
        assert_eq!(
            closure(s, &self.input, b),
            sorted(self.sources.iter().map(|source| source.path.clone()))
        );
        for path in &self.forbidden {
            assert!(!valid(s, path, Some(b)), "copied producer build closure: {path}");
        }
    }
}

struct Consumer {
    file: PathBuf,
    drv: String,
    drv_bytes: Vec<u8>,
    out: String,
}

impl Consumer {
    fn build(&self, s: &Store, b: &Builder, f: &Fixture) {
        let out = ok(s
            .iets()
            .args(["build", "--builder", &b.url(), "--impure", "--no-tracing-cache"])
            .arg(&self.file));
        self.assert_result(s, b, f, &out);
    }

    fn assert_result(&self, s: &Store, b: &Builder, f: &Fixture, out: &Output) {
        assert_eq!(stdout(out), self.out, "original input-addressed output identity");
        assert_eq!(fs::read(b.real(&self.out)).unwrap(), f.expected);
        let info = path_info(s, &self.out, Some(b));
        assert!(info["ca"].is_null());
        assert_eq!(info["deriver"], self.drv);
        assert_eq!(references(&info), f.output_refs);
        assert_eq!(
            closure(s, &self.out, b),
            sorted(f.output_refs.iter().cloned().chain([self.out.clone()]))
        );
        assert_eq!(fs::read(b.real(&self.drv)).unwrap(), self.drv_bytes);
        f.assert_destination(s, b);
    }
}

#[test]
fn a_locally_built_yaml_reaches_a_cold_unsubstituted_builder() {
    let s = store!();
    let fixture = Fixture::new(&s, false);
    let consumer = fixture.consumer(&s, "source-transfer-simple-consumer");
    let b = s.builder("cold", &[("require-sigs", "true")]);
    assert!(!valid(&s, &fixture.input, Some(&b)));
    assert!(!valid(&s, &consumer.out, Some(&b)));
    consumer.build(&s, &b, &fixture);
}

#[test]
fn a_transitive_self_referencing_closure_precedes_eval_text_cold_and_warm() {
    let s = store!();
    let fixture = Fixture::new(&s, true);
    let consumer = fixture.consumer(&s, "source-transfer-closure-consumer");
    for preseed_leaf in [false, true] {
        let b = s.builder(if preseed_leaf { "partial" } else { "cold" }, &[("require-sigs", "true")]);
        for source in &fixture.sources {
            assert!(!valid(&s, &source.path, Some(&b)));
        }
        if preseed_leaf {
            // Nix imports only the independent leaf as fixture setup, using the
            // same already-trusted destination capability as ordinary copies.
            let leaf = &fixture.sources[0];
            let imported = e2e::pipe(
                s.nix("nix-store").args(["--export", &leaf.path]),
                s.nix("nix-store").args(["--store", &b.store(), "--import"]),
            );
            assert!(imported.status.success(), "{}", e2e::stderr(&imported));
            leaf.assert_copied(&s, &b);
        }
        consumer.build(&s, &b, &fixture);
        let before: Vec<_> = fixture
            .sources
            .iter()
            .map(|source| path_info(&s, &source.path, Some(&b)))
            .collect();
        consumer.build(&s, &b, &fixture);
        let after: Vec<_> = fixture
            .sources
            .iter()
            .map(|source| path_info(&s, &source.path, Some(&b)))
            .collect();
        assert_eq!(before, after, "warm destination metadata stays unchanged");
    }
}

#[test]
fn eval_jobs_streams_the_local_source_closure_before_native_builds() {
    let s = store!();
    let fixture = Fixture::new(&s, true);
    let first = fixture.consumer(&s, "source-transfer-stream-first");
    let second = fixture.consumer(&s, "source-transfer-stream-second");
    let jobs = s.write(
        "jobs.nix",
        &format!(
            "{{ first = import {}; second = import {}; }}\n",
            first.file.display(),
            second.file.display()
        ),
    );
    let b = s.builder("stream", &[("require-sigs", "true")]);
    for source in &fixture.sources {
        assert!(!valid(&s, &source.path, Some(&b)));
    }
    for consumer in [&first, &second] {
        assert!(!valid(&s, &consumer.drv, Some(&b)));
    }
    // eval-jobs instantiates but does not build. Check its effects before a
    // fixed build could hide a failure to send the streamed source closure.
    for _ in 0..2 {
        let out = ok(s
            .iets()
            .args(["eval-jobs", "--eval-store", &b.url(), "--workers", "2", "--impure"])
            .arg(&jobs));
        let mut lines: Vec<Value> = stdout(&out)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        lines.sort_by(|a, b| a["attr"].as_str().cmp(&b["attr"].as_str()));
        assert_eq!(lines.len(), 2);
        for (line, (attr, consumer)) in lines.iter().zip([("first", &first), ("second", &second)]) {
            assert_eq!(line["attr"], attr);
            assert_eq!(line["drvPath"], consumer.drv);
            assert_eq!(line["outputs"], json!({ "out": consumer.out }));
            assert!(!valid(&s, &consumer.out, Some(&b)), "eval-jobs must not build");
            assert_eq!(fs::read(b.real(&consumer.drv)).unwrap(), consumer.drv_bytes);
        }
        fixture.assert_destination(&s, &b);
    }
    first.build(&s, &b, &fixture);
    second.build(&s, &b, &fixture);
}

#[test]
fn a_genuinely_missing_source_is_still_substituted() {
    let s = store!();
    let input: String = serde_json::from_slice(
        &ok(s.nix("nix-instantiate").args([
            "--eval",
            "--strict",
            "--json",
            "-E",
            r#"builtins.toFile "source-transfer-cached.yaml" "enabled: true\nmode: cached\n""#,
        ]))
        .stdout,
    )
    .unwrap();
    let source = Source::read(&s, input.clone());
    assert_eq!(source.bytes, b"enabled: true\nmode: cached\n");
    assert!(source.info["ca"].as_str().unwrap().starts_with("text:sha256:"));
    let fixture = Fixture {
        expected: source.bytes.clone(),
        consumer_body: plain_consumer(&input),
        input: input.clone(),
        sources: vec![source],
        forbidden: Vec::new(),
        output_refs: Vec::new(),
    };
    let consumer = fixture.consumer(&s, "source-transfer-substituted-consumer");
    let cache = format!("file://{}/source-cache", s.dir().display());
    ok(s.nix("nix").args(["copy", "--to", &cache, &input]));
    ok(s.nix("nix-store").args(["--delete", &consumer.drv, &input]));
    assert!(!valid(&s, &input, None));
    let b = s.builder("substituting", &[("substituters", &cache), ("require-sigs", "true")]);
    assert!(!valid(&s, &input, Some(&b)));
    assert!(!valid(&s, &consumer.out, Some(&b)));
    consumer.build(&s, &b, &fixture);
}

#[test]
fn an_untrusted_destination_rejects_unsigned_ia_but_accepts_signed_ia_and_ca() {
    let s = store!();
    let mut fixture = Fixture::new(&s, false);
    let consumer = fixture.consumer(&s, "source-transfer-untrusted-consumer");
    let secret_key = s.dir().join("fixture-signing-key.sec");
    let public_key_file = s.dir().join("fixture-signing-key.pub");
    ok(s.nix("nix-store")
        .args(["--generate-binary-cache-key", "source-transfer-fixture"])
        .arg(&secret_key)
        .arg(&public_key_file));
    let public_key = fs::read_to_string(&public_key_file).unwrap();
    let public_key = public_key.trim();
    let b = s.builder(
        "trust-boundary",
        &[("require-sigs", "true"), ("trusted-public-keys", public_key)],
    );

    // As in the existing SSH transport fixture, the executable launcher avoids
    // needing an SSH server. All worker requests go to a real nix-daemon, whose
    // per-connection override makes even a same-uid/root client untrusted.
    let ssh = s.write(
        "untrusted-bin/ssh",
        &format!(
            "#!/bin/sh\nexec '{}' --store '{}' --extra-experimental-features daemon-trust-override --stdio --force-untrusted --option sandbox-paths /nix/store --option require-sigs true --option trusted-public-keys '{}'\n",
            e2e::resolved("nix-daemon").display(),
            b.store(),
            public_key
        ),
    );
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        ssh.parent().unwrap().display(),
        std::env::var("PATH").unwrap()
    );
    let url = "ssh-ng://forced-untrusted";

    // A content-addressed source and its consumer must work over the exact
    // same untrusted endpoint. A startup/transport failure cannot satisfy the
    // rejection assertion below, nor can rejecting every unsigned path.
    let input: String = serde_json::from_slice(
        &ok(s.nix("nix-instantiate").args([
            "--eval",
            "--strict",
            "--json",
            "-E",
            r#"builtins.toFile "source-transfer-trust-control.yaml" "enabled: true\nmode: untrusted-ca\n""#,
        ]))
        .stdout,
    )
    .unwrap();
    let source = Source::read(&s, input.clone());
    assert!(source.info["ca"].as_str().unwrap().starts_with("text:sha256:"));
    let control = Fixture {
        expected: b"enabled: true\nmode: untrusted-ca\n".to_vec(),
        consumer_body: plain_consumer(&input),
        input,
        sources: vec![source],
        forbidden: Vec::new(),
        output_refs: Vec::new(),
    };
    let ca_consumer = control.consumer(&s, "source-transfer-trust-control-consumer");
    assert!(!valid(&s, &control.input, Some(&b)));
    let accepted = ok(s
        .iets()
        .env("PATH", &path)
        .args(["build", "--builder", url, "--impure", "--no-tracing-cache"])
        .arg(&ca_consumer.file));
    ca_consumer.assert_result(&s, &b, &control, &accepted);

    assert!(!valid(&s, &fixture.input, Some(&b)));
    assert!(!valid(&s, &consumer.drv, Some(&b)));
    assert!(!valid(&s, &consumer.out, Some(&b)));
    let rejected = s
        .iets()
        .env("PATH", &path)
        .args(["build", "--builder", url, "--impure", "--no-tracing-cache"])
        .arg(&consumer.file)
        .output()
        .unwrap();
    assert!(
        !rejected.status.success(),
        "an untrusted connection must not import an unsigned IA source: {}",
        e2e::stderr(&rejected)
    );
    assert!(!valid(&s, &fixture.input, Some(&b)));
    assert!(!valid(&s, &consumer.out, Some(&b)));

    let jobs = s.write(
        "untrusted-jobs.nix",
        &format!("{{ consumer = import {}; }}\n", consumer.file.display()),
    );
    let rejected = s
        .iets()
        .env("PATH", &path)
        .args(["eval-jobs", "--eval-store", url, "--workers", "1", "--impure"])
        .arg(jobs)
        .output()
        .unwrap();
    assert!(
        !rejected.status.success(),
        "streamed uploads must retain the same import trust boundary: {}",
        e2e::stderr(&rejected)
    );
    assert!(!valid(&s, &fixture.input, Some(&b)));
    assert!(!valid(&s, &consumer.out, Some(&b)));

    // The same unsigned bytes are accepted through an already-trusted
    // connection, without making them valid on the untrusted destination.
    let trusted = s.builder("trusted-comparison", &[("require-sigs", "true")]);
    consumer.build(&s, &trusted, &fixture);
    assert!(!valid(&s, &fixture.input, Some(&b)));

    // Only fixture-local keys and daemon options participate. Signing must
    // change neither store identity, NAR bytes, nor any other copied metadata.
    ok(s.nix("nix")
        .args(["store", "sign", "--key-file"])
        .arg(&secret_key)
        .arg(&fixture.input));
    let signed = Source::read(&s, fixture.input.clone());
    assert_eq!(signed.bytes, fixture.sources[0].bytes);
    let signatures = signed.info["signatures"].as_array().unwrap();
    assert_eq!(signatures.len(), 1);
    assert_eq!(
        signatures[0].as_str().unwrap().split_once(':').unwrap().0,
        "source-transfer-fixture"
    );
    let mut expected_identity = identity(&fixture.sources[0].info);
    expected_identity["signatures"] = signed.info["signatures"].clone();
    assert_eq!(identity(&signed.info), expected_identity);
    fixture.sources[0] = signed;

    let accepted = ok(s
        .iets()
        .env("PATH", &path)
        .args(["build", "--builder", url, "--impure", "--no-tracing-cache"])
        .arg(&consumer.file));
    // This checks the exact nonempty signatures, not just successful import.
    consumer.assert_result(&s, &b, &fixture, &accepted);
}
