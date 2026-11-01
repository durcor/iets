//! Pure checkout URLs read the final root source, even before it exists in
//! the store, without exposing checkout-only files through the fetch cache.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::Output;

use e2e::{Store, git, stderr, stdout};
use serde_json::Value;

struct Fixture {
    store: Store,
    repo: PathBuf,
    flake: PathBuf,
    reference: String,
}

impl Fixture {
    fn new(setting: Option<bool>, subdir: bool, committed: bool) -> Option<Self> {
        let store = Store::start_with(&[])?;
        let repo = store.dir().join("repo");
        let flake = if subdir {
            repo.join("nested")
        } else {
            repo.clone()
        };
        fs::create_dir_all(&flake).unwrap();
        let setting = setting
            .map(|enabled| format!("inputs.self.submodules = {enabled};"))
            .unwrap_or_default();
        store.write(
            flake.join("flake.nix").to_str().unwrap(),
            &r#"{
  @SETTING@
  outputs = { self }: let
    fetch = path: hash: builtins.readFile (builtins.fetchurl {
      url = "file://${path}";
      sha256 = hash;
    });
    hash = builtins.hashString "sha256" "in";
    metadata = {
      data = builtins.readFile "${self}/in.txt";
      info = removeAttrs self.sourceInfo [ "outPath" ];
      source = self.sourceInfo.outPath;
    };
  in {
    inherit metadata;
    snapshot = metadata // {
      inside = fetch "@FLAKE@/in.txt" (builtins.hashString "sha256" metadata.data);
      sibling = fetch "@REPO@/root.txt" hash;
    };
    untracked = fetch "@FLAKE@/private/in.txt" hash;
    outside = fetch "@OUTSIDE@/in.txt" hash;
    prefix = fetch "@REPO@-other/in.txt" hash;
  };
}
"#
            .replace("@SETTING@", &setting)
            .replace("@FLAKE@", flake.to_str().unwrap())
            .replace("@REPO@", repo.to_str().unwrap())
            .replace("@OUTSIDE@", store.dir().join("outside").to_str().unwrap()),
        );
        fs::write(flake.join("in.txt"), "in").unwrap();
        fs::write(repo.join("root.txt"), "in").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "-A"]);
        if committed {
            git(&repo, &["commit", "-qm", "root fetch fixture"]);
        }
        // Every rejected URL shares a basename and hash with an allowed
        // fetch. A cached flat file must not bypass source confinement.
        fs::create_dir_all(flake.join("private")).unwrap();
        fs::write(flake.join("private/in.txt"), "in").unwrap();
        store.write("outside/in.txt", "in");
        store.write("repo-other/in.txt", "in");
        let git_bin = store.dir().join("git-only");
        fs::create_dir(&git_bin).unwrap();
        symlink(e2e::resolved("git"), git_bin.join("git")).unwrap();
        let reference = flake.display().to_string();
        Some(Self {
            store,
            repo,
            flake,
            reference,
        })
    }

    fn pin(&mut self) {
        let rev = stdout(&git(&self.repo, &["rev-parse", "HEAD"]));
        let dir = self.flake.strip_prefix(&self.repo).unwrap();
        self.reference = format!(
            "git+file://{}?rev={rev}&dir={}",
            self.repo.display(),
            dir.display()
        );
    }

    fn iets(&self, attr: &str, cached: bool) -> Output {
        let mut cmd = self.store.iets();
        cmd.env("IETS_CACHE_DIR", self.store.dir().join("fetch-cache"))
            .env("PATH", self.store.dir().join("git-only"))
            .env("IETS_NIX_BIN_DIR", self.store.no_nix())
            .env("IETS_PROVENANCE", "0")
            .args(["eval", "--json", "--pure-eval", "--timings"]);
        if !cached {
            cmd.args(["--no-cache", "--no-tracing-cache"]);
        }
        cmd.arg(format!("{}#{attr}", self.reference))
            .output()
            .unwrap()
    }

    fn snapshot(&self, expected: &str) -> Value {
        let mut last = Value::Null;
        for (run, cached) in [false, true, true].into_iter().enumerate() {
            let out = self.iets("snapshot", cached);
            assert!(out.status.success(), "{}", stderr(&out));
            let value: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(value["data"], expected);
            assert_eq!(value["inside"], expected);
            assert_eq!(value["sibling"], "in");
            let source = value["source"].as_str().unwrap();
            assert!(
                !PathBuf::from(source).exists(),
                "native fetch materialized the root source: {source}"
            );
            if run == 2 {
                assert!(stderr(&out).contains("cache: hit"), "{}", stderr(&out));
            }
            if !last.is_null() {
                assert_eq!(value, last, "cached and uncached source reads differ");
            }
            last = value;
        }
        last
    }

    fn rejects_checkout_only_paths(&self) {
        for attr in ["untracked", "outside", "prefix"] {
            for cached in [false, true] {
                let out = self.iets(attr, cached);
                assert!(!out.status.success(), "native fetch exposed {attr}");
                assert!(out.stdout.is_empty(), "native fetch returned {attr}");
                if attr != "untracked" {
                    assert!(
                        stderr(&out).contains("forbidden in pure evaluation mode"),
                        "{attr}: {}",
                        stderr(&out)
                    );
                }
            }
        }
    }

    fn metadata_parity(&self, snapshot: &Value) {
        // Only the oracle may now materialize the source. Its metadata reads
        // the source itself, not the mutable checkout URL under test.
        let out = self
            .store
            .nix("nix")
            .args(["eval", "--json", "--option", "eval-cache", "false"])
            .arg(format!("{}#metadata", self.reference))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        for field in ["data", "info", "source"] {
            assert_eq!(snapshot[field], value[field], "source {field} parity");
        }
    }
}

