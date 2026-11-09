//! Legacy absolute `path` lock nodes select an independent pinned source,
//! not the parent Git checkout's filtered view of the current directory.
#![cfg(target_os = "linux")]

mod e2e;
mod hermetic;

use std::fs::{self, File, FileTimes};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, UNIX_EPOCH};

use e2e::{Builder, Store, git, ok, parse, stderr, stdout};
use serde_json::{Value, json};

const INPUT_MTIME: u64 = 1_700_000_000;
const LOCK_MTIME: u64 = 1_234_567_890;

const INPUT: &str = r#"{
  outputs = { self }: {
    tracked = builtins.readFile (self + "/tracked");
    untracked = builtins.readFile (self + "/untracked");
  };
}
"#;

const ROOT: &str = r#"{
  inputs.dep.url = "path:@INPUT@";
  inputs.rel.url = "path:./rel";
  outputs = { self, dep, rel }: let
    consumer = derivation {
      name = "locked-path-consumer";
      system = "@SYSTEM@";
      builder = "@SHELL@";
      salt = "@NONCE@";
      input = dep.outPath;
      args = [ "-c" ''
        {
          while IFS= read -r line; do printf '%s\n' "$line"; done < "$input/tracked"
          while IFS= read -r line; do printf '%s\n' "$line"; done < "$input/untracked"
        } > "$out"
      '' ];
    };
    public = value: builtins.seq dep.outPath value;
  in {
    inherit consumer;
    rootSource = self.sourceInfo.outPath;
    snapshot = {
      source = dep.sourceInfo.outPath;
      info = removeAttrs dep.sourceInfo [ "outPath" ];
      inherit (dep) tracked untracked;
      names = builtins.attrNames (builtins.readDir dep.outPath);
      drv = consumer.drvPath;
      output = consumer.outPath;
    };
    readTracked = public (builtins.readFile "@INPUT@/tracked");
    readUntracked = public (builtins.readFile "@INPUT@/untracked");
    publicPath = public (let
      source = builtins.path { path = "@INPUT@"; name = "source"; };
    in {
      tracked = builtins.readFile "${source}/tracked";
      hidden = builtins.pathExists "${source}/untracked";
    });
    publicFilteredFetch = public (let
      source = builtins.fetchTree {
        type = "path";
        path = "@INPUT@";
        narHash = "@FILTERED_HASH@";
      };
    in {
      tracked = builtins.readFile "${source}/tracked";
      hidden = builtins.pathExists "${source}/untracked";
    });
    publicFullFetch = public (builtins.readFile ((builtins.fetchTree {
      type = "path";
      path = "@INPUT@";
      narHash = "@HASH@";
    }).outPath + "/untracked"));
    relative = {
      data = rel.value;
      hidden = builtins.pathExists (rel.outPath + "/untracked");
      source = rel.outPath;
      parent = self.outPath;
      info = removeAttrs rel.sourceInfo [ "outPath" ];
      rootInfo = removeAttrs self.sourceInfo [ "outPath" ];
    };
  };
}
"#;

fn hash_path(s: &Store, path: &Path) -> String {
    stdout(&ok(s
        .nix("nix")
        .args(["hash", "path", "--type", "sha256", "--sri"])
        .arg(path)))
}

/// This computes an identity only: unlike fetchTree, it cannot populate the
/// store and accidentally turn a native cold-source test into a cache test.
fn source_path(s: &Store, hash: &str) -> PathBuf {
    PathBuf::from(s.nix_store(&[
        "--print-fixed-path",
        "--recursive",
        "sha256",
        hash,
        "source",
    ]))
}

fn set_mtimes(path: &Path) {
    if path.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            set_mtimes(&entry.unwrap().path());
        }
    }
    File::open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(INPUT_MTIME)))
        .unwrap();
}

fn write_input(s: &Store, path: &Path, untracked: bool) {
    s.write(path.join("flake.nix").to_str().unwrap(), INPUT);
    s.write(path.join("tracked").to_str().unwrap(), "tracked @NONCE@\n");
    if untracked {
        s.write(
            path.join("untracked").to_str().unwrap(),
            "untracked @NONCE@\n",
        );
    }
    set_mtimes(path);
}

fn valid(s: &Store, path: &Path, builder: Option<&Builder>) -> bool {
    let mut cmd = s.nix("nix-store");
    if let Some(builder) = builder {
        cmd.args(["--store", &builder.store()]);
    } else {
        cmd.args(["--store", "local"]);
    }
    cmd.arg("--check-validity")
        .arg(path)
        .output()
        .unwrap()
        .status
        .success()
}

