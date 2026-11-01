//! The `grpc://` transport against a nix-grpc-daemon in plaintext standalone
//! mode: no TLS, no niks3, the node schedules onto itself. The daemon fronts
//! a chroot builder store, so this is Linux only.
//!
//! Needs `IETS_TEST_NIX_GRPC_DAEMON`, the nix-grpc-daemon binary.
#![cfg(target_os = "linux")]

mod e2e;
mod hermetic;

use std::env;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use e2e::{Builder, Store, checks_flake, ok, stderr, stdout};
use serde_json::Value;

/// `b` drops before `s`, whose dir it runs in.
struct Grpc {
    b: Builder,
    s: Store,
    url: String,
    log: PathBuf,
    daemon: Child,
}

impl Drop for Grpc {
    fn drop(&mut self) {
        // SAFETY: kill only sends a signal
        unsafe { libc::kill(-(self.daemon.id() as i32), libc::SIGKILL) };
        let _ = self.daemon.wait();
    }
}

impl Grpc {
    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn build(&self, args: &[&str]) -> Output {
        self.s
            .iets()
            .args(["build", "--builder", &self.url, "--no-tracing-cache"])
            .args(args)
            .output()
            .unwrap()
    }
}

fn start() -> Option<Grpc> {
    let Ok(bin) = env::var("IETS_TEST_NIX_GRPC_DAEMON") else {
        eprintln!("skipped: IETS_TEST_NIX_GRPC_DAEMON unset");
        return None;
    };
    let s = Store::start_with(&[])?;
    let b = s.builder("b", &[("max-jobs", "4")]);
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let log = s.dir().join("grpc.log");
    let f = fs::File::create(&log).unwrap();
    // --proxy-store is what the native RPCs read and write: the chroot store
    // itself, since through the socket a LocalFSStore resolves real paths
    // under the store dir
    let mut c = s.nix(&bin);
    c.args(["--listen", &format!("127.0.0.1:{port}")])
        .arg("--proxy-socket")
        .arg(b.socket())
        .args(["--proxy-store", &b.store()])
        .stdin(Stdio::null())
        .stdout(f.try_clone().unwrap())
        .stderr(f)
        .process_group(0);
    let daemon = c.spawn().unwrap();
    let mut g = Grpc {
        url: format!("grpc://127.0.0.1:{port}?insecure=1"),
        s,
        b,
        log,
        daemon,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        let exited = g.daemon.try_wait().unwrap().is_some();
        assert!(
            !exited && Instant::now() < deadline,
            "nix-grpc-daemon did not start: {}",
            g.log()
        );
        thread::sleep(Duration::from_millis(20));
    }
    Some(g)
}

#[test]
fn a_source_tree_and_a_closure() {
    let Some(g) = start() else { return };
    let s = &g.s;

    // built on the daemon, drv and source deleted here first
    let case = e2e::make_case(s, "c1", "iets-grpc-e2e");
    let drv = stdout(&ok(s.nix("nix-instantiate").arg(&case)));
    let src = s.nix_store(&["-q", "--references", &drv]);
    let expected = s.nix_store(&["-q", "--outputs", &drv]);
    let drv_bytes = fs::read(&drv).unwrap();
    ok(s.nix("nix-store").args(["--delete", &drv, &src]));
    let fetch = format!("{}/c1/fetch", s.dir().display());
    let o = g.build(&["--timings", "--fetch-to", &fetch, &case]);
    assert!(o.status.success(), "{}\n{}", stderr(&o), g.log());
    assert_eq!(stdout(&o), expected);
    let store = g.b.store();
    ok(s.nix("nix-store")
        .args(["--store", &store, "--check-validity", &drv, &src]));
    assert_eq!(fs::read(g.b.real(&drv)).unwrap(), drv_bytes);
    let mut want = fs::read(s.dir().join("c1/src/a")).unwrap();
    want.extend(fs::read(s.dir().join("c1/src/sub/b")).unwrap());
    let base = expected.rsplit('/').next().unwrap();
    assert_eq!(fs::read(format!("{fetch}/{base}")).unwrap(), want);

    // the chain cold, --no-build, then warm
    let chain = s.pkgs().join("chain.nix");
    let chain = chain.to_str().unwrap();
    let cdrv = stdout(&ok(s.nix("nix-instantiate").arg(chain)));
    let o = g.build(&["--timings", "--no-build", chain]);
    let err = stderr(&o);
    assert!(o.status.success(), "{err}");
    let closure = s.nix_store(&["-qR", &cdrv]).lines().count();
    assert!(
        err.contains(&format!("query: {closure} paths, {closure} missing")),
        "{err}"
    );
    assert_eq!(
        stdout(&ok(s
            .nix("nix-store")
            .args(["--store", &store, "-q", "--hash", &cdrv]))),
        s.nix_store(&["-q", "--hash", &cdrv])
    );
    assert_eq!(fs::read(g.b.real(&cdrv)).unwrap(), fs::read(&cdrv).unwrap());
    let o = g.build(&["--timings", "--no-build", chain]);
    assert!(stderr(&o).contains(", 0 missing"), "{}", stderr(&o));
}

