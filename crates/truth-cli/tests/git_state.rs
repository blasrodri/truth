//! Git-state claim verification, end to end: "committed as <sha>", "pushed to
//! origin main", "on branch X", "no longer ahead" — the 26-claim class the
//! field audit found entirely refused. All decided from the local object
//! store and remote-tracking refs; a bare-repo "remote" stands in for origin
//! so push containment is tested without any network.

use std::path::PathBuf;
use std::process::Command;
use truth_cli::verify_turn::{retarget_repo, verify_claims};
use truth_core::config::Config;
use truth_core::enums::VerdictStatus;

struct GitRepo {
    root: PathBuf,
    remote: PathBuf,
}

impl GitRepo {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("truth_git_state_{name}_{}", std::process::id()));
        let root = base.join("work");
        let remote = base.join("remote.git");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(&remote)
            .args(["init", "-q", "--bare"])
            .output()
            .unwrap()
            .status
            .success());
        let r = Self { root, remote };
        r.git(&["init", "-q", "-b", "main"]);
        r.git(&["remote", "add", "origin", &r.remote.to_string_lossy()]);
        r
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(&self, file: &str, content: &str, msg: &str) -> String {
        std::fs::write(self.root.join(file), content).unwrap();
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", msg, "--no-gpg-sign"]);
        self.git(&["rev-parse", "--short", "HEAD"])
    }

    fn verify(&self, claims: &[String]) -> Vec<(String, VerdictStatus)> {
        let mut config = Config::from_toml_str("").unwrap();
        config.loki.enabled = false;
        retarget_repo(&mut config, &self.root.to_string_lossy());
        let conn = truth_db::open(&config.database.path).unwrap();
        let report = verify_claims(&conn, &config, &claims.join(". "), Some(claims), None).unwrap();
        report
            .verdicts
            .into_iter()
            .map(|v| (v.text, v.status))
            .collect()
    }
}

impl Drop for GitRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.root.parent().unwrap());
    }
}

#[test]
fn committed_sha_and_push_state_are_verified() {
    let repo = GitRepo::new("push");
    let sha1 = repo.commit("a.txt", "one\n", "first");
    repo.git(&["push", "-q", "-u", "origin", "main"]);
    let sha2 = repo.commit("a.txt", "two\n", "second"); // NOT pushed

    let verdicts = repo.verify(&[
        format!("The compose changes are committed as {sha1} and pushed to origin main"),
        format!("The fix was committed as {sha2} and pushed to origin main"),
        "The change was committed as deadb33 and pushed to origin main".to_string(),
        "the branch is no longer ahead".to_string(),
    ]);

    assert_eq!(
        verdicts[0].1,
        VerdictStatus::Supported,
        "pushed commit must verify: {verdicts:?}"
    );
    assert_eq!(
        verdicts[1].1,
        VerdictStatus::Contradicted,
        "unpushed commit claimed pushed must contradict: {verdicts:?}"
    );
    assert_eq!(
        verdicts[2].1,
        VerdictStatus::Contradicted,
        "nonexistent sha must contradict: {verdicts:?}"
    );
    assert_eq!(
        verdicts[3].1,
        VerdictStatus::Contradicted,
        "ahead of upstream while claiming no-longer-ahead must contradict: {verdicts:?}"
    );

    // After pushing, both formerly-false claims become true.
    repo.git(&["push", "-q", "origin", "main"]);
    let verdicts = repo.verify(&[
        format!("The fix was committed as {sha2} and pushed to origin main"),
        "the branch is no longer ahead".to_string(),
    ]);
    assert_eq!(verdicts[0].1, VerdictStatus::Supported, "{verdicts:?}");
    assert_eq!(verdicts[1].1, VerdictStatus::Supported, "{verdicts:?}");
}

#[test]
fn branch_claims_are_verified() {
    let repo = GitRepo::new("branch");
    let sha = repo.commit("a.txt", "one\n", "first");
    repo.git(&["checkout", "-qb", "feat/luca-leads-dlr"]);
    let sha2 = repo.commit("b.txt", "two\n", "feature work");

    let verdicts = repo.verify(&[
        format!("The changes were committed as {sha2} on branch feat/luca-leads-dlr"),
        format!("The changes were committed as {sha} on branch feat/nope"),
    ]);
    assert_eq!(
        verdicts[0].1,
        VerdictStatus::Supported,
        "commit on existing branch must verify: {verdicts:?}"
    );
    assert_eq!(
        verdicts[1].1,
        VerdictStatus::Contradicted,
        "nonexistent branch must contradict: {verdicts:?}"
    );
}

#[test]
fn push_claim_without_remote_refs_is_refused_not_contradicted() {
    // No push has ever happened → no remote-tracking refs → "not pushed" is
    // UNKNOWABLE here, and contradicting would be a false accusation.
    let repo = GitRepo::new("norefs");
    let sha = repo.commit("a.txt", "one\n", "first");
    let verdicts = repo.verify(&[format!(
        "The change was committed as {sha} and pushed to origin main"
    )]);
    assert_ne!(
        verdicts[0].1,
        VerdictStatus::Contradicted,
        "no remote refs → refuse, never accuse: {verdicts:?}"
    );
}