#[test]
fn checkout_fetches_use_final_unmaterialized_clean_and_pinned_roots() {
    for (setting, pinned, subdir) in [
        (None, false, false),
        (Some(false), false, false),
        (Some(true), false, false),
        (Some(false), false, true),
        (Some(true), false, true),
        (None, true, true),
        (Some(false), true, true),
        (Some(true), true, true),
    ] {
        let Some(mut f) = Fixture::new(setting, subdir, true) else {
            return;
        };
        if pinned {
            f.pin();
        }
        let clean = f.snapshot("in");
        assert!(clean["info"]["rev"].is_string());
        assert_eq!(clean["info"]["revCount"], 1);
        f.rejects_checkout_only_paths();

        // Git still calls this checkout clean. Explicit self settings and
        // pinned references must fetch committed bytes, not this host edit;
        // an ordinary root retains its mutable working-tree semantics.
        let file = f.flake.join("in.txt");
        let rel = file.strip_prefix(&f.repo).unwrap().to_str().unwrap();
        git(&f.repo, &["update-index", "--assume-unchanged", rel]);
        fs::write(&file, "hidden checkout edit").unwrap();
        let committed = pinned || setting.is_some();
        let hidden = f.snapshot(if committed {
            "in"
        } else {
            "hidden checkout edit"
        });
        assert_eq!(hidden["info"]["rev"], clean["info"]["rev"]);
        if committed {
            assert_eq!(hidden, clean);
        } else {
            assert_ne!(hidden["source"], clean["source"]);
        }

        git(&f.repo, &["update-index", "--no-assume-unchanged", rel]);
        let dirty = f.snapshot(if pinned { "in" } else { "hidden checkout edit" });
        if pinned {
            assert_eq!(dirty, clean);
        } else {
            assert!(dirty["info"].get("rev").is_none());
            assert!(dirty["info"].get("revCount").is_none());
            assert_eq!(
                dirty["info"]["dirtyRev"],
                format!("{}-dirty", clean["info"]["rev"].as_str().unwrap())
            );
        }
        // A second mutation while already dirty must invalidate cached reads,
        // while a pinned root remains independent of mutable checkout bytes.
        fs::write(&file, "second checkout edit").unwrap();
        let changed = f.snapshot(if pinned { "in" } else { "second checkout edit" });
        if pinned {
            assert_eq!(changed, clean);
        } else {
            assert_ne!(changed["source"], dirty["source"]);
            assert_eq!(changed["info"]["dirtyRev"], dirty["info"]["dirtyRev"]);
        }
        f.rejects_checkout_only_paths();
        f.metadata_parity(&changed);
    }
}

#[test]
fn checkout_fetches_follow_no_head_roots_through_the_first_commit() {
    for setting in [None, Some(false), Some(true)] {
        let Some(f) = Fixture::new(setting, true, false) else {
            return;
        };
        let initial = f.snapshot("in");
        assert!(initial["info"].get("rev").is_none());
        assert!(initial["info"].get("dirtyRev").is_none());
        assert!(initial["info"].get("revCount").is_none());
        assert_eq!(initial["info"]["lastModified"], 0);
        assert_eq!(initial["info"]["submodules"], setting.unwrap_or(false));
        f.rejects_checkout_only_paths();

        fs::write(f.flake.join("in.txt"), "first checkout edit").unwrap();
        let edited = f.snapshot("first checkout edit");
        assert_ne!(edited["source"], initial["source"]);
        assert!(edited["info"].get("rev").is_none());
        assert!(edited["info"].get("dirtyRev").is_none());
        assert!(edited["info"].get("revCount").is_none());
        assert_eq!(edited["info"]["lastModified"], 0);

        // Creating HEAD changes metadata even when the source bytes and NAR
        // identity are unchanged. Keep the checkout-only files out of the index.
        git(&f.repo, &["add", "nested/in.txt"]);
        git(&f.repo, &["commit", "-qm", "first root revision"]);
        let committed = f.snapshot("first checkout edit");
        assert_eq!(committed["source"], edited["source"]);
        assert!(committed["info"]["rev"].is_string());
        assert!(committed["info"].get("dirtyRev").is_none());
        assert_eq!(committed["info"]["revCount"], 1);
        assert!(committed["info"]["lastModified"].as_u64().unwrap() > 0);
        assert_eq!(committed["info"]["submodules"], setting.unwrap_or(false));
        f.rejects_checkout_only_paths();
        f.metadata_parity(&committed);
    }
}