#[test]
fn a_local_ia_input_source_keeps_its_runtime_closure() {
    let Some(g) = start() else { return };
    let s = &g.s;
    let source_drv = stdout(&ok(s
        .nix("nix-instantiate")
        .arg(s.pkgs().join("chain.nix"))));
    let input = s.nix_store(&["-r", &source_drv]);
    let closure = s.nix_store(&["-qR", &input]);
    let sources: Vec<_> = closure.lines().collect();
    assert_eq!(sources.len(), 4);
    let metadata: Value = serde_json::from_slice(
        &ok(s.nix("nix").args(["path-info", "--json"]).args(&sources)).stdout,
    )
    .unwrap();
    for path in &sources {
        assert!(metadata[path]["ca"].is_null());
        assert!(
            !s.nix("nix-store")
                .args(["--store", &g.b.store(), "--check-validity", path])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let case = s.write("stored-source-consumer.nix", &format!(r#"derivation {{
  name = "grpc-stored-source-consumer";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" ''while read -r line; do printf '%s\n' "$line"; done < ${{builtins.storePath "{input}"}}/n > "$out"'' ];
}}
"#));
    let drv = stdout(&ok(s.nix("nix-instantiate").arg(&case)));
    assert_eq!(s.nix_store(&["-q", "--references", &drv]), input);
    let expected = s.nix_store(&["-q", "--outputs", &drv]);
    let result = g.build(&["--impure", case.to_str().unwrap()]);
    assert!(result.status.success(), "{}\n{}", stderr(&result), g.log());
    assert_eq!(stdout(&result), expected);
    assert_eq!(fs::read(g.b.real(&expected)).unwrap(), b"3\n");
    let copied: Value = serde_json::from_slice(
        &ok(s
            .nix("nix")
            .args(["path-info", "--json", "--store", &g.b.store()])
            .args(&sources))
        .stdout,
    )
    .unwrap();
    for path in sources {
        for key in [
            "ca",
            "narHash",
            "narSize",
            "references",
            "deriver",
            "signatures",
        ] {
            assert_eq!(copied[path][key], metadata[path][key], "{path}: {key}");
        }
        assert_ne!(copied[path]["ultimate"], true);
        assert_eq!(
            fs::read(g.b.real(path).join("n")).unwrap(),
            fs::read(format!("{path}/n")).unwrap()
        );
        let producer = metadata[path]["deriver"].as_str().unwrap();
        assert!(
            !s.nix("nix-store")
                .args(["--store", &g.b.store(), "--check-validity", producer])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    assert!(g.log().contains("method=AddMultipleToStore"));
}

/// iets under the name `nix`, as the classic package installs it.
fn farm(s: &Store) -> PathBuf {
    let bin = s.dir().join("farm");
    fs::create_dir(&bin).unwrap();
    symlink(env!("CARGO_BIN_EXE_iets"), bin.join("nix")).unwrap();
    bin.join("nix")
}

/// Classic `nix build --store grpc://…` prints what Nix prints for the same
/// store over the worker protocol, and an out-link fetches the output back.
#[test]
fn classic_nix_build() {
    let Some(g) = start() else { return };
    let s = &g.s;
    let nix = farm(s);
    let case = e2e::make_case(s, "c4", "iets-grpc-e2e-classic");
    let tail = [
        "--eval-store",
        "auto",
        "--no-link",
        "--print-out-paths",
        "-f",
        &case,
    ];
    let ours = s
        .iets_as(&nix)
        .env("IETS_NIX_FALLBACK", "never")
        .args(["build", "--store", &g.url])
        .args(tail)
        .output()
        .unwrap();
    assert!(ours.status.success(), "{}\n{}", stderr(&ours), g.log());
    let reference = ok(s
        .nix("nix")
        .args(["build", "--store", &g.b.url()])
        .args(tail));
    assert_eq!(ours.stdout, reference.stdout);

    // A scratch daemon builds unsigned, and registration checks signatures
    // as `nix copy --from` does: a refusal after the fetch is the answer here
    let case = e2e::make_case(s, "c5", "iets-grpc-e2e-link");
    let link = s.dir().join("c5/result");
    let r = s
        .iets_as(&nix)
        .env("IETS_NIX_FALLBACK", "never")
        .args(["build", "--store", &g.url, "--eval-store", "auto", "-o"])
        .arg(&link)
        .args(["--print-out-paths", "-f", &case])
        .output()
        .unwrap();
    let err = stderr(&r);
    if r.status.success() {
        let out = stdout(&r);
        assert_eq!(fs::read_link(&link).unwrap().to_str().unwrap(), out);
        ok(s.nix("nix-store").args(["--check-validity", &out]));
    } else {
        assert!(
            err.contains("lacks a signature by a trusted key"),
            "{err}\n{}",
            g.log()
        );
        assert!(g.log().contains("method=FetchNars"), "never fetched back");
        assert!(err.contains("trusted-public-keys"), "no hint: {err}");
    }
}

/// A set of checks gets the same verdicts over grpc as over the worker
/// protocol on the same store.
#[test]
fn the_same_verdicts_as_the_worker_protocol() {
    let Some(g) = start() else { return };
    let s = &g.s;
    // two flakes, so neither run finds the other's outputs built
    let verdicts = |url: &str, dir: &str| -> (Option<i32>, String, Vec<(String, bool)>) {
        let flake = checks_flake(s, dir);
        let o = s
            .iets()
            .args(["build", "--builder", url, "--no-tracing-cache", "--json"])
            .arg(format!("{flake}#checks"))
            .output()
            .unwrap();
        let j: Value =
            serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("{e}: {}", stderr(&o)));
        let mut rows: Vec<(String, bool)> = j["outcomes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["attr"].as_str().unwrap().into(),
                    r["success"].as_bool().unwrap(),
                )
            })
            .collect();
        rows.sort();
        (o.status.code(), stderr(&o), rows)
    };
    let (gc, gerr, grows) = verdicts(&g.url, "fb-grpc");
    let (wc, _, wrows) = verdicts(&g.b.url(), "fb-worker");
    assert_eq!(gc, wc);
    assert_eq!(grows, wrows);
    assert!(gerr.contains(&format!("{}.bad", e2e::system())), "{gerr}");
    assert!(gerr.contains("about to fail"), "{gerr}");
    assert!(gerr.contains("building fb-chain"), "{gerr}");
}

#[test]
fn a_multi_root_build_names_every_attribute() {
    let Some(g) = start() else { return };
    let s = &g.s;
    let a = e2e::make_case(s, "c7a", "iets-grpc-e2e-multi-a");
    let b = e2e::make_case(s, "c7b", "iets-grpc-e2e-multi-b");
    let o = g.build(&["--impure", "-E", &format!("[ (import {a}) (import {b}) ]")]);
    assert!(o.status.success(), "{}\n{}", stderr(&o), g.log());
    for n in ["iets-grpc-e2e-multi-a", "iets-grpc-e2e-multi-b"] {
        assert!(
            stderr(&o).contains(&format!("building {n}")),
            "{}",
            stderr(&o)
        );
    }
    assert_eq!(stdout(&o).lines().count(), 2);
}