fn path_info(s: &Store, path: &str, builder: Option<&Builder>) -> Value {
    let mut cmd = s.nix("nix");
    cmd.arg("path-info");
    if let Some(builder) = builder {
        cmd.args(["--store", &builder.store()]);
    } else {
        cmd.args(["--store", "local"]);
    }
    let info = parse(&ok(cmd.args(["--json", path])));
    match info {
        Value::Array(mut entries) => {
            assert_eq!(entries.len(), 1);
            entries.remove(0)
        }
        Value::Object(mut entries) => entries.remove(path).unwrap(),
        _ => panic!("unexpected path-info: {info}"),
    }
}

fn source_identity(info: &Value) -> Value {
    ["ca", "narHash", "narSize", "references"]
        .into_iter()
        .map(|key| (key.to_string(), info[key].clone()))
        .collect()
}

struct Fixture {
    s: Store,
    repo: PathBuf,
    input: PathBuf,
    hash: String,
    filtered_hash: String,
    source: PathBuf,
    last_modified: Option<u64>,
}

impl Fixture {
    fn new(last_modified: Option<u64>) -> Option<Self> {
        let s = Store::start_with(&[("keep-derivations", "false")])?;
        let repo = s.dir().join("repo");
        let input = repo.join("dep");
        write_input(&s, &input, false);
        let filtered_hash = hash_path(&s, &input);
        s.write(
            repo.join("rel/flake.nix").to_str().unwrap(),
            r#"{ outputs = { self }: { value = builtins.readFile (self + "/tracked"); }; }
"#,
        );
        s.write(repo.join("rel/tracked").to_str().unwrap(), "relative one\n");
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "dep", "rel"]);
        // The independent input includes this file. The ordinary Git root
        // and the unpinned relative input must continue to exclude it.
        write_input(&s, &input, true);
        s.write(repo.join("rel/untracked").to_str().unwrap(), "private\n");
        let hash = hash_path(&s, &input);
        assert_ne!(hash, filtered_hash);
        let source = source_path(&s, &hash);
        assert!(!source.exists(), "fixture materialized its cold source");
        let git_bin = s.dir().join("git-only");
        fs::create_dir(&git_bin).unwrap();
        symlink(e2e::resolved("git"), git_bin.join("git")).unwrap();
        // Cache metadata is allowed, but source evaluation/fetch/build must
        // remain native. Forward only the pinned local path-info command to
        // real Nix; never synthesize metadata or allow delegated operations.
        let metadata_bin = s.dir().join("metadata-only");
        fs::create_dir(&metadata_bin).unwrap();
        let wrapper = metadata_bin.join("nix");
        fs::write(
            &wrapper,
            format!(
                "#!{}\nif [ \"$#\" -ge 7 ] && [ \"$1\" = --store ] && [ \"$2\" = local ] && [ \"$3\" = --extra-experimental-features ] && [ \"$4\" = nix-command ] && [ \"$5\" = path-info ] && [ \"$6\" = --json ]; then\n  exec {:?} \"$@\"\nfi\nprintf '%s\\n' \"$*\" >> {:?}\nexit 97\n",
                e2e::resolved("bash").display(),
                e2e::resolved("nix"),
                s.dir().join("forbidden-nix-operations"),
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let f = Self {
            s,
            repo,
            input,
            hash,
            filtered_hash,
            source,
            last_modified,
        };
        f.configure_input(&f.input);
        git(&f.repo, &["commit", "-qm", "legacy locked path fixture"]);
        Some(f)
    }

    fn configure_input(&self, input: &Path) {
        self.s.write(
            self.repo.join("flake.nix").to_str().unwrap(),
            &ROOT
                .replace("@INPUT@", input.to_str().unwrap())
                .replace("@HASH@", &self.hash)
                .replace("@FILTERED_HASH@", &self.filtered_hash),
        );
        let mut locked = json!({
            "type": "path", "path": input, "narHash": self.hash,
        });
        if let Some(date) = self.last_modified {
            locked["lastModified"] = date.into();
        }
        let lock = json!({
            "nodes": {
                "root": { "inputs": { "dep": "dep", "rel": "rel" } },
                "dep": {
                    "locked": locked,
                    "original": { "type": "path", "path": input },
                },
                "rel": {
                    "locked": { "type": "path", "path": "./rel" },
                    "original": { "type": "path", "path": "./rel" },
                    "parent": [],
                },
            },
            "root": "root", "version": 7,
        });
        fs::write(self.repo.join("flake.lock"), lock.to_string()).unwrap();
        git(&self.repo, &["add", "flake.nix", "flake.lock"]);
    }

    fn native(&self) -> Command {
        let mut cmd = self.s.iets();
        cmd.env("IETS_CACHE_DIR", self.s.dir().join("native-cache"))
            .env("IETS_NIX_BIN_DIR", self.s.dir().join("metadata-only"))
            .env("PATH", self.s.dir().join("git-only"))
            .env("IETS_PROVENANCE", "0");
        cmd
    }

    fn reference(&self, attr: &str) -> String {
        format!("{}#{attr}", self.repo.display())
    }

    fn eval(&self, attr: &str, cached: bool) -> Output {
        let mut cmd = self.native();
        cmd.args(["eval", "--json", "--pure-eval"]);
        if !cached {
            cmd.args(["--no-cache", "--no-tracing-cache"]);
        }
        let out = cmd.arg(self.reference(attr)).output().unwrap();
        self.assert_metadata_only();
        out
    }

    fn assert_metadata_only(&self) {
        assert!(
            !self.s.dir().join("forbidden-nix-operations").exists(),
            "delegated a non-metadata Nix operation"
        );
    }

    fn value(&self, attr: &str, cached: bool) -> Value {
        let out = self.eval(attr, cached);
        assert!(out.status.success(), "{}", stderr(&out));
        parse(&out)
    }

    fn oracle(&self, attr: &str) -> Value {
        parse(&ok(self.s.nix("nix").args([
            "eval",
            "--json",
            "--no-write-lock-file",
            "--option",
            "eval-cache",
            "false",
            &self.reference(attr),
        ])))
    }

    /// Deliberately populate the real Nix store/fetcher cache, without
    /// evaluating the parent flake or making an iets cache entry.
    fn seed(&self) {
        let mut attrs = json!({
            "type": "path", "path": self.input, "narHash": self.hash,
        });
        if let Some(date) = self.last_modified {
            attrs["lastModified"] = date.into();
        }
        let encoded = serde_json::to_string(&attrs.to_string()).unwrap();
        let expr = format!(
            "let t = builtins.fetchTree (builtins.fromJSON {encoded}); in {{ source = t.outPath; info = removeAttrs t [ \"outPath\" ]; }}"
        );
        let tree = parse(&ok(self
            .s
            .nix("nix")
            .args(["--store", "local"])
            .args(["eval", "--json", "--impure", "--expr", &expr])));
        assert_eq!(tree["source"], self.source.to_str().unwrap());
        assert!(valid(&self.s, &self.source, None));
        assert_eq!(hash_path(&self.s, &self.source), self.hash);
    }

    fn assert_snapshot(&self, value: &Value) {
        assert_eq!(value["source"], self.source.to_str().unwrap());
        assert_eq!(value["info"]["narHash"], self.hash);
        assert_eq!(
            value["info"].get("lastModified").and_then(Value::as_u64),
            self.last_modified
        );
        if self.last_modified.is_none() {
            assert!(value["info"].get("lastModifiedDate").is_none());
        }
        if self.last_modified == Some(LOCK_MTIME) {
            assert_eq!(value["info"]["lastModifiedDate"], "20090213233130");
        }
        assert_eq!(value["names"], json!(["flake.nix", "tracked", "untracked"]));
        assert_eq!(
            value["tracked"],
            fs::read_to_string(self.s.dir().join("expected-tracked")).unwrap()
        );
        assert_eq!(
            value["untracked"],
            fs::read_to_string(self.s.dir().join("expected-untracked")).unwrap()
        );
    }

    fn save_expected(&self) {
        fs::copy(
            self.input.join("tracked"),
            self.s.dir().join("expected-tracked"),
        )
        .unwrap();
        fs::copy(
            self.input.join("untracked"),
            self.s.dir().join("expected-untracked"),
        )
        .unwrap();
    }

    fn assert_rejected(&self, attr: &str, cached: bool) -> Output {
        let out = self.eval(attr, cached);
        assert!(
            !out.status.success(),
            "unexpectedly exposed {attr}: {}",
            stdout(&out)
        );
        assert!(out.stdout.is_empty(), "rejected {attr} returned a value");
        out
    }

    fn assert_build(&self, snapshot: &Value, name: &str) -> Builder {
        let builder = self.s.builder(name, &[("require-sigs", "true")]);
        assert!(!valid(&self.s, &self.source, Some(&builder)));
        let out = ok(self
            .native()
            .args([
                "build",
                "--builder",
                &builder.url(),
                "--pure-eval",
                "--no-cache",
                "--no-tracing-cache",
            ])
            .arg(self.reference("consumer")));
        self.assert_metadata_only();
        let output = snapshot["output"].as_str().unwrap();
        assert_eq!(stdout(&out), output);
        let mut expected = fs::read(self.s.dir().join("expected-tracked")).unwrap();
        expected.extend(fs::read(self.s.dir().join("expected-untracked")).unwrap());
        assert_eq!(fs::read(builder.real(output)).unwrap(), expected);
        let info = path_info(&self.s, output, Some(&builder));
        assert_eq!(info["deriver"], snapshot["drv"]);
        assert_eq!(
            hash_path(&self.s, &builder.real(self.source.to_str().unwrap())),
            self.hash
        );
        builder
    }

    fn assert_nar_parity(&self, builder: &Builder) {
        let source = self.source.to_str().unwrap();
        let oracle = path_info(&self.s, source, None);
        let native = path_info(&self.s, source, Some(builder));
        assert_eq!(source_identity(&native), source_identity(&oracle));
        assert_eq!(native["narHash"], self.hash);
        assert!(native["ca"].is_string(), "source must be content addressed");
        assert_eq!(native["references"], json!([]));
        let expected = ok(self.s.nix("nix-store").arg("--dump").arg(source)).stdout;
        let actual = ok(self
            .s
            .nix("nix-store")
            .arg("--dump")
            .arg(builder.real(source)))
        .stdout;
        assert_eq!(
            actual, expected,
            "native consumer received a different source NAR"
        );
        assert_eq!(native["narSize"].as_u64().unwrap(), actual.len() as u64);
    }
}

