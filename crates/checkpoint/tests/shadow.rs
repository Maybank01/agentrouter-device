//! Shadow-git checkpoints on a real folder: undo brings back deleted and changed files and removes new ones,
//! ignored files stay, repeated undos step back turn by turn, an undo can be undone, and nothing the folder's own
//! `.git` configures (hooks, fsmonitor) runs — the person's repository is not touched.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentrouter_checkpoint::Shadow;

fn scratch(tag: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("checkpoint-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("work").join("sub")).unwrap();
    std::fs::create_dir_all(dir.join("data")).unwrap();
    dir
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|_| "<missing>".into())
}

#[test]
fn undo_restores_the_folder_and_never_touches_the_persons_git() {
    if !git_available() {
        eprintln!("no git: skipped");
        return;
    }
    let dir = scratch("undo");
    let work = dir.join("work");
    // The person's own repository, with config that would run code if anything used it.
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&work)
            .status()
            .unwrap()
            .success()
    );
    let marker = dir.join("PWNED");
    let config = work.join(".git").join("config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "[core]\n\tfsmonitor = \"touch '{}'\"\n\thooksPath = hooks-evil\n",
        marker.display()
    ));
    std::fs::write(&config, &text).unwrap();
    let config_before = std::fs::read(&config).unwrap();
    std::fs::write(work.join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(work.join("a.txt"), "one").unwrap();
    std::fs::write(work.join("sub").join("b.txt"), "bee").unwrap();
    std::fs::write(work.join("keep.log"), "ignored").unwrap();

    let shadow = Shadow::new(&dir.join("data"), "lnk_test", &work);
    assert!(
        shadow.git_dir().starts_with(dir.join("data")),
        "outside the folder"
    );
    let first = shadow.snapshot("files__write_file a.txt").unwrap();
    assert_eq!(first.kind, "turn");

    // Turn 1: change, delete, create.
    std::fs::write(work.join("a.txt"), "two").unwrap();
    std::fs::remove_file(work.join("sub").join("b.txt")).unwrap();
    std::fs::write(work.join("new.txt"), "new").unwrap();
    std::fs::write(work.join("keep.log"), "ignored, changed").unwrap();
    let second = shadow.snapshot("turn 2").unwrap();
    // Turn 2: another change.
    std::fs::write(work.join("a.txt"), "three").unwrap();

    // Undo: back to before turn 2.
    let undone = shadow.undo(None).unwrap();
    assert_eq!(undone.restored.id, second.id);
    assert_eq!(read(&work.join("a.txt")), "two");
    // Undo again: back to before turn 1.
    let undone = shadow.undo(None).unwrap();
    assert_eq!(undone.restored.id, first.id);
    assert_eq!(read(&work.join("a.txt")), "one");
    assert_eq!(
        read(&work.join("sub").join("b.txt")),
        "bee",
        "a deleted file comes back"
    );
    assert!(
        !work.join("new.txt").exists(),
        "a file the turn created is removed"
    );
    assert_eq!(
        read(&work.join("keep.log")),
        "ignored, changed",
        "ignored files are left alone"
    );

    // An undo can be undone: the state saved before it.
    let redo = shadow.undo(Some(&undone.saved.id)).unwrap();
    assert_eq!(redo.restored.id, undone.saved.id);
    assert_eq!(read(&work.join("a.txt")), "two");

    let list = shadow.list(10).unwrap();
    assert!(list.len() >= 4 && list.iter().any(|c| c.kind == "before-undo"));

    // The person's repository: config byte-identical, no commits, no hooks run, no fsmonitor run.
    assert_eq!(std::fs::read(&config).unwrap(), config_before);
    let log = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(&work)
        .env_remove("GIT_DIR")
        .output()
        .unwrap();
    assert!(
        !log.status.success() || log.stdout.is_empty(),
        "no commits in the person's repo"
    );
    assert!(!marker.exists(), "nothing configured in the folder ran");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn big_files_are_left_out_and_an_empty_history_says_so() {
    if !git_available() {
        eprintln!("no git: skipped");
        return;
    }
    let dir = scratch("big");
    let work = dir.join("work");
    let shadow = Shadow::new(&dir.join("data"), "lnk_big", &work);
    assert!(shadow.undo(None).is_err(), "nothing to undo yet");
    let big = std::fs::File::create(work.join("big.bin")).unwrap();
    big.set_len(agentrouter_checkpoint::FILE_MAX + 1).unwrap();
    std::fs::write(work.join("small.txt"), "s").unwrap();
    shadow.snapshot("t").unwrap();
    std::fs::remove_file(work.join("small.txt")).unwrap();
    shadow.undo(None).unwrap();
    assert_eq!(read(&work.join("small.txt")), "s");
    assert!(
        work.join("big.bin").exists(),
        "an unsnapshotted big file is not removed by undo"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
