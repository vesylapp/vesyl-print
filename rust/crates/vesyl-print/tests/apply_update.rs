//! `scripts/apply-update`: the root OTA helper only ever repoints
//! `<install_root>/current`, and only at a slot holding the vesyl-print binary.
//!
//! sudoers lets the service user run the helper as root with any arguments,
//! so it must not trust any of them as a path. These tests run a copy of the
//! helper with its fixed install root pointed at a temp dir (standing in for
//! /opt/vesyl-print), the root check bypassed and systemctl stubbed, then
//! check what it accepts and that every rejected call leaves the whole tree
//! untouched, and that `update::slot_is_runnable` (what the agent and CLI
//! check) accepts exactly the slots the helper accepts.

mod common;

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

use common::{have_tools, path_str, repo_root, run, sh_quote, snapshot, write, write_exe, Run};
use vesyl_print::update::slot_is_runnable;

/// The helper's fixed PATH; GNU ln and mv must be found there.
const HELPER_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

fn sub_once(text: &str, old: &str, new: &str) -> String {
    assert_eq!(
        text.matches(old).count(),
        1,
        "expected exactly one {old:?} in apply-update"
    );
    text.replacen(old, new, 1)
}

fn supported() -> bool {
    let coreutils = ["ln", "mv"].iter().all(|tool| {
        HELPER_PATH
            .split(':')
            .any(|dir| Path::new(dir).join(tool).exists())
    });
    cfg!(target_os = "linux") && coreutils && have_tools(&["bash"])
}

fn s(p: &Path) -> String {
    path_str(p).to_string()
}

struct Helper {
    _td: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    calls: PathBuf,
    helper: PathBuf,
}

impl Helper {
    fn new() -> Option<Helper> {
        if !supported() {
            return None;
        }
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        let root = base.join("opt/vesyl-print");
        fs::create_dir_all(root.join("releases")).unwrap();
        let calls = base.join("systemctl.calls");
        let stubs = base.join("stubbin");
        write_exe(
            &stubs.join("systemctl"),
            &format!("#!/bin/sh\necho \"$*\" >> {}\n", sh_quote(&calls)),
        );

        let src = fs::read_to_string(repo_root().join("scripts/apply-update")).unwrap();
        let src = sub_once(
            &src,
            "\nINSTALL_ROOT=/opt/vesyl-print\n",
            &format!("\nINSTALL_ROOT={}\n", sh_quote(&root)),
        );
        let src = sub_once(&src, "[[ $EUID -ne 0 ]]", "false");
        let src = sub_once(
            &src,
            &format!("\nPATH={HELPER_PATH}\n"),
            &format!("\nPATH={}:{HELPER_PATH}\n", sh_quote(&stubs)),
        );
        let helper = base.join("apply-update");
        write_exe(&helper, &src);
        Some(Helper {
            _td: td,
            base,
            root,
            calls,
            helper,
        })
    }

    fn releases(&self) -> PathBuf {
        self.root.join("releases")
    }

    /// A Rust release slot: the executable binary plus the Python LCD.
    fn release(&self, version: &str) -> PathBuf {
        self.release_in(&self.releases(), version)
    }

    fn release_in(&self, releases: &Path, version: &str) -> PathBuf {
        let dir = releases.join(version);
        write_exe(&dir.join("vesyl-print"), "#!/bin/sh\n");
        write(&dir.join("main.py"), "# LCD\n");
        dir
    }

    fn run(&self, args: &[&str]) -> Run {
        let path = std::env::var("PATH").unwrap_or_default();
        run(&self.helper, args, &[("PATH".into(), path)])
    }

    fn activate(&self, version: &str) -> Run {
        let release = s(&self.releases().join(version));
        let current = s(&self.root.join("current"));
        self.run(&["activate", &release, &current])
    }

    fn rollback(&self, version: &str) -> Run {
        self.run(&["rollback", &s(&self.root), version])
    }

    fn current(&self) -> Option<PathBuf> {
        fs::read_link(self.root.join("current")).ok()
    }