#[test]
fn cached_legacy_path_keeps_selected_bytes_after_tracked_and_untracked_edits_and_removal() {
    let Some(f) = Fixture::new(Some(LOCK_MTIME)) else {
        return;
    };
    f.save_expected();
    f.seed();
    let oracle = f.oracle("snapshot");
    f.assert_snapshot(&oracle);
    for cached in [false, true, true] {
        assert_eq!(f.value("snapshot", cached), oracle);
    }
    fs::write(f.input.join("tracked"), "new tracked checkout bytes\n").unwrap();
    fs::write(f.input.join("untracked"), "new private checkout bytes\n").unwrap();
    assert_ne!(hash_path(&f.s, &f.input), f.hash);
    for cached in [true, false, true] {
        assert_eq!(f.value("snapshot", cached), oracle);
    }
    fs::remove_dir_all(&f.input).unwrap();
    for cached in [true, false, true] {
        assert_eq!(f.value("snapshot", cached), oracle);
    }
    assert_eq!(f.oracle("snapshot"), oracle);
    let builder = f.assert_build(&oracle, "cached-consumer");
    f.assert_nar_parity(&builder);
}

#[test]
fn missing_locked_timestamp_matches_cold_and_cached_nix_metadata() {
    let Some(f) = Fixture::new(None) else {
        return;
    };
    f.save_expected();
    let oracle = f.oracle("snapshot");
    assert!(oracle["info"].get("lastModified").is_none());
    assert!(oracle["info"].get("lastModifiedDate").is_none());
    for cached in [false, true, true] {
        assert_eq!(f.value("snapshot", cached), oracle);
    }
}

