//! `setup.sh`: the root helpers are installed with INSTALL_ROOT written in,
//! in plain form (so OTA activations match it), a non-default install root
//! provisions and activates through the installed `apply-update`, the
//! services run as SUDO_USER or else the tree's owner (never root), a missing
//! optional package never blocks the required ones, and re-provisioning from
//! an extracted release without a Tailscale key skips Tailscale and removes
//! only that tree. setup.sh and `apply-update` take exactly the release
//! versions update.rs `is_version` takes, and the agent unit outlives a
//! renderer killed for memory.
//!
//! The unprivileged tests run setup.sh's preflight, which checks the release
//! tree before it asks for sudo, with a `sudo` stub that records the hand-off.
//! The root_* tests (ignored: they need root, or a user namespace that maps
//! the service account's uid, as `unshare --map-root-user --map-auto` does)
//! run all of setup.sh in a chroot inside a private mount namespace: the
//! host's /usr is mounted read-only, everything setup.sh writes lands in a
//! temp dir, and apt-get, dpkg-query, systemctl, usermod, visudo,
//! update-initramfs, tailscale, curl and sudo are stubs that log their calls.

mod common;

use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{
    have_tools, output, path_str, repo_root, run, sh_quote, which, write, write_exe, Run,
};
use vesyl_print::update::is_version;