    /// A call that must fail and leave every file, dir and link as it was.
    fn assert_rejected(&self, args: &[&str], code: Option<i32>) -> Run {
        let before = snapshot(&self.base, &[&self.helper]);
        let r = self.run(args);
        assert!(!r.ok(), "accepted {args:?}\n{}", r.log());
        if let Some(code) = code {
            assert_eq!(r.code, Some(code), "{args:?}\n{}", r.log());
        }
        assert_eq!(
            snapshot(&self.base, &[&self.helper]),
            before,
            "rejected call {args:?} changed the tree"
        );
        r
    }

    fn assert_activate_rejected(&self, version: &str) -> Run {
        let release = s(&self.releases().join(version));
        let current = s(&self.root.join("current"));
        self.assert_rejected(&["activate", &release, &current], None)
    }
}

macro_rules! helper {
    () => {
        match Helper::new() {
            Some(h) => h,
            None => return,
        }
    };
}

// --- accepted ---------------------------------------------------------------

#[test]
fn activate_points_current_at_relative_slot() {
    let h = helper!();
    h.release("1.2.3");
    let r = h.activate("1.2.3");
    assert!(r.ok(), "{}", r.log());
    assert_eq!(h.current(), Some(PathBuf::from("releases/1.2.3")));
    assert!(h.root.join("current/vesyl-print").is_file());
    assert!(fs::symlink_metadata(h.root.join("current.new")).is_err());
}

#[test]
fn activate_repoints_existing_current() {
    let h = helper!();
    h.release("1.2.3");
    h.release("1.2.4");
    assert!(h.activate("1.2.3").ok());
    let r = h.activate("1.2.4");
    assert!(r.ok(), "{}", r.log());
    assert_eq!(h.current(), Some(PathBuf::from("releases/1.2.4")));
}

#[test]
fn activate_accepts_release_version_shapes() {
    let h = helper!();
    for version in ["0.4.1-rc.1", "1.0.0.2", "10.20.30-beta"] {
        h.release(version);
        let r = h.activate(version);
        assert!(r.ok(), "{version}: {}", r.log());
        assert_eq!(h.current(), Some(Path::new("releases").join(version)));
    }
}

#[test]
fn rollback_activates_version() {
    let h = helper!();
    h.release("1.2.3");
    h.release("1.2.4");
    assert!(h.activate("1.2.4").ok());
    let r = h.rollback("1.2.3");
    assert!(r.ok(), "{}", r.log());
    assert_eq!(h.current(), Some(PathBuf::from("releases/1.2.3")));
    let r = h.run(&["rollback", &format!("{}/", s(&h.root)), "1.2.4"]);
    assert!(r.ok(), "{}", r.log());
    assert_eq!(h.current(), Some(PathBuf::from("releases/1.2.4")));
}

#[test]
fn stale_current_new_symlink_is_replaced_not_followed() {
    let h = helper!();
    h.release("1.2.3");
    let outside = h.base.join("etc");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, h.root.join("current.new")).unwrap();
    let r = h.activate("1.2.3");
    assert!(r.ok(), "{}", r.log());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    assert_eq!(h.current(), Some(PathBuf::from("releases/1.2.3")));
    assert!(fs::symlink_metadata(h.root.join("current.new")).is_err());
}

#[test]
fn restart_restarts_display_then_agent() {
    let h = helper!();
    let r = h.run(&["restart"]);
    assert!(r.ok(), "{}", r.log());
    assert_eq!(
        fs::read_to_string(&h.calls).unwrap(),
        "restart --no-block vesyl-print-display.service\n\
         restart --no-block vesyl-print-agent.service\n"
    );
}

// --- rejected ---------------------------------------------------------------

/// Two Rust slots (1.2.3 active), plus a slot outside the install root.
fn rejecting() -> Option<(Helper, PathBuf)> {
    let h = Helper::new()?;
    h.release("1.2.3");
    h.release("1.2.4");
    assert!(h.activate("1.2.3").ok());
    let attacker = h.release_in(&h.base.join("attacker"), "6.6.6");
    Some((h, attacker))
}

macro_rules! rejecting {
    () => {
        match rejecting() {
            Some(x) => x,
            None => return,
        }
    };
}