#[test]
fn native_cold_legacy_path_includes_untracked_files_and_builds_before_nix_fetches_it() {
    for last_modified in [Some(LOCK_MTIME), None] {
        let Some(f) = Fixture::new(last_modified) else {
            return;
        };
        f.save_expected();
        assert!(!valid(&f.s, &f.source, None));
        // No oracle fetch/evaluation precedes these runs or the actual build.
        let native = f.value("snapshot", false);
        f.assert_snapshot(&native);
        for cached in [true, true, false] {
            assert_eq!(f.value("snapshot", cached), native);
        }
        let builder = f.assert_build(&native, "cold-consumer");
        let oracle = f.oracle("snapshot");
        assert_eq!(
            native, oracle,
            "source metadata and deterministic drvPath parity"
        );
        f.assert_nar_parity(&builder);
    }
}

#[test]
fn cold_legacy_path_reports_the_real_full_tree_hash_mismatch() {
    let Some(f) = Fixture::new(Some(LOCK_MTIME)) else {
        return;
    };
    fs::write(f.input.join("untracked"), "different untracked content\n").unwrap();
    let actual = hash_path(&f.s, &f.input);
    assert_ne!(actual, f.hash);
    assert_ne!(actual, f.filtered_hash);
    assert!(!valid(&f.s, &f.source, None));
    for cached in [false, true, true] {
        let out = f.assert_rejected("snapshot", cached);
        // The expected and actual hashes are the semantic mismatch, not a
        // diagnostic's punctuation, source location or wording.
        assert!(stderr(&out).contains(&f.hash), "{}", stderr(&out));
        assert!(stderr(&out).contains(&actual), "{}", stderr(&out));
    }
    let oracle =
        f.s.nix("nix")
            .args(["eval", "--json", "--no-write-lock-file"])
            .arg(f.reference("snapshot"))
            .output()
            .unwrap();
    assert!(
        !oracle.status.success(),
        "Nix accepted a genuine cold mismatch"
    );
    assert!(!valid(&f.s, &f.source, None));
}

