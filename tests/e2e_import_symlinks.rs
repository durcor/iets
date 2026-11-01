//! Imports use the symlink target's virtual source path without exposing
//! checkout entries that Git left out of the flake.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::Output;

use e2e::{Store, git, stderr, stdout};

struct Fixture {
    store: Store,
    repo: PathBuf,
}

impl Fixture {
    fn new() -> Option<Self> {
        let store = Store::start_with(&[])?;
        let repo = store.dir().join("repo");
        fs::create_dir_all(repo.join(".config/nixos")).unwrap();
        fs::create_dir_all(repo.join(".config/modules")).unwrap();
        fs::create_dir_all(repo.join("private-default")).unwrap();
        fs::create_dir_all(repo.join("aliases")).unwrap();
        fs::write(
            repo.join("flake.nix"),
            r#"{
  outputs = { self }: {
    ancestor = import "${self}/etc/nixos/configuration.nix";
    directory = import "${self}/etc/nixos";
    leaf = import "${self}/aliases/module.nix";
    scoped = builtins.scopedImport { suffix = "-scoped"; } "${self}/aliases/scoped.nix";
    identity = (import "${self}/aliases/identity.nix") == "${self}/.config/nixos";
    untracked = import "${self}/untracked-link.nix";
    untrackedDirectory = import "${self}/untracked-dir/module.nix";
    untrackedDefault = import "${self}/private-default";
    hiddenLink = import "${self}/hidden-link.nix";
    escape = import "${self}/escape/module.nix";
    absolute = import "${self}/absolute.nix";
    cycle = import "${self}/cycle-a.nix";
    danglingExists = builtins.pathExists "${self}/dangling.nix";
    danglingImport = import "${self}/dangling.nix";
  };
}
"#,
        )
        .unwrap();
        fs::write(
            repo.join("flake.lock"),
            r#"{"nodes":{"root":{}},"root":"root","version":7}"#,
        )
        .unwrap();
        fs::write(
            repo.join(".config/nixos/configuration.nix"),
            "import ./value.nix\n",
        )
        .unwrap();
        fs::write(
            repo.join(".config/modules/default.nix"),
            "import ./value.nix\n",
        )
        .unwrap();
        fs::write(
            repo.join(".config/modules/value.nix"),
            "\"directory-target\"\n",
        )
        .unwrap();
        symlink(
            "../modules/default.nix",
            repo.join(".config/nixos/default.nix"),
        )
        .unwrap();
        symlink("../private.nix", repo.join("private-default/default.nix")).unwrap();
        fs::write(repo.join(".config/nixos/value.nix"), "\"target-one\"\n").unwrap();
        fs::write(
            repo.join(".config/nixos/scoped.nix"),
            "(import ./value.nix) + suffix\n",
        )
        .unwrap();
        fs::write(
            repo.join(".config/nixos/identity.nix"),
            "builtins.toString ./.\n",
        )
        .unwrap();
        // These decoys make a wrong relative import base observable even if
        // importing the link itself succeeds.
        fs::write(repo.join("aliases/value.nix"), "\"wrong-base\"\n").unwrap();
        symlink(".config", repo.join("etc")).unwrap();
        symlink(
            "../.config/nixos/configuration.nix",
            repo.join("aliases/module.nix"),
        )
        .unwrap();
        symlink(
            "../.config/nixos/scoped.nix",
            repo.join("aliases/scoped.nix"),
        )
        .unwrap();
        symlink(
            "../.config/nixos/identity.nix",
            repo.join("aliases/identity.nix"),
        )
        .unwrap();
        symlink("private.nix", repo.join("untracked-link.nix")).unwrap();
        symlink("private-dir", repo.join("untracked-dir")).unwrap();
        symlink("../outside", repo.join("escape")).unwrap();
        let outside = store.write("outside/module.nix", "\"host-secret\"\n");
        symlink(&outside, repo.join("absolute.nix")).unwrap();
        symlink("cycle-b.nix", repo.join("cycle-a.nix")).unwrap();
        symlink("cycle-a.nix", repo.join("cycle-b.nix")).unwrap();
        symlink("absent.nix", repo.join("dangling.nix")).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "tracked symlinks"]);
        // Existing host targets must stay invisible, including an untracked
        // link that would otherwise lead back to a tracked file.
        fs::write(repo.join("private.nix"), "\"host-secret\"\n").unwrap();
        fs::create_dir(repo.join("private-dir")).unwrap();
        fs::write(repo.join("private-dir/module.nix"), "\"host-secret\"\n").unwrap();
        symlink(".config/nixos/value.nix", repo.join("hidden-link.nix")).unwrap();
        Some(Self { store, repo })
    }

    fn iets(&self, attr: &str, cached: bool) -> Output {
        let mut cmd = self.store.iets();
        cmd.env("IETS_CACHE_DIR", self.store.dir().join("import-cache"))
            .args(["eval", "--json", "--pure-eval", "--timings"]);
        if !cached {
            cmd.args(["--no-cache", "--no-tracing-cache"]);
        }
        cmd.arg(format!("{}#{attr}", self.repo.display()))
            .output()
            .unwrap()
    }

    fn nix(&self, attr: &str) -> Output {
        self.store
            .nix("nix")
            .args(["eval", "--json", "--option", "eval-cache", "false"])
            .arg(format!("{}#{attr}", self.repo.display()))
            .output()
            .unwrap()
    }

    fn parity(&self, attr: &str, expected: &str, cached: bool) -> Output {
        // iets goes first: Nix must not materialize a store tree that hides
        // mistakes in virtual source resolution.
        let i = self.iets(attr, cached);
        let n = self.nix(attr);
        assert!(i.status.success(), "{attr}: iets: {}", stderr(&i));
        assert!(n.status.success(), "{attr}: nix: {}", stderr(&n));
        assert_eq!(stdout(&n), expected, "{attr}: oracle result");
        assert_eq!(stdout(&i), stdout(&n), "{attr}: import parity");
        i
    }

    fn rejected(&self, attr: &str, reason: &str) {
        self.rejected_with_reasons(attr, reason, reason);
    }

    fn rejected_with_reasons(&self, attr: &str, iets_reason: &str, nix_reason: &str) {
        let i = self.iets(attr, false);
        let n = self.nix(attr);
        for (name, result, reason) in [("iets", i, iets_reason), ("nix", n, nix_reason)] {
            assert!(!result.status.success(), "{name} accepted {attr}");
            assert!(result.stdout.is_empty(), "{name} exposed {attr}");
            assert!(
                stderr(&result).contains(reason),
                "{name} {attr}: expected {reason:?}: {}",
                stderr(&result)
            );
        }
    }
}