#[test]
fn slot_without_the_binary_is_never_activated() {
    let (h, _) = rejecting!();
    let releases = h.releases();
    // A Python-era release (setup.sh keeps none, but an OTA might stage one).
    write(&releases.join("0.3.17/agent.py"), "# Python agent\n");
    write(&releases.join("0.3.17/main.py"), "# LCD\n");
    write(&releases.join("0.3.17/cli.py"), "# Python CLI\n");
    // Not executable / only under bin/ / a directory / nothing at all: the
    // units exec <slot>/vesyl-print, so none of these could start the agent.
    write(&releases.join("2.0.0/vesyl-print"), "#!/bin/sh\n");
    write_exe(&releases.join("2.0.1/bin/vesyl-print"), "#!/bin/sh\n");
    fs::create_dir_all(releases.join("2.0.2/vesyl-print")).unwrap();
    write(&releases.join("2.0.3/README"), "x");
    for version in ["0.3.17", "2.0.0", "2.0.1", "2.0.2", "2.0.3"] {
        let r = h.assert_activate_rejected(version);
        assert!(
            r.stderr.contains("no executable vesyl-print binary"),
            "{version}: {}",
            r.log()
        );
        let r = h.assert_rejected(&["rollback", &s(&h.root), version], None);
        assert!(
            r.stderr.contains("no executable vesyl-print binary"),
            "{version}: {}",
            r.log()
        );
    }
    assert_eq!(h.current(), Some(PathBuf::from("releases/1.2.3")));
}

/// `update::slot_is_runnable`, which the agent and CLI check before they
/// install or roll back to a slot, agrees with the helper on every shape of
/// slot, symlinks included.
#[test]
fn rust_runnable_check_agrees_with_the_helper() {
    let (h, _) = rejecting!();
    let releases = h.releases();
    let script = "#!/bin/sh\n";
    write(&releases.join("1.0.1/vesyl-print"), script);
    write_exe(&releases.join("1.0.2/bin/vesyl-print"), script);
    fs::create_dir_all(releases.join("1.0.3/vesyl-print")).unwrap();
    write(&releases.join("1.0.4/main.py"), "# LCD\n");
    // A link to the executable counts, as `[[ -f && -x ]]` follows it ...
    write_exe(&releases.join("1.1.0/bin/vesyl-print"), script);
    symlink("bin/vesyl-print", releases.join("1.1.0/vesyl-print")).unwrap();
    // ... unless it dangles or what it points at is not executable.
    fs::create_dir_all(releases.join("1.1.1")).unwrap();
    symlink("bin/vesyl-print", releases.join("1.1.1/vesyl-print")).unwrap();
    write(&releases.join("1.1.2/bin/vesyl-print"), script);
    symlink("bin/vesyl-print", releases.join("1.1.2/vesyl-print")).unwrap();
    // A symlinked slot never counts.
    symlink("1.2.4", releases.join("1.2.5")).unwrap();
    for (version, runnable) in [
        ("1.2.4", true),
        ("1.0.1", false),
        ("1.0.2", false),
        ("1.0.3", false),
        ("1.0.4", false),
        ("1.1.0", true),
        ("1.1.1", false),
        ("1.1.2", false),
        ("1.2.5", false),
    ] {
        assert_eq!(
            slot_is_runnable(&releases.join(version)),
            runnable,
            "{version}"
        );
        let r = h.activate(version);
        assert_eq!(r.ok(), runnable, "{version}: {}", r.log());
    }
}

#[test]
fn current_symlink_must_be_install_root_current() {
    let (h, _) = rejecting!();
    let preload = h.base.join("etc/ld.so.preload");
    write(&preload, "original\n");
    let release = s(&h.releases().join("1.2.4"));
    let root = s(&h.root);
    for link in [
        s(&preload),
        s(&h.base.join("current")),
        format!("{root}/current/"),
        format!("{root}/current.new"),
        format!("{root}/releases/../current"),
        format!("{root}//current"),
        "current".to_string(),
        String::new(),
    ] {
        let r = h.assert_rejected(&["activate", &release, &link], None);
        assert!(
            r.stderr.contains("current_symlink must be"),
            "{link}: {}",
            r.log()
        );
    }
    assert_eq!(fs::read_to_string(&preload).unwrap(), "original\n");
    assert_eq!(h.current(), Some(PathBuf::from("releases/1.2.3")));
}