#[test]
fn disappearing_cached_source_invalidates_warm_results_and_allows_a_matching_cold_retry() {
    let Some(f) = Fixture::new(Some(LOCK_MTIME)) else {
        return;
    };
    f.save_expected();
    f.seed();
    let oracle = f.oracle("snapshot");
    for cached in [false, true, true] {
        assert_eq!(f.value("snapshot", cached), oracle);
    }
    fs::write(
        f.input.join("untracked"),
        "current source no longer matches\n",
    )
    .unwrap();
    assert_eq!(f.value("snapshot", true), oracle);
    ok(f.s
        .nix("nix-store")
        .arg("--delete")
        .arg(oracle["drv"].as_str().unwrap())
        .arg(&f.source));
    assert!(!f.source.exists());
    assert!(!valid(&f.s, &f.source, None));
    for cached in [true, false] {
        f.assert_rejected("snapshot", cached);
    }
    write_input(&f.s, &f.input, true);
    assert_eq!(hash_path(&f.s, &f.input), f.hash);
    for cached in [true, false, true] {
        assert_eq!(f.value("snapshot", cached), oracle);
    }
}

#[test]
fn an_unregistered_mutable_hash_named_directory_is_not_a_pinned_cache_entry() {
    let Some(f) = Fixture::new(Some(LOCK_MTIME)) else {
        return;
    };
    f.save_expected();
    write_input(&f.s, &f.source, true);
    fs::write(f.source.join("tracked"), "forged store directory\n").unwrap();
    assert_ne!(hash_path(&f.s, &f.source), f.hash);
    assert!(!valid(&f.s, &f.source, None));
    let native = f.value("snapshot", false);
    f.assert_snapshot(&native);
    // Remove only this deliberately unregistered, mutable fixture before
    // asking Nix to populate the genuine immutable source as an oracle.
    fs::remove_dir_all(&f.source).unwrap();
    assert_eq!(native, f.oracle("snapshot"));
}