const VERSION: &str = "0.9.1";
/// A non-default install root.
const CUSTOM_ROOT: &str = "/srv/vesyl-print";
/// The service account in the sandbox's /etc/passwd.
const SERVICE_USER: &str = "vesyl";
const SERVICE_UID: u32 = 1000;
/// PATH inside the sandbox: the stubs first.
const SANDBOX_PATH: &str = "/stubs:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// What the release's `wifi_setup.py` stands in for: it reports where the
/// Wi-Fi helper loaded it from.
const FAKE_WIFI_SETUP_PY: &str = "def main():\n    print(__file__)\n    return 0\n";

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// True when `uid` has a mapping in this process's user namespace (always,
/// as real root).
fn uid_mapped(uid: u32) -> bool {
    let Ok(map) = fs::read_to_string("/proc/self/uid_map") else {
        return false;
    };
    let uid = u64::from(uid);
    map.lines().any(|line| {
        let f: Vec<u64> = line
            .split_whitespace()
            .filter_map(|n| n.parse().ok())
            .collect();
        f.len() == 3 && f[0] <= uid && uid < f[0] + f[2]
    })
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Replace the one `old` in the file at `path` (an edited copy of a script).
fn edit(path: &Path, old: &str, new: &str) {
    let text = read(path);
    assert_eq!(
        text.matches(old).count(),
        1,
        "expected exactly one {old:?} in {}",
        path.display()
    );
    write_exe(path, &text.replacen(old, new, 1));
}

/// The Wi-Fi helper as it was before it had an install-root line: setup.sh
/// copied it verbatim, so it loaded wifi_setup.py from /opt/vesyl-print
/// whatever the install root.
fn make_wifi_helper_pre_install_root(tree: &Path) {
    let helper = tree.join("scripts/wifi-setup");
    edit(&helper, "INSTALL_ROOT = Path(\"/opt/vesyl-print\")\n", "");
    edit(
        &helper,
        "INSTALL_ROOT / \"current\",",
        "Path(\"/opt/vesyl-print/current\"),",
    );
}

/// An extracted release at `dir`: this repository's setup.sh, units, root
/// helpers and public key, a stand-in binary that reports [`VERSION`], and a
/// stand-in LCD.
fn release_tree(dir: &Path) {
    let repo = repo_root();
    for rel in ["setup.sh", "scripts/apply-update", "scripts/wifi-setup"] {
        write_exe(&dir.join(rel), &read(&repo.join(rel)));
    }
    for rel in [
        "vesyl-print-agent.service",
        "vesyl-print-display.service",
        "keys/update_public.pem",
    ] {
        write(&dir.join(rel), read(&repo.join(rel)));
    }
    write(&dir.join("VERSION"), format!("{VERSION}\n"));
    write_exe(
        &dir.join("vesyl-print"),
        &format!("#!/bin/sh\necho \"vesyl-print {VERSION}\"\n"),
    );
    write(&dir.join("main.py"), "# LCD\n");
    write(&dir.join("wifi_setup.py"), FAKE_WIFI_SETUP_PY);
    write(&dir.join("overlays/mhs35.dtbo"), "dtbo\n");
}

// --- preflight, unprivileged --------------------------------------------------

/// A release tree whose setup.sh runs unprivileged with `sudo` stubbed: the
/// stub records its arguments and exits 97, so a run that passes preflight
/// stops at the hand-off to sudo.
struct Preflight {
    _td: tempfile::TempDir,
    tree: PathBuf,
    stubs: PathBuf,
    sudo_log: PathBuf,
}

impl Preflight {
    fn new() -> Option<Preflight> {
        if euid() == 0 {
            // Past preflight, setup.sh as root would provision this machine.
            eprintln!("skipping: running as root (the root_* tests run setup.sh in a sandbox)");
            return None;
        }
        if !cfg!(target_os = "linux") || !have_tools(&["bash", "grep", "sed"]) {
            return None;
        }
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        let tree = base.join(format!("vesyl-print-{VERSION}"));
        release_tree(&tree);
        let stubs = base.join("stubs");
        let sudo_log = base.join("sudo.log");
        write_exe(
            &stubs.join("sudo"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexit 97\n",
                sh_quote(&sudo_log)
            ),
        );
        Some(Preflight {
            _td: td,
            tree,
            stubs,
            sudo_log,
        })
    }

    fn run(&self) -> Run {
        self.run_with_root(CUSTOM_ROOT)
    }

    fn run_with_root(&self, install_root: &str) -> Run {
        let path = format!(
            "{}:{}",
            path_str(&self.stubs),
            std::env::var("PATH").unwrap_or_default()
        );
        run(
            &self.tree.join("setup.sh"),
            &[],
            &[
                ("PATH".into(), path),
                ("INSTALL_ROOT".into(), install_root.into()),
            ],
        )
    }

    fn sudo_calls(&self) -> String {
        fs::read_to_string(&self.sudo_log).unwrap_or_default()
    }

    /// The run stopped in preflight, naming `file`, and never asked for sudo.
    fn assert_refused(&self, r: &Run, file: &str) {
        assert_eq!(r.code, Some(1), "{}", r.log());
        let file = self.tree.join(file);
        assert!(
            r.stderr.contains(&format!("{}: ", path_str(&file)))
                && r.stderr.contains("expected exactly 1"),
            "{}",
            r.log()
        );
        assert_eq!(self.sudo_calls(), "", "{}", r.log());
    }
}

#[test]
fn preflight_hands_a_release_tree_to_sudo() {
    let Some(p) = Preflight::new() else { return };
    let r = p.run();
    assert_eq!(r.code, Some(97), "{}", r.log());
    assert_eq!(
        p.sudo_calls(),
        format!(
            "-- env INSTALL_ROOT={CUSTOM_ROOT} bash {}\n",
            path_str(&p.tree.join("setup.sh"))
        )
    );
}

#[test]
fn preflight_refuses_a_wifi_helper_without_its_install_root_line() {
    let Some(p) = Preflight::new() else { return };
    make_wifi_helper_pre_install_root(&p.tree);
    p.assert_refused(&p.run(), "scripts/wifi-setup");
}

#[test]
fn preflight_refuses_an_install_root_line_that_is_not_exact_or_not_alone() {
    let Some(p) = Preflight::new() else { return };
    let helper = p.tree.join("scripts/wifi-setup");
    let line = "INSTALL_ROOT = Path(\"/opt/vesyl-print\")\n";
    // A second copy: which one would the installed helper use?
    edit(&helper, line, &format!("{line}{line}"));
    p.assert_refused(&p.run(), "scripts/wifi-setup");
    // Not the whole line: the anchored pattern must not half-match it.
    edit(
        &helper,
        &format!("{line}{line}"),
        "INSTALL_ROOT = Path(\"/opt/vesyl-print\")  # fixed\n",
    );
    p.assert_refused(&p.run(), "scripts/wifi-setup");
}

#[test]
fn preflight_refuses_an_apply_update_without_its_install_root_line() {
    let Some(p) = Preflight::new() else { return };
    edit(
        &p.tree.join("scripts/apply-update"),
        "\nINSTALL_ROOT=/opt/vesyl-print\n",
        "\nINSTALL_ROOT=\"/opt/vesyl-print\"\n",
    );
    p.assert_refused(&p.run(), "scripts/apply-update");
}

/// The installed apply-update compares its install root, as a string, with
/// the paths the agent builds from the units' VESYL_PRINT_INSTALL_ROOT
/// (`Path::join` drops a trailing slash): with `INSTALL_ROOT=/srv/vp/`
/// written into both, every OTA activation was refused. setup.sh hands on
/// only a plain absolute path.
#[test]
fn preflight_normalizes_the_install_root_or_refuses_it() {
    let Some(p) = Preflight::new() else { return };
    let setup = path_str(&p.tree.join("setup.sh")).to_string();
    let reset = || {
        let _ = fs::remove_file(&p.sudo_log);
    };
    for (given, used) in [
        ("/srv/vesyl-print/", CUSTOM_ROOT),
        ("/srv/vesyl-print//", CUSTOM_ROOT),
        ("/srv/vesyl-print///", CUSTOM_ROOT),
        // Dots within a name are fine.
        ("/srv/vesyl-print.v2", "/srv/vesyl-print.v2"),
        ("/srv/..x/.vp/", "/srv/..x/.vp"),
    ] {
        reset();
        let r = p.run_with_root(given);
        assert_eq!(r.code, Some(97), "{given}: {}", r.log());
        assert_eq!(
            p.sudo_calls(),
            format!("-- env INSTALL_ROOT={used} bash {setup}\n"),
            "{given}"
        );
    }
    for given in [
        "/srv//vesyl-print",
        "/srv/./vesyl-print",
        "/srv/vesyl-print/.",
        "/srv/vesyl-print/..",
        "/opt/vesyl-print/../etc",
        "/",
        "//",
        "srv/vesyl-print",
        "/srv/vesyl print",
        "/srv/vesyl-print\n",
    ] {
        reset();
        let r = p.run_with_root(given);
        assert_eq!(r.code, Some(1), "{given:?}: {}", r.log());
        assert!(
            r.stderr.contains(&format!(
                "INSTALL_ROOT must be an absolute path whose components are letters, \
                 digits and ._- (no '.', '..' or empty ones): '{given}'"
            )),
            "{given:?}: {}",
            r.log()
        );
        assert_eq!(p.sudo_calls(), "", "{given:?}");
    }
}

/// A renderer (pdftoppm, gs) the kernel kills for memory fails its print
/// job without stopping the agent, and the agent's cgroup is bounded, so a
/// runaway render cannot take the LCD and the rest of the system into the
/// kernel's OOM killer. systemd-analyze, where installed, accepts the unit.
#[test]
fn agent_unit_survives_a_renderer_killed_for_memory() {
    let unit = read(&repo_root().join("vesyl-print-agent.service"));
    let service: Vec<&str> = unit
        .lines()
        .skip_while(|l| *l != "[Service]")
        .take_while(|l| *l != "[Install]")
        .collect();
    for line in ["OOMPolicy=continue", "MemoryMax=50%", "Restart=on-failure"] {
        assert!(service.contains(&line), "{line} not in [Service]\n{unit}");
    }

    let Some(analyze) = which("systemd-analyze") else {
        eprintln!("not verifying the unit: no systemd-analyze");
        return;
    };
    // As installed, but runnable here: an existing binary, no service user.
    let td = tempfile::tempdir().unwrap();
    let copy = td.path().join("vesyl-print-agent.service");
    let exec = which("true").expect("true");
    write(
        &copy,
        unit.replace("/opt/vesyl-print/current/vesyl-print", path_str(&exec))
            .replace(
                "WorkingDirectory=/opt/vesyl-print/current",
                "WorkingDirectory=/",
            )
            .replace("\nUser=vesyl\n", "\n"),
    );
    let out = output(
        Command::new(analyze)
            .args(["verify", "--man=no"])
            .arg(&copy),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let about_unit: Vec<&str> = stderr
        .lines()
        .filter(|l| l.contains("vesyl-print-agent.service"))
        .collect();
    assert!(about_unit.is_empty(), "{stderr}");
}

/// Version names the scripts must judge as update.rs `is_version` does
/// (ASCII: the helpers run in the C locale).
const VERSION_CANDIDATES: &[&str] = &[
    "0.9.1",
    "10.20.30",
    "0.9.1-rc.1",
    "0.9.1.lab",
    "0.9.1-staging",
    "0.9.1.staging",
    "0.9.1-rc.staging",
    "0.9.1-rc.1.staging",
    "0.9.1.staging.2",
    "0.9.1..",
    "0.9.1.",
    "0.9.1-",
    "0.9",
    "0.9.1+build.5",
    "0.9.1/x",
    "../0.9.1",
    "a.b.c",
    "",
];

/// setup.sh installs a release only under a name the agent counts as a
/// release: never `X.Y.Z.staging`, an interrupted extract's directory.
#[test]
fn preflight_version_check_matches_update_rs_is_version() {
    let Some(p) = Preflight::new() else { return };
    for &v in VERSION_CANDIDATES {
        write(&p.tree.join("VERSION"), format!("{v}\n"));
        write_exe(
            &p.tree.join("vesyl-print"),
            &format!("#!/bin/sh\necho \"vesyl-print {v}\"\n"),
        );
        let r = p.run();
        let refused = r.code == Some(1)
            && r.stderr
                .contains(&format!("VERSION ('{v}') is not a release version"));
        let handed_to_sudo = r.code == Some(97);
        assert!(refused != handed_to_sudo, "{v:?}: {}", r.log());
        assert_eq!(refused, !is_version(v), "{v:?}: {}", r.log());
    }
}

/// apply-update (run unprivileged: its install root is a temp dir and the
/// root check is dropped) activates only a version update.rs `is_version`
/// accepts. A `<version>.staging` dir never becomes `current`, even one
/// holding a runnable tree.
#[test]
fn apply_update_takes_only_what_update_rs_is_version_takes() {
    if !cfg!(target_os = "linux") || !have_tools(&["bash"]) {
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let base = td.path().canonicalize().unwrap();
    let root = base.join("opt/vesyl-print");
    fs::create_dir_all(root.join("releases")).unwrap();
    let helper = base.join("apply-update");
    write_exe(&helper, &read(&repo_root().join("scripts/apply-update")));
    edit(
        &helper,
        "\nINSTALL_ROOT=/opt/vesyl-print\n",
        &format!("\nINSTALL_ROOT={}\n", sh_quote(&root)),
    );
    edit(&helper, "[[ $EUID -ne 0 ]]", "false");
    let path = std::env::var("PATH").unwrap_or_default();
    let helper_run = |args: &[&str]| run(&helper, args, &[("PATH".into(), path.clone())]);
    let current = root.join("current");

    // No slots: a version gets as far as looking for its slot.
    for &v in VERSION_CANDIDATES {
        let release = root.join("releases").join(v);
        for args in [
            ["activate", path_str(&release), path_str(&current)],
            ["rollback", path_str(&root), v],
        ] {
            let r = helper_run(&args);
            assert_eq!(r.code, Some(1), "{args:?}: {}", r.log());
            let refused = r
                .stderr
                .contains(&format!("invalid release version: {v}\n"));
            assert_eq!(refused, !is_version(v), "{args:?}: {}", r.log());
        }
    }

    let staging = root.join("releases/0.9.1.staging");
    write_exe(&staging.join("vesyl-print"), "#!/bin/sh\n");
    for args in [
        ["activate", path_str(&staging), path_str(&current)],
        ["rollback", path_str(&root), "0.9.1.staging"],
    ] {
        let r = helper_run(&args);
        assert_eq!(r.code, Some(1), "{args:?}: {}", r.log());
        assert!(
            r.stderr.contains("invalid release version: 0.9.1.staging"),
            "{}",
            r.log()
        );
        assert!(fs::symlink_metadata(&current).is_err(), "{args:?}");
    }
    write_exe(&root.join("releases/0.9.1/vesyl-print"), "#!/bin/sh\n");
    let r = helper_run(&["rollback", path_str(&root), "0.9.1"]);
    assert!(r.ok(), "{}", r.log());
    assert_eq!(
        fs::read_link(&current).unwrap(),
        Path::new("releases/0.9.1")
    );
}

// --- the whole of setup.sh, as root, in a sandbox -------------------------

/// A chroot for setup.sh: the host's /usr (read-only) and library dirs, its
/// own /etc, /boot, /opt, /var, /root and /tmp, /usr/local from `usr_local`,
/// and stubs for everything that would touch the system or the network.
struct Sandbox {
    /// `None` once kept (see [`Sandbox::check_unmounted`]).
    td: Option<tempfile::TempDir>,
    root: PathBuf,
    /// Mounted at `<root>/usr/local`; what setup.sh installs there stays.
    usr_local: PathBuf,
    /// Mounts, then `chroot <root> env -i "$@"`.
    script: PathBuf,
    unshare: PathBuf,
}

/// The stubs, as `(name, what they do after logging the call)`.
const STUBS: &[(&str, &str)] = &[
    // With $APT_OFFLINE set every call fails. `install` records the packages
    // in /dpkg.installed, unless one of them is in $APT_MISSING (not
    // packaged here): then, like apt-get, it installs none of them.
    (
        "apt-get",
        "[ -z \"$APT_OFFLINE\" ] || { echo 'E: Failed to fetch' >&2; exit 100; }\n\
         [ \"$1\" = install ] || exit 0\n\
         shift\n\
         for p; do\n  \
           case \" $APT_MISSING \" in *\" $p \"*)\n    \
             echo \"E: Unable to locate package $p\" >&2; exit 100 ;;\n  \
           esac\n\
         done\n\
         for p; do case \"$p\" in -*) ;; *) echo \"$p\" >> /dpkg.installed ;; esac; done",
    ),
    // `dpkg-query -W -f=… PKG`: "installed" for what is in /dpkg.installed.
    (
        "dpkg-query",
        "for p; do :; done\n\
         grep -qx -- \"$p\" /dpkg.installed 2>/dev/null ||\n  \
           { echo \"dpkg-query: no packages found matching $p\" >&2; exit 1; }\n\
         printf installed",
    ),
    ("usermod", "exit 0"),
    ("visudo", "exit 0"),
    ("systemctl", "exit 0"),
    ("update-initramfs", "exit 0"),
    // Installed and already joined: `status` succeeds.
    (
        "tailscale",
        "case \"$1\" in ip) echo 100.64.0.7 ;; esac\nexit 0",
    ),
    ("hostname", "echo VESYL-PRINT-TEST"),
    // Tripwires: no network, and root never needs sudo.
    ("curl", "exit 1"),
    ("sudo", "exit 97"),
];

impl Sandbox {
    fn new() -> Option<Sandbox> {
        if euid() != 0 {
            return None;
        }
        if !cfg!(target_os = "linux") || !have_tools(&["bash", "unshare", "mount", "chroot", "env"])
        {
            return None;
        }
        if !uid_mapped(SERVICE_UID) {
            eprintln!(
                "skipping: uid {SERVICE_UID} is not mapped here \
                 (run under unshare --map-root-user --map-auto, or as root)"
            );
            return None;
        }
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        let root = base.join("root");
        for dir in [
            "etc/systemd/system",
            "etc/sudoers.d",
            "boot/firmware/overlays",
            "var/lib",
            "opt",
            "root",
            "home/vesyl",
            "dev",
            "usr",
            "tmp",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::set_permissions(root.join("tmp"), fs::Permissions::from_mode(0o1777)).unwrap();
        fs::set_permissions(root.join("root"), fs::Permissions::from_mode(0o700)).unwrap();
        write(
            &root.join("etc/passwd"),
            format!(
                "root:x:0:0:root:/root:/bin/bash\n\
                 {SERVICE_USER}:x:{SERVICE_UID}:{SERVICE_UID}::/home/vesyl:/bin/bash\n"
            ),
        );
        write(
            &root.join("etc/group"),
            format!("root:x:0:\n{SERVICE_USER}:x:{SERVICE_UID}:\n"),
        );
        write(
            &root.join("etc/nsswitch.conf"),
            "passwd: files\ngroup: files\n",
        );
        write(
            &root.join("boot/firmware/config.txt"),
            "# Raspberry Pi\ndtparam=audio=on\n",
        );
        for dev in ["null", "zero", "random", "urandom"] {
            write(&root.join("dev").join(dev), "");
        }
        for (name, body) in STUBS {
            write_exe(
                &root.join("stubs").join(name),
                &format!("#!/bin/sh\necho \"{name} $*\" >> /calls.log\n{body}\n"),
            );
        }

        // Host trees the chroot sees: /usr, and the top-level library and
        // binary dirs as the host has them (symlinks into /usr, or dirs).
        let mut binds = vec!["/usr".to_string()];
        for top in ["bin", "sbin", "lib", "lib32", "lib64", "libx32"] {
            let host = Path::new("/").join(top);
            match fs::symlink_metadata(&host) {
                Ok(m) if m.file_type().is_symlink() => {
                    symlink(fs::read_link(&host).unwrap(), root.join(top)).unwrap();
                }
                Ok(m) if m.is_dir() => {
                    fs::create_dir(root.join(top)).unwrap();
                    binds.push(path_str(&host).to_string());
                }
                _ => {}
            }
        }
        // Debian's /usr/bin links through /etc/alternatives; the linker
        // cache finds multiarch libraries.
        if Path::new("/etc/alternatives").is_dir() {
            fs::create_dir(root.join("etc/alternatives")).unwrap();
            binds.push("/etc/alternatives".into());
        }
        if Path::new("/etc/ld.so.cache").is_file() {
            write(&root.join("etc/ld.so.cache"), "");
            binds.push("/etc/ld.so.cache".into());
        }

        // As on any Debian system (setup.sh installs the CLI into bin/).
        let usr_local = base.join("usr-local");
        for dir in ["bin", "sbin", "lib"] {
            fs::create_dir_all(usr_local.join(dir)).unwrap();
        }
        let mut script = String::from("set -eu\n");
        for host in &binds {
            let target = sh_quote(root.join(host.trim_start_matches('/')));
            script += &format!("mount --rbind {host} {target}\n");
            // As real root the host tree would be writable from the chroot.
            // In a user namespace it is not (its owner is unmapped), and the
            // remount may be refused.
            script += &format!(
                "mount -o remount,bind,ro {target} 2>/dev/null || ! [ -w {host} ] ||\n  \
                 {{ echo 'cannot mount {host} read-only' >&2; exit 1; }}\n"
            );
        }
        script += &format!(
            "mount --bind {} {}\n",
            sh_quote(&usr_local),
            sh_quote(root.join("usr/local"))
        );
        for dev in ["null", "zero", "random", "urandom"] {
            script += &format!(
                "mount --bind /dev/{dev} {}\n",
                sh_quote(root.join("dev").join(dev))
            );
        }
        script += &format!("exec chroot {} /usr/bin/env -i \"$@\"\n", sh_quote(&root));
        let script_path = base.join("sandbox.sh");
        write(&script_path, script);

        Some(Sandbox {
            td: Some(td),
            root,
            usr_local,
            script: script_path,
            unshare: which("unshare").unwrap(),
        })
    }

    /// `path` inside the chroot, as the host sees it.
    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    /// Run `argv` in the chroot, in a private mount namespace, with exactly
    /// `env` plus PATH (stubs first), HOME and LC_ALL.
    fn exec(&mut self, env: &[(&str, &str)], argv: &[&str]) -> Run {
        let mut args: Vec<String> = ["--mount", "--propagation", "private", "--", "bash"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        args.push(path_str(&self.script).into());
        args.push(format!("PATH={SANDBOX_PATH}"));
        args.push("HOME=/root".into());
        args.push("LC_ALL=C".into());
        args.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
        args.extend(argv.iter().map(|a| a.to_string()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let path = std::env::var("PATH").unwrap_or_default();
        let r = run(&self.unshare, &args, &[("PATH".into(), path)]);
        self.check_unmounted();
        r
    }

    /// `sudo ./setup.sh` from the service account: run the setup.sh of the
    /// release at `src` (a path inside the chroot) as root, SUDO_USER set.
    fn setup(&mut self, src: &str, env: &[(&str, &str)]) -> Run {
        let mut env = env.to_vec();
        env.push(("SUDO_USER", SERVICE_USER));
        self.exec(&env, &[&format!("{src}/setup.sh")])
    }

    /// The stubs' log: one `name args` line per call.
    fn calls(&self) -> String {
        fs::read_to_string(self.at("/calls.log")).unwrap_or_default()
    }

    /// The namespace, and its mounts, ended with the command: `usr` is an
    /// empty directory again. Otherwise removing the temp dir could walk
    /// into the host's /usr, so it is left in place and the test fails.
    fn check_unmounted(&mut self) {
        let empty = fs::read_dir(self.root.join("usr")).is_ok_and(|mut d| d.next().is_none());
        if !empty {
            let kept = self.td.take().map(tempfile::TempDir::keep);
            panic!("sandbox mounts outlived their namespace; left in place: {kept:?}");
        }
    }
}

/// Owner uid of `path` (not following a symlink).
fn owner(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().uid()
}

/// `chown -R uid:uid path`, never following a symlink.
fn chown_tree(path: &Path, uid: u32) {
    std::os::unix::fs::lchown(path, Some(uid), Some(uid)).unwrap();
    if fs::symlink_metadata(path).unwrap().is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            chown_tree(&entry.unwrap().path(), uid);
        }
    }
}

/// True when the stubs' log has a call of `name`.
fn called(calls: &str, name: &str) -> bool {
    calls
        .lines()
        .any(|l| l == name || l.starts_with(&format!("{name} ")))
}

#[test]
#[ignore = "needs root (or a user namespace mapping uid 1000) to chroot and chown"]
fn root_custom_install_root_provisions_and_activates() {
    let Some(mut sb) = Sandbox::new() else { return };
    let src = format!("/root/vesyl-print-{VERSION}");
    release_tree(&sb.at(&src));
    // A stale default-root slot: nothing may load code from it.
    write(
        &sb.at("/opt/vesyl-print/current/wifi_setup.py"),
        "def main():\n    print('stale /opt slot')\n    return 0\n",
    );

    let r = sb.setup(&src, &[("INSTALL_ROOT", CUSTOM_ROOT)]);
    assert!(r.ok(), "{}", r.log());
    assert!(r.stdout.contains("==> Done."), "{}", r.log());

    // Both root helpers carry the install root, root-owned and 0755.
    let helpers = sb.usr_local.join("lib/vesyl-print");
    let apply = read(&helpers.join("apply-update"));
    assert!(
        apply
            .lines()
            .any(|l| l == format!("INSTALL_ROOT={CUSTOM_ROOT}")),
        "{apply}"
    );
    let wifi = read(&helpers.join("wifi-setup"));
    assert!(
        wifi.lines()
            .any(|l| l == format!("INSTALL_ROOT = Path(\"{CUSTOM_ROOT}\")")),
        "{wifi}"
    );
    for helper in ["apply-update", "wifi-setup"] {
        let path = helpers.join(helper);
        assert!(!read(&path).contains("/opt/vesyl-print"), "{helper}");
        assert_eq!((owner(&path), mode(&path)), (0, 0o755), "{helper}");
    }
    let sudoers = sb.at("/etc/sudoers.d/vesyl-print");
    assert_eq!(
        read(&sudoers),
        "# vesyl-print helpers — managed by setup.sh (do not edit by hand)\n\
         vesyl ALL=(root) NOPASSWD: /usr/local/lib/vesyl-print/apply-update\n\
         vesyl ALL=(root) NOPASSWD: /usr/local/lib/vesyl-print/wifi-setup\n"
    );
    assert_eq!(mode(&sudoers), 0o440);

    // The release, activated through the installed apply-update (which only
    // accepts its own install root), owned by the service account.
    let install = sb.at(CUSTOM_ROOT);
    let slot = install.join("releases").join(VERSION);
    assert_eq!(
        fs::read_link(install.join("current")).unwrap(),
        Path::new("releases").join(VERSION)
    );
    assert_eq!(mode(&slot.join("vesyl-print")) & 0o111, 0o111);
    assert_eq!(read(&slot.join("VERSION")), format!("{VERSION}\n"));
    for path in [&install, &slot, &slot.join("main.py")] {
        assert_eq!(owner(path), SERVICE_UID, "{}", path.display());
    }
    assert!(!sb.at("/opt/vesyl-print/releases").exists());

    // Units and the CLI wrapper run from the custom root.
    for unit in ["vesyl-print-agent.service", "vesyl-print-display.service"] {
        let text = read(&sb.at("/etc/systemd/system").join(unit));
        assert!(text.contains("\nUser=vesyl\n"), "{text}");
        assert!(!text.contains("/opt/vesyl-print"), "{text}");
    }
    let agent = read(&sb.at("/etc/systemd/system/vesyl-print-agent.service"));
    assert!(
        agent.contains(&format!(
            "\nExecStart={CUSTOM_ROOT}/current/vesyl-print agent\n"
        )),
        "{agent}"
    );
    assert!(
        agent.contains(&format!(
            "\nEnvironment=VESYL_PRINT_INSTALL_ROOT={CUSTOM_ROOT}\n"
        )),
        "{agent}"
    );
    let calls = sb.calls();
    for call in [
        "systemctl daemon-reload",
        "systemctl enable vesyl-print-display",
        "systemctl enable vesyl-print-agent",
        "systemctl restart vesyl-print-agent",
    ] {
        assert!(calls.lines().any(|l| l == call), "{call}\n{calls}");
    }
    assert!(
        !called(&calls, "sudo") && !called(&calls, "curl"),
        "{calls}"
    );

    // The installed CLI runs the active binary; the installed Wi-Fi helper
    // loads the active release's wifi_setup.py, not the stale /opt one.
    let r = sb.exec(&[], &["/usr/local/bin/vesyl-print", "--version"]);
    assert_eq!(r.stdout, format!("vesyl-print {VERSION}\n"), "{}", r.log());
    if Path::new("/usr/bin/python3").exists() {
        let r = sb.exec(
            &[("PYTHONDONTWRITEBYTECODE", "1")],
            &["/usr/local/lib/vesyl-print/wifi-setup"],
        );
        assert_eq!(
            r.stdout,
            format!("{CUSTOM_ROOT}/current/wifi_setup.py\n"),
            "{}",
            r.log()
        );
    } else {
        eprintln!("not running the Wi-Fi helper: no /usr/bin/python3");
    }

    // The extracted release it ran from is gone.
    assert!(!sb.at(&src).exists());
    assert!(sb.at("/root").is_dir());
}

#[test]
#[ignore = "needs root (or a user namespace mapping uid 1000) to chroot and chown"]
fn root_reprovisions_a_joined_device_from_an_extracted_release() {
    let Some(mut sb) = Sandbox::new() else { return };
    // A device set up before: a Python-era slot, a lab build of the binary
    // (active), Python units, its config, credentials and queued job.
    let install = sb.at("/opt/vesyl-print");
    write(
        &install.join("releases/0.3.17/agent.py"),
        "# Python agent\n",
    );
    write(&install.join("releases/0.4.1/main.py"), "# LCD\n");
    write_exe(
        &install.join("releases/0.4.1/vesyl-print"),
        "#!/bin/sh\necho \"vesyl-print 0.4.1\"\n",
    );
    symlink("releases/0.4.1", install.join("current")).unwrap();
    // What an update killed while it unpacked leaves beside its slot.
    write_exe(
        &install.join("releases/0.4.2.staging/vesyl-print-0.4.2/vesyl-print"),
        "#!/bin/sh\necho \"vesyl-print 0.4.2\"\n",
    );
    write(
        &sb.at("/etc/systemd/system/vesyl-print-agent.service"),
        "[Service]\nExecStart=/usr/bin/python3 /opt/vesyl-print/current/agent.py\n",
    );
    let config = r#"{"api_base_url": "https://lab.invalid"}"#;
    write(&sb.at("/etc/vesyl-print/config.json"), config);
    write(
        &sb.at("/etc/vesyl-print/credentials.json"),
        "{\"node_id\": \"n1\"}",
    );
    write(&sb.at("/var/lib/vesyl-print/queue/j1.json"), "{}");
    for path in [
        "/opt/vesyl-print",
        "/opt/vesyl-print/releases",
        "/etc/vesyl-print",
        "/var/lib/vesyl-print",
    ] {
        std::os::unix::fs::chown(sb.at(path), Some(SERVICE_UID), Some(SERVICE_UID)).unwrap();
    }
    // Extracted next to other files, with no keys/tailscale.key.
    let src = format!("/root/vesyl-print-{VERSION}");
    release_tree(&sb.at(&src));
    write(&sb.at("/root/notes.txt"), "keep\n");

    // A lab run keeps the source tree.
    let r = sb.setup(&src, &[("SKIP_SOURCE_CLEANUP", "1")]);
    assert!(r.ok(), "{}", r.log());
    assert!(
        r.stdout
            .contains("==> SKIP_SOURCE_CLEANUP=1 — keeping source tree"),
        "{}",
        r.log()
    );
    assert!(sb.at(&src).join("setup.sh").is_file());
    // A slot that cannot run goes, and so does an interrupted update's
    // leftover (never a release: update.rs and apply-update refuse the name).
    for out in [
        "   removing release 0.3.17: no vesyl-print binary\n",
        "   removing 0.4.2.staging: left by an interrupted update\n",
    ] {
        assert!(r.stdout.contains(out), "{out}\n{}", r.log());
    }
    assert!(!install.join("releases/0.4.2.staging").exists());

    // Run again: idempotent, and this time the source tree goes.
    let r = sb.setup(&src, &[]);
    assert!(r.ok(), "{}", r.log());
    for out in [
        format!("==> No Tailscale auth key ({src}/keys/tailscale.key) — skip Tailscale"),
        format!("==> Removing factory source tree: {src}"),
    ] {
        assert!(r.stdout.contains(&out), "{out}\n{}", r.log());
    }
    assert!(!sb.at(&src).exists());
    assert_eq!(read(&sb.at("/root/notes.txt")), "keep\n");

    // Tailscale is left alone: never joined again, nothing downloaded.
    let calls = sb.calls();
    assert!(
        !called(&calls, "tailscale up") && !called(&calls, "curl") && !called(&calls, "sudo"),
        "{calls}"
    );

    // The new release is active; the lab build stays for rollback; the
    // Python-era slot is gone; config, credentials and the queue are kept.
    assert_eq!(
        fs::read_link(install.join("current")).unwrap(),
        Path::new("releases").join(VERSION)
    );
    assert!(install.join("releases/0.4.1/vesyl-print").is_file());
    assert!(!install.join("releases/0.3.17").exists());
    assert_eq!(read(&sb.at("/etc/vesyl-print/config.json")), config);
    assert_eq!(
        read(&sb.at("/etc/vesyl-print/credentials.json")),
        "{\"node_id\": \"n1\"}"
    );
    assert!(sb.at("/var/lib/vesyl-print/queue/j1.json").is_file());
    let agent = read(&sb.at("/etc/systemd/system/vesyl-print-agent.service"));
    assert!(
        agent.contains("\nExecStart=/opt/vesyl-print/current/vesyl-print agent\n"),
        "{agent}"
    );
    let wifi = read(&sb.usr_local.join("lib/vesyl-print/wifi-setup"));
    assert!(
        wifi.lines()
            .any(|l| l == "INSTALL_ROOT = Path(\"/opt/vesyl-print\")"),
        "{wifi}"
    );
}

#[test]
#[ignore = "needs root (or a user namespace mapping uid 1000) to chroot and chown"]
fn root_service_account_is_the_sudo_user_else_the_tree_owner() {
    let Some(mut sb) = Sandbox::new() else { return };
    // Extracted in a root shell: the tree belongs to root.
    let src = format!("/root/vesyl-print-{VERSION}");
    release_tree(&sb.at(&src));
    let setup = format!("{src}/setup.sh");
    let run_as = format!("==> Run as user:  {SERVICE_USER}\n");

    // A direct root login or `su -` (no SUDO_USER), or `sudo` run from a
    // root shell (SUDO_USER=root): the tree's owner, root, so setup.sh stops
    // before it changes anything.
    let no_sudo_user: &[(&str, &str)] = &[];
    for env in [no_sudo_user, &[("SUDO_USER", "root")]] {
        let r = sb.exec(env, &[&setup]);
        assert_eq!(r.code, Some(1), "{env:?}: {}", r.log());
        assert!(
            r.stderr
                .contains("the services need a normal account (e.g. vesyl), not 'root'"),
            "{env:?}: {}",
            r.log()
        );
        assert_eq!(sb.calls(), "", "{env:?}");
        assert!(!sb.at("/etc/vesyl-print").exists(), "{env:?}");
        assert!(!sb.at("/opt/vesyl-print").exists(), "{env:?}");
        assert!(!sb.usr_local.join("lib/vesyl-print").exists(), "{env:?}");
    }

    // A `sudo -i` or `sudo -s` shell keeps SUDO_USER: the account that ran
    // sudo becomes the service account, whoever owns the tree.
    let r = sb.exec(
        &[("SUDO_USER", SERVICE_USER), ("SKIP_SOURCE_CLEANUP", "1")],
        &[&setup],
    );
    assert!(r.ok(), "{}", r.log());
    assert!(r.stdout.contains(&run_as), "{}", r.log());
    assert_eq!(owner(&sb.at(&src)), 0);
    for unit in ["vesyl-print-agent.service", "vesyl-print-display.service"] {
        let text = read(&sb.at("/etc/systemd/system").join(unit));
        assert!(text.contains("\nUser=vesyl\n"), "{text}");
    }
    for path in [
        "/etc/vesyl-print",
        "/var/lib/vesyl-print",
        "/opt/vesyl-print",
        "/opt/vesyl-print/releases",
    ] {
        assert_eq!(owner(&sb.at(path)), SERVICE_UID, "{path}");
    }
    let sudoers = read(&sb.at("/etc/sudoers.d/vesyl-print"));
    assert!(
        sudoers
            .lines()
            .any(|l| l == "vesyl ALL=(root) NOPASSWD: /usr/local/lib/vesyl-print/apply-update"),
        "{sudoers}"
    );

    // Once the tree is chowned to the service account, its owner is the
    // account, for `sudo` from a root shell and for a root login alike.
    chown_tree(&sb.at(&src), SERVICE_UID);
    let sudo_from_root: &[(&str, &str)] = &[("SUDO_USER", "root"), ("SKIP_SOURCE_CLEANUP", "1")];
    for env in [sudo_from_root, no_sudo_user] {
        let r = sb.exec(env, &[&setup]);
        assert!(r.ok(), "{env:?}: {}", r.log());
        assert!(r.stdout.contains(&run_as), "{env:?}: {}", r.log());
    }
    // The last run removed the extracted tree.
    assert!(!sb.at(&src).exists());
}

#[test]
#[ignore = "needs root (or a user namespace mapping uid 1000) to chroot and chown"]
fn root_source_cleanup_keeps_a_checkout_and_other_directories() {
    let Some(mut sb) = Sandbox::new() else { return };
    // A checkout with a binary copied in: never deleted, history and all.
    let checkout = format!("/root/vesyl-print-{VERSION}");
    release_tree(&sb.at(&checkout));
    write(
        &sb.at(&checkout).join(".git/HEAD"),
        "ref: refs/heads/main\n",
    );
    let r = sb.setup(&checkout, &[]);
    assert!(r.ok(), "{}", r.log());
    assert!(
        r.stdout.contains(&format!(
            "==> Source cleanup skipped: {checkout} is a git checkout"
        )),
        "{}",
        r.log()
    );
    assert!(sb.at(&checkout).join(".git/HEAD").is_file());

    // A release unpacked straight into a home directory: not ours to delete.
    let home = "/home/vesyl";
    release_tree(&sb.at(home));
    write(&sb.at(home).join(".profile"), "# keep\n");
    let r = sb.setup(home, &[]);
    assert!(r.ok(), "{}", r.log());
    assert!(
        r.stdout.contains(&format!(
            "==> Source cleanup skipped: {home} is not an extracted release (vesyl-print-X.Y.Z)"
        )),
        "{}",
        r.log()
    );
    assert!(sb.at(home).join(".profile").is_file());
    assert!(sb.at(home).join("setup.sh").is_file());
}

#[test]
#[ignore = "needs root (or a user namespace mapping uid 1000) to chroot and chown"]
fn root_refuses_a_helper_without_its_install_root_line_before_any_change() {
    let Some(mut sb) = Sandbox::new() else { return };
    let src = format!("/root/vesyl-print-{VERSION}");
    release_tree(&sb.at(&src));
    make_wifi_helper_pre_install_root(&sb.at(&src));

    let r = sb.setup(&src, &[("INSTALL_ROOT", CUSTOM_ROOT)]);
    assert_eq!(r.code, Some(1), "{}", r.log());
    assert!(
        r.stderr
            .contains(&format!("{src}/scripts/wifi-setup: 0 lines match")),
        "{}",
        r.log()
    );
    // Nothing ran and nothing was written.
    assert_eq!(sb.calls(), "");
    assert!(!sb.at("/etc/vesyl-print").exists());
    assert!(!sb.at(CUSTOM_ROOT).exists());
    assert!(!sb.usr_local.join("lib/vesyl-print").exists());
    assert!(!sb.usr_local.join("bin/vesyl-print").exists());
    assert!(sb.at(&src).join("setup.sh").is_file());
}

#[test]
#[ignore = "needs root (or a user namespace mapping uid 1000) to chroot and chown"]
fn root_install_root_with_trailing_slashes_takes_ota_activations() {
    let Some(mut sb) = Sandbox::new() else { return };
    let src = format!("/root/vesyl-print-{VERSION}");
    release_tree(&sb.at(&src));

    let r = sb.setup(&src, &[("INSTALL_ROOT", "/srv/vesyl-print//")]);
    assert!(r.ok(), "{}", r.log());
    assert!(
        r.stdout.contains(&format!(
            "==> Install root: {CUSTOM_ROOT} (version {VERSION})\n"
        )),
        "{}",
        r.log()
    );
    let apply = read(&sb.usr_local.join("lib/vesyl-print/apply-update"));
    assert!(
        apply
            .lines()
            .any(|l| l == format!("INSTALL_ROOT={CUSTOM_ROOT}")),
        "{apply}"
    );
    let unit = read(&sb.at("/etc/systemd/system/vesyl-print-agent.service"));
    let agent_root = unit
        .lines()
        .find_map(|l| l.strip_prefix("Environment=VESYL_PRINT_INSTALL_ROOT="))
        .unwrap_or_else(|| panic!("no VESYL_PRINT_INSTALL_ROOT\n{unit}"));
    assert_eq!(agent_root, CUSTOM_ROOT);

    // An OTA activation, with the paths the agent joins onto its install
    // root (update.rs: <root>/releases/<version> and <root>/current).
    let root = Path::new(agent_root);
    let release = root.join("releases").join("0.9.2");
    write_exe(
        &sb.at(path_str(&release)).join("vesyl-print"),
        "#!/bin/sh\necho \"vesyl-print 0.9.2\"\n",
    );
    let helper = "/usr/local/lib/vesyl-print/apply-update";
    let current = root.join("current");
    let r = sb.exec(
        &[],
        &[helper, "activate", path_str(&release), path_str(&current)],
    );
    assert!(r.ok(), "{}", r.log());
    assert_eq!(
        fs::read_link(sb.at(path_str(&current))).unwrap(),
        Path::new("releases/0.9.2")
    );
    // The installed helper never activates an interrupted extract's dir.
    let staging = root.join("releases").join("0.9.3.staging");
    write_exe(
        &sb.at(path_str(&staging)).join("vesyl-print"),
        "#!/bin/sh\necho \"vesyl-print 0.9.3\"\n",
    );
    let r = sb.exec(
        &[],
        &[helper, "activate", path_str(&staging), path_str(&current)],
    );
    assert_eq!(r.code, Some(1), "{}", r.log());
    assert!(
        r.stderr.contains("invalid release version: 0.9.3.staging"),
        "{}",
        r.log()
    );
    assert_eq!(
        fs::read_link(sb.at(path_str(&current))).unwrap(),
        Path::new("releases/0.9.2")
    );
}

/// What setup.sh installs in one apt-get call; python3-segno goes alone.
const REQUIRED_PACKAGES: &[&str] = &[
    "cups",
    "poppler-utils",
    "network-manager",
    "rsync",
    "python3",
    "python3-pil",
    "python3-numpy",
    "fonts-dejavu-core",
];

/// The packages the apt-get stub installed.
fn installed_packages(sb: &Sandbox) -> Vec<String> {
    fs::read_to_string(sb.at("/dpkg.installed"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
#[ignore = "needs root (or a user namespace mapping uid 1000) to chroot and chown"]
fn root_a_distro_without_segno_still_gets_the_required_packages() {
    let Some(mut sb) = Sandbox::new() else { return };
    let src = format!("/root/vesyl-print-{VERSION}");
    release_tree(&sb.at(&src));

    let r = sb.setup(&src, &[("APT_MISSING", "python3-segno")]);
    assert!(r.ok(), "{}", r.log());
    assert!(
        r.stderr.contains(
            "WARNING: python3-segno not installed: the Wi-Fi setup screen shows text \
             instead of a QR code"
        ),
        "{}",
        r.log()
    );
    assert!(
        !r.stdout.contains("keeping the installed one"),
        "{}",
        r.log()
    );
    let installed = installed_packages(&sb);
    for pkg in REQUIRED_PACKAGES {
        assert!(installed.iter().any(|p| p == pkg), "{pkg}: {installed:?}");
    }
    assert!(!installed.iter().any(|p| p == "python3-segno"));
    // lpadmin, which the usermod adds the service user to, comes with cups.
    let calls = sb.calls();
    let install = calls
        .lines()
        .position(|l| l.starts_with("apt-get install -y cups "))
        .unwrap_or_else(|| panic!("{calls}"));
    let usermod = calls
        .lines()
        .position(|l| l.starts_with("usermod "))
        .unwrap_or_else(|| panic!("{calls}"));
    assert!(install < usermod, "{calls}");
}

#[test]
#[ignore = "needs root (or a user namespace mapping uid 1000) to chroot and chown"]
fn root_a_missing_required_package_stops_setup_before_anything_else() {
    let Some(mut sb) = Sandbox::new() else { return };
    let src = format!("/root/vesyl-print-{VERSION}");
    release_tree(&sb.at(&src));

    // A fresh device whose apt cannot install cups: no package is
    // installed, so usermod would fail on the missing lpadmin group.
    let r = sb.setup(&src, &[("APT_MISSING", "cups")]);
    assert_eq!(r.code, Some(1), "{}", r.log());
    assert!(
        r.stderr.contains(&format!(
            "!! apt-get could not install required packages: {} (see its errors above)",
            REQUIRED_PACKAGES.join(" ")
        )),
        "{}",
        r.log()
    );
    let calls = sb.calls();
    assert!(!called(&calls, "usermod"), "{calls}");
    assert!(!sb.at("/etc/vesyl-print").exists());
    assert!(!sb.usr_local.join("lib/vesyl-print").exists());
    assert!(!sb.at("/opt/vesyl-print").exists());
}

#[test]
#[ignore = "needs root (or a user namespace mapping uid 1000) to chroot and chown"]
fn root_offline_reprovision_carries_on_with_the_installed_packages() {
    let Some(mut sb) = Sandbox::new() else { return };
    let src = format!("/root/vesyl-print-{VERSION}");
    release_tree(&sb.at(&src));
    // Provisioned before (segno too), now with no route to the mirror.
    let mut before = REQUIRED_PACKAGES.join("\n");
    before.push_str("\npython3-segno\n");
    write(&sb.at("/dpkg.installed"), before);

    let r = sb.setup(&src, &[("APT_OFFLINE", "1")]);
    assert!(r.ok(), "{}", r.log());
    for out in [
        "   (apt-get update failed — continuing with cached lists)\n",
        "   (apt-get install failed — every required package is already installed, continuing)\n",
        "   (apt-get install python3-segno failed — keeping the installed one)\n",
        "==> Done.\n",
    ] {
        assert!(r.stdout.contains(out), "{out}\n{}", r.log());
    }
    // The installed segno still draws the QR code: no warning that it is gone.
    assert!(
        !r.stderr.contains("python3-segno not installed"),
        "{}",
        r.log()
    );
}