#[test]
fn tracked_ancestor_and_leaf_imports_match_nix() {
    let Some(f) = Fixture::new() else { return };
    for attr in ["ancestor", "leaf"] {
        f.parity(attr, "\"target-one\"", false);
    }
    f.parity("directory", "\"target-one\"", false);
    f.parity("scoped", "\"target-one-scoped\"", false);
    f.parity("identity", "true", false);
}

#[test]
fn import_links_preserve_source_visibility_and_purity() {
    let Some(f) = Fixture::new() else { return };
    for attr in [
        "untracked",
        "untrackedDirectory",
        "untrackedDefault",
        "hiddenLink",
    ] {
        f.rejected(attr, "not tracked by Git");
    }
    // Both targets exist in the checkout's host surroundings. Neither is
    // part of the copied Git tree or the pure evaluator's allow list.
    for attr in ["escape", "absolute"] {
        f.rejected(attr, "forbidden in pure evaluation mode");
    }
    f.rejected_with_reasons(
        "cycle",
        "infinite symlink recursion",
        "too many symbolic links",
    );
    f.parity("danglingExists", "true", false);
    f.rejected_with_reasons(
        "danglingImport",
        "No such file or directory",
        "does not exist in Git repository",
    );
}

#[test]
fn cached_imports_follow_target_edits_retargets_and_tracking_changes() {
    let Some(f) = Fixture::new() else { return };
    f.parity("leaf", "\"target-one\"", true);
    let warm = f.parity("leaf", "\"target-one\"", true);
    assert!(stderr(&warm).contains("cache: hit"), "{}", stderr(&warm));

    fs::write(f.repo.join(".config/nixos/value.nix"), "\"target-two\"\n").unwrap();
    let edited = f.parity("leaf", "\"target-two\"", true);
    assert!(
        !stderr(&edited).contains("cache: hit"),
        "{}",
        stderr(&edited)
    );

    fs::remove_file(f.repo.join("aliases/module.nix")).unwrap();
    symlink("value.nix", f.repo.join("aliases/module.nix")).unwrap();
    let retargeted = f.parity("leaf", "\"wrong-base\"", true);
    assert!(
        !stderr(&retargeted).contains("cache: hit"),
        "{}",
        stderr(&retargeted)
    );
    f.parity("leaf", "\"wrong-base\"", true);

    // Leave the file on disk but remove it from the index: cached access
    // must not keep reading it through the tracked link.
    git(&f.repo, &["rm", "--cached", "aliases/value.nix"]);
    let untracked = f.iets("leaf", true);
    assert!(
        !untracked.status.success(),
        "cached import exposed untracked target"
    );
    assert!(
        stderr(&untracked).contains("not tracked by Git"),
        "{}",
        stderr(&untracked)
    );
    f.rejected("leaf", "not tracked by Git");
}