/// The logical store namespace is shared, but the second store's bytes and
/// registration database live below its own root. No provider is mocked.
#[test]
fn daemonless_snapshot_metadata_cannot_follow_ambient_store_selection() {
    for setting in ["NIX_REMOTE", "nix.conf", "NIX_CONFIG"] {
        let Some(f) = Fixture::new(Some(LOCK_MTIME)) else {
            return;
        };
        f.save_expected();
        let remote = f.s.builder("metadata-authority", &[]);
        let added = stdout(&ok(f
            .s
            .nix("nix")
            .args([
                "--store",
                &remote.store(),
                "store",
                "add-path",
                "--name",
                "source",
            ])
            .arg(&f.input)));
        assert_eq!(added, f.source.to_str().unwrap());
        assert_eq!(hash_path(&f.s, &remote.real(&added)), f.hash);
        assert_eq!(path_info(&f.s, &added, Some(&remote))["narHash"], f.hash);
        write_input(&f.s, &f.source, true);
        fs::write(f.source.join("tracked"), "wrong unregistered local bytes\n").unwrap();
        assert_ne!(hash_path(&f.s, &f.source), f.hash);
        assert!(!valid(&f.s, &f.source, None));

        let native = || {
            let mut cmd = f.native();
            cmd.env("IETS_NIX_BIN_DIR", f.s.dir().join("metadata-only"));
            match setting {
                "NIX_REMOTE" => {
                    cmd.env("NIX_REMOTE", remote.url());
                }
                "NIX_CONFIG" => {
                    cmd.env("NIX_CONFIG", format!("store = {}\n", remote.url()));
                }
                _ => {
                    let conf = f.s.dir().join("conf/nix.conf");
                    let text = fs::read_to_string(&conf).unwrap();
                    if !text.contains("store = ") {
                        fs::write(conf, format!("{text}store = {}\n", remote.url())).unwrap();
                    }
                }
            }
            cmd.args([
                "eval",
                "--json",
                "--pure-eval",
                "--option",
                "store",
                "local",
            ]);
            cmd
        };
        for cached in [false, true, true] {
            let mut cmd = native();
            if !cached {
                cmd.arg("--no-cache");
            }
            let value = parse(&ok(cmd.arg(f.reference("snapshot"))));
            f.assert_snapshot(&value);
        }
        // Absence in the selected local database must still take the cold
        // authorized path, including a real mismatch rather than remote reuse.
        fs::write(f.input.join("untracked"), "authorized source changed\n").unwrap();
        let actual = hash_path(&f.s, &f.input);
        assert_ne!(actual, f.hash);
        let out = native()
            .arg("--no-cache")
            .arg(f.reference("snapshot"))
            .output()
            .unwrap();
        assert!(!out.status.success(), "{}", stdout(&out));
        assert!(stderr(&out).contains(&f.hash), "{}", stderr(&out));
        assert!(stderr(&out).contains(&actual), "{}", stderr(&out));
    }
}

#[test]
fn daemonless_local_metadata_keeps_registered_bytes_and_reports_permission_errors() {
    let Some(f) = Fixture::new(Some(LOCK_MTIME)) else {
        return;
    };
    f.save_expected();
    f.seed();
    let expected = f.oracle("snapshot");
    fs::write(f.input.join("tracked"), "changed authorized checkout\n").unwrap();
    let native = || {
        let mut cmd = f.native();
        cmd.env("IETS_NIX_BIN_DIR", f.s.dir().join("metadata-only"))
            .args([
                "eval",
                "--json",
                "--pure-eval",
                "--no-cache",
                "--option",
                "store",
                "local",
            ])
            .arg(f.reference("snapshot"));
        cmd
    };
    assert_eq!(parse(&ok(&mut native())), expected);
    for path in [f.s.dir().join("var/db"), f.s.store_dir()] {
        let permissions = fs::metadata(&path).unwrap().permissions();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
        let out = native().output().unwrap();
        fs::set_permissions(&path, permissions).unwrap();
        assert!(
            !out.status.success(),
            "permission failure returned {}",
            stdout(&out)
        );
        assert!(out.stdout.is_empty());
        assert_eq!(parse(&ok(&mut native())), expected);
    }
}

#[test]
fn relocated_metadata_never_authenticates_the_logical_store_directory() {
    let Some(f) = Fixture::new(Some(LOCK_MTIME)) else {
        return;
    };
    f.save_expected();
    let remote = f.s.builder("relocated-metadata", &[]);
    let added = stdout(&ok(f
        .s
        .nix("nix")
        .args([
            "--store",
            &remote.store(),
            "store",
            "add-path",
            "--name",
            "source",
        ])
        .arg(&f.input)));
    assert_eq!(added, f.source.to_str().unwrap());
    assert_eq!(hash_path(&f.s, &remote.real(&added)), f.hash);
    write_input(&f.s, &f.source, true);
    fs::write(f.source.join("tracked"), "wrong logical namespace bytes\n").unwrap();
    assert_ne!(hash_path(&f.s, &f.source), f.hash);
    let out = ok(f
        .native()
        .env("IETS_NIX_BIN_DIR", f.s.dir().join("metadata-only"))
        .env("NIX_REMOTE", remote.store())
        .args([
            "eval",
            "--json",
            "--pure-eval",
            "--no-cache",
            "--option",
            "store",
            &remote.store(),
        ])
        .arg(f.reference("snapshot")));
    f.assert_snapshot(&parse(&out));
}