#[test]
fn release_dir_must_be_a_slot_under_install_root() {
    let (h, attacker) = rejecting!();
    let current = s(&h.root.join("current"));
    let root = s(&h.root);
    for release in [
        s(&attacker),
        s(&h.base.join("opt/releases/1.2.4")),
        format!("{root}/releases/../../../attacker/6.6.6"),
        format!("{root}/releases/1.2.4/../../../../attacker/6.6.6"),
        format!("{root}/releases/1.2.4/"),
        format!("{root}/releases/"),
        format!("{root}/releases/."),
        format!("{root}/releases/.."),
        format!("{root}//releases/1.2.4"),
        "releases/1.2.4".to_string(),
        String::new(),
    ] {
        h.assert_rejected(&["activate", &release, &current], None);
    }
}

#[test]
fn version_must_look_like_a_release() {
    let (h, _) = rejecting!();
    for version in [
        "latest", "1.2", "-1.2.3", "1.2.3 ", "1.2.3\n", " 1.2.3", "v1.2.3", "1.2.3/x",
    ] {
        let r = h.assert_activate_rejected(version);
        assert!(
            r.stderr.contains("invalid release version"),
            "{version:?}: {}",
            r.log()
        );
    }
}

#[test]
fn symlinked_slot_rejected() {
    let (h, attacker) = rejecting!();
    symlink(&attacker, h.releases().join("6.6.6")).unwrap();
    let r = h.assert_activate_rejected("6.6.6");
    assert!(r.stderr.contains("missing release_dir"), "{}", r.log());
    h.assert_rejected(&["rollback", &s(&h.root), "6.6.6"], None);
}

#[test]
fn symlinked_releases_dir_rejected() {
    let (h, _) = rejecting!();
    let moved = h.base.join("elsewhere-releases");
    fs::rename(h.releases(), &moved).unwrap();
    symlink(&moved, h.releases()).unwrap();
    h.assert_activate_rejected("1.2.4");
}

#[test]
fn missing_slot_rejected() {
    let (h, _) = rejecting!();
    h.assert_activate_rejected("9.9.9");
}

#[test]
fn existing_current_new_directory_is_not_written_into() {
    let (h, _) = rejecting!();
    fs::create_dir(h.root.join("current.new")).unwrap();
    h.assert_activate_rejected("1.2.4");
}

#[test]
fn rollback_install_root_is_fixed() {
    let (h, _) = rejecting!();
    let other = h.base.join("other");
    h.release_in(&other.join("releases"), "1.2.4");
    let root = s(&h.root);
    for (install_root, version) in [
        (s(&other), "1.2.4"),
        (s(&h.base.join("attacker").join("..")), "1.2.4"),
        (format!("{root}/.."), "1.2.4"),
        (format!("{root}//"), "1.2.4"),
        (root.clone(), "../../../attacker/6.6.6"),
        (root.clone(), ".."),
        (root.clone(), "."),
        (root.clone(), ""),
    ] {
        h.assert_rejected(&["rollback", &install_root, version], None);
    }
    assert_eq!(h.current(), Some(PathBuf::from("releases/1.2.3")));
}

#[test]
fn wrong_arity_and_unknown_commands() {
    let (h, _) = rejecting!();
    let release = format!("{}/releases/1.2.4", s(&h.root));
    let current = s(&h.root.join("current"));
    let root = s(&h.root);
    let cases: [&[&str]; 8] = [
        &[],
        &["bogus"],
        &["activate"],
        &["activate", &release],
        &["activate", &release, &current, "extra"],
        &["rollback", &root],
        &["rollback", &root, "1.2.4", "extra"],
        &["restart", "now"],
    ];
    for args in cases {
        h.assert_rejected(args, Some(2));
    }
    assert!(!h.calls.exists());
}

#[test]
fn unmodified_helper_refuses_non_root() {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 || !supported() {
        return; // root passes the root check
    }
    let bash = common::which("bash").unwrap();
    let helper = repo_root().join("scripts/apply-update");
    let r = run(
        &bash,
        &[path_str(&helper), "rollback", "/opt/vesyl-print", "0.0.0"],
        &[],
    );
    assert_eq!(r.code, Some(1), "{}", r.log());
    assert!(r.stderr.contains("must run as root"), "{}", r.log());
}

#[test]
fn shipped_helper_is_executable() {
    let mode = fs::metadata(repo_root().join("scripts/apply-update"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0, "scripts/apply-update must be executable");
}