#[test]
fn unix_metadata_cannot_authenticate_another_filesystem_namespace_through_any_selection_route() {
    for route in [
        "explicit-uri",
        "NIX_REMOTE",
        "nix.conf",
        "NIX_CONFIG",
        "socket-daemon",
        "socket-auto",
        "socket-unix",
        "socket-empty",
        "state-default-socket",
    ] {
        let Some(f) = Fixture::new(Some(LOCK_MTIME)) else {
            return;
        };
        f.save_expected();
        let remote = f.s.builder("preferred-metadata-authority", &[]);
        let added = stdout(&ok(f
            .s
            .nix("nix")
            .args(["--store", &remote.store(), "store", "add-path", "--name", "source"])
            .arg(&f.input)));
        assert_eq!(added, f.source.to_str().unwrap());
        let registered = path_info(&f.s, &added, Some(&remote));
        assert_eq!(registered["narHash"], f.hash);
        assert_eq!(hash_path(&f.s, &remote.real(&added)), f.hash);
        write_input(&f.s, &f.source, true);
        fs::write(f.source.join("tracked"), "wrong unregistered local bytes\n").unwrap();
        assert_ne!(hash_path(&f.s, &f.source), f.hash);
        assert!(!valid(&f.s, &f.source, None));
        if route == "nix.conf" {
            let conf = f.s.dir().join("conf/nix.conf");
            let text = fs::read_to_string(&conf).unwrap();
            fs::write(conf, format!("{text}store = {}\n", remote.url())).unwrap();
        }
        if route == "state-default-socket" {
            let sockets = f.s.dir().join("var/daemon-socket");
            fs::create_dir_all(&sockets).unwrap();
            symlink(remote.socket(), sockets.join("socket")).unwrap();
        }
        let native = |cached| {
            let mut cmd = f.native();
            cmd.args(["eval", "--json", "--pure-eval"]);
            if !cached {
                cmd.args(["--no-cache", "--no-tracing-cache"]);
            }
            match route {
                "explicit-uri" => { cmd.args(["--option", "store", &remote.url()]); }
                "NIX_REMOTE" => { cmd.env("NIX_REMOTE", remote.url()); }
                "NIX_CONFIG" => { cmd.env("NIX_CONFIG", format!("store = {}\n", remote.url())); }
                "nix.conf" => {}
                "state-default-socket" => { cmd.env_remove("NIX_DAEMON_SOCKET_PATH"); }
                _ => {
                    cmd.env("NIX_DAEMON_SOCKET_PATH", remote.socket());
                    let selection = match route {
                        "socket-auto" => "auto",
                        "socket-unix" => "unix://",
                        "socket-empty" => "",
                        _ => "daemon",
                    };
                    cmd.args(["--option", "store", selection]);
                }
            }
            cmd.arg(f.reference("snapshot"));
            cmd
        };
        for cached in [false, true, true] {
            let value = parse(&ok(&mut native(cached)));
            f.assert_metadata_only();
            f.assert_snapshot(&value);
            if route == "explicit-uri" && !cached {
                // Transfer the selected origin, not just its claimed hash.
                let builder = f.assert_build(&value, "namespace-source-consumer");
                let uploaded = path_info(&f.s, &added, Some(&builder));
                assert_eq!(source_identity(&uploaded), source_identity(&registered));
                let expected = ok(f.s.nix("nix-store").arg("--dump").arg(remote.real(&added))).stdout;
                let actual = ok(f.s.nix("nix-store").arg("--dump").arg(builder.real(&added))).stdout;
                assert_eq!(actual, expected, "selected native source NAR");
            }
        }
        fs::write(f.input.join("untracked"), "authorized source changed\n").unwrap();
        let actual = hash_path(&f.s, &f.input);
        assert_ne!(actual, f.hash);
        let out = native(false).output().unwrap();
        f.assert_metadata_only();
        assert!(!out.status.success(), "{route}: {}", stdout(&out));
        assert!(out.stdout.is_empty());
        assert!(stderr(&out).contains(&f.hash), "{}", stderr(&out));
        assert!(stderr(&out).contains(&actual), "{}", stderr(&out));

        // A genuinely registered local snapshot is still authoritative even
        // when the selected Unix daemon owns another physical store.
        write_input(&f.s, &f.input, true);
        fs::remove_dir_all(&f.source).unwrap();
        f.seed();
        fs::write(f.input.join("tracked"), "changed original after local registration\n").unwrap();
        for cached in [false, true, true] {
            f.assert_snapshot(&parse(&ok(&mut native(cached))));
            f.assert_metadata_only();
        }
    }
}

#[test]
fn unavailable_unix_provider_does_not_prevent_authorized_native_cold_ingestion() {
    let Some(f) = Fixture::new(Some(LOCK_MTIME)) else {
        return;
    };
    f.save_expected();
    write_input(&f.s, &f.source, true);
    fs::write(f.source.join("tracked"), "wrong unregistered local bytes\n").unwrap();
    for cached in [false, true, true] {
        let mut cmd = f.native();
        cmd.env("NIX_DAEMON_SOCKET_PATH", f.s.dir().join("absent-socket"))
            .args(["eval", "--json", "--pure-eval"]);
        if !cached {
            cmd.args(["--no-cache", "--no-tracing-cache"]);
        }
        f.assert_snapshot(&parse(&ok(cmd.arg(f.reference("snapshot")))));
        f.assert_metadata_only();
    }
}

#[test]
fn seeded_cache_does_not_authorize_outside_prefix_or_symlink_escape_inputs() {
    let Some(f) = Fixture::new(Some(LOCK_MTIME)) else {
        return;
    };
    f.seed();
    let outside = f.s.dir().join("outside");
    let prefix = f.s.dir().join("repo-other");
    for path in [&outside, &prefix] {
        write_input(&f.s, path, true);
        assert_eq!(hash_path(&f.s, path), f.hash);
    }
    let escape = f.repo.join("escape");
    symlink(&outside, &escape).unwrap();
    for input in [&outside, &prefix, &escape] {
        f.configure_input(input);
        assert!(valid(&f.s, &f.source, None));
        for cached in [false, true, true] {
            f.assert_rejected("snapshot", cached);
        }
    }
}

#[test]
fn warm_trace_rechecks_original_location_symlink_authority_before_pinned_cache_use() {
    let Some(f) = Fixture::new(Some(LOCK_MTIME)) else {
        return;
    };
    f.save_expected();
    f.seed();
    // This alias is intentionally untracked: changing only its target must
    // not change the parent Git source and mask a missing authorization trace.
    let alias = f.repo.join("alias");
    symlink(&f.input, &alias).unwrap();
    f.configure_input(&alias);
    let root = f.value("rootSource", false);
    let initial = f.value("snapshot", false);
    f.assert_snapshot(&initial);
    for _ in 0..2 {
        assert_eq!(f.value("snapshot", true), initial);
    }
    let outside = f.s.dir().join("outside");
    write_input(&f.s, &outside, true);
    assert_eq!(hash_path(&f.s, &outside), f.hash);
    fs::remove_file(&alias).unwrap();
    symlink(&outside, &alias).unwrap();
    assert_eq!(f.value("rootSource", false), root);
    assert!(valid(&f.s, &f.source, None));
    for cached in [true, false, true] {
        f.assert_rejected("snapshot", cached);
    }
    fs::remove_file(&alias).unwrap();
    symlink(&f.input, &alias).unwrap();
    assert_eq!(f.value("snapshot", true), initial);
}

#[test]
fn declared_source_authority_does_not_widen_public_reads_path_or_fetch_tree() {
    let Some(f) = Fixture::new(Some(LOCK_MTIME)) else {
        return;
    };
    f.save_expected();
    f.seed();
    let selected = f.value("snapshot", false);
    f.assert_snapshot(&selected);
    for cached in [false, true, true] {
        // Ordinary absolute reads do not gain the private hook's checkout
        // authority, even for tracked bytes selected by the declared input.
        f.assert_rejected("readTracked", cached);
        f.assert_rejected("publicPath", cached);
        let public = f.value("publicFilteredFetch", cached);
        assert_eq!(public["tracked"], selected["tracked"]);
        assert_eq!(
            public["hidden"], false,
            "public fetch exposed checkout-only data"
        );
        for attr in ["readUntracked", "publicFullFetch"] {
            f.assert_rejected(attr, cached);
        }
    }
}

#[test]
fn unpinned_relative_parent_inputs_keep_the_filtered_live_parent_source() {
    let Some(f) = Fixture::new(Some(LOCK_MTIME)) else {
        return;
    };
    let lock: Value =
        serde_json::from_slice(&fs::read(f.repo.join("flake.lock")).unwrap()).unwrap();
    assert!(lock["nodes"]["rel"]["locked"].get("narHash").is_none());
    for expected in ["relative one\n", "relative two\n"] {
        fs::write(f.repo.join("rel/tracked"), expected).unwrap();
        let native = f.value("relative", true);
        assert_eq!(native["data"], expected);
        assert_eq!(native["hidden"], false);
        assert_eq!(native["info"], native["rootInfo"]);
        assert_eq!(f.value("relative", false), native);
        assert_eq!(f.value("relative", true), native);
        assert_eq!(native, f.oracle("relative"));
    }
}
