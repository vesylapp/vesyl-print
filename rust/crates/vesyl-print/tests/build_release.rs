//! `scripts/build-release.sh`: one-shot build+sign, the split CI modes
//! (BUILD_ONLY / SIGN_ONLY / VERIFY_ONLY), the allowlisted tarball, a binary
//! that runs on the glibc floor, versions as devices judge them, and
//! manifests that devices accept (`update::verify_manifest`); and the
//! workflows that run it pin their actions and tools.
//!
//! The script runs from a throwaway copy of a small repo with a fake `cargo`
//! (`metadata` reports the target dir as cargo would; `zigbuild` records its
//! environment and arguments and writes a stand-in binary that prints
//! `vesyl-print <version>`), a fake `qemu-aarch64`, a fake `readelf` that
//! lists the glibc versions a test asks for, tripwires that fail the test if
//! Python ever runs, and throwaway Ed25519 keys made with openssl.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;
use vesyl_print::update::{is_version, sha256_file, verify_manifest, ReleaseManifest};
use vesyl_print::JsonObject;

use common::{
    have_tools, openssl_verifies, path_str, repo_root, run, sh_quote, which, write, write_exe,
    KeyPair, Run,
};

const VERSION: &str = "0.9.1";
/// The script signs with this lab key when no key is given.
const LAB_KEY: &str = "/tmp/vesyl-print-update_private.pem";

const FAKE_CARGO: &str = r#"#!/bin/sh
case "$1" in
  metadata)
    td="${CARGO_TARGET_DIR:-$PWD/target}"
    case "$td" in /*) ;; *) td="$PWD/$td" ;; esac
    printf '{"packages": [], "target_directory": "%s"}\n' "$td"
    ;;
  zigbuild)
    env > "$FAKE_CARGO_ENV"
    printf '%s\n' "$@" > "$FAKE_CARGO_ENV.args"
    [ -n "${FAKE_CARGO_NO_OUTPUT:-}" ] && exit 0
    out="${CARGO_TARGET_DIR:-$PWD/target}/aarch64-unknown-linux-gnu/release"
    mkdir -p "$out"
    printf '#!/bin/sh\necho "vesyl-print %s"\n' \
      "${FAKE_CARGO_VERSION:-$VESYL_PRINT_VERSION}" > "$out/vesyl-print"
    chmod 755 "$out/vesyl-print"
    ;;
  *)
    echo "fake cargo: unexpected: $*" >&2
    exit 99
    ;;
esac
"#;

/// `qemu-aarch64 -L <sysroot> <binary> [args]`: run the (shell script) binary.
const FAKE_QEMU: &str = "#!/bin/sh\n[ \"$1\" = \"-L\" ] || exit 98\nshift 2\nexec \"$@\"\n";

/// The glibc versions the real aarch64 release binary needs, as `readelf -V`
/// lists them (cargo-zigbuild for `aarch64-unknown-linux-gnu.2.31`).
const REAL_GLIBC_NEEDS: &str = "2.17 2.18 2.25 2.28 2.29 2.30";

/// `readelf -V <binary>`, in GNU readelf's layout: a version-needs entry for
/// each glibc version in `$FAKE_GLIBC` (unset: [`REAL_GLIBC_NEEDS`]; empty:
/// none).
const FAKE_READELF: &str = r#"#!/bin/sh
[ "$1" = "-V" ] && [ -f "$2" ] || { echo "fake readelf: unexpected: $*" >&2; exit 99; }
echo "Version needs section '.gnu.version_r' contains 2 entries:"
echo "  000000: Version: 1  File: libgcc_s.so.1  Cnt: 1"
echo "  0x0010:   Name: GCC_4.2.0  Flags: none  Version: 2"
echo "  0x0020: Version: 1  File: libc.so.6  Cnt: 6"
n=3
for v in ${FAKE_GLIBC-__REAL__}; do
  echo "  0x00${n}0:   Name: GLIBC_$v  Flags: none  Version: $n"
  n=$((n + 1))
done
"#;

/// Everything SIGN_ONLY / VERIFY_ONLY may run (besides bash builtins).
const SIGN_TOOLS: &[&str] = &[
    "dirname",
    "mkdir",
    "mktemp",
    "rm",
    "mv",
    "sha256sum",
    "base64",
    "tr",
    "openssl",
    "jq",
    "date",
];
/// Build tooling that must never run next to the signing key, and Python,
/// which no mode may run at all.
const TRIPWIRES: &[&str] = &[
    "cargo",
    "cargo-zigbuild",
    "zig",
    "rsync",
    "tar",
    "gzip",
    "install",
    "pip",
    "pip3",
    "uname",
    "readelf",
    "python3",
    "python",
];
const BUILD_TOOLS: &[&str] = &[
    "bash", "rsync", "tar", "gzip", "install", "du", "cut", "grep", "uname", "env",
];

/// Paths in the synthetic repo that the tarball must carry.
const SHIPPED: &[&str] = &[
    "VERSION",
    "main.py",
    "display_status.py",
    "setup.sh",
    "vesyl-print-agent.service",
    "vesyl-print-display.service",
    "README.md",
    "OTA_UPDATES.md",
    "base.jpg",
    "assets/logo.png",
    "assets/test-labels/vesyl-roadrunner-4x6.pdf",
    "assets/test-labels/vesyl-roadrunner-4x6.zpl",
    "assets/test-labels/vesyl-roadrunner-4x6.png",
    "overlays/mhs35.dtbo",
    "scripts/apply-update",
    "scripts/wifi-setup",
    "scripts/bootstrap-fresh-pi.sh",
    "keys/update_public.pem",
];

/// Paths in the synthetic repo that must never reach a device.
const NOT_SHIPPED: &[&str] = &[
    "rust/Cargo.toml",
    "rust/crates/vesyl-print/src/main.rs",
    "tests/test_display_pages.py",
    "tests/fixtures/actioncable/welcome.json",
    ".github/workflows/release.yml",
    ".git/HEAD",
    ".gitignore",
    ".claude/settings.json",
    "keys/README.md",
    "keys/tailscale.key",
    "keys/update_private.pem",
    "requirements.txt",
    "notes.txt",
    "credentials.json",
    ".env",
    "main.pyc",
    "__pycache__/main.cpython-313.pyc",
    "assets/__pycache__/logo.cpython-313.pyc",
    "dist/vesyl-print-0.0.1-linux-aarch64.tar.gz",
];

fn tarball_name(version: &str) -> String {
    format!("vesyl-print-{version}-linux-aarch64.tar.gz")
}

fn manifest_name(version: &str) -> String {
    format!("vesyl-print-{version}.manifest.json")
}

fn artifact_url(version: &str) -> String {
    format!(
        "https://github.com/vesylapp/vesyl-print/releases/download/v{version}/{}",
        tarball_name(version)
    )
}

fn lab_key_present() -> bool {
    let present = Path::new(LAB_KEY).exists();
    if present {
        eprintln!("skipping: {LAB_KEY} would be used as the signing key");
    }
    present
}

/// One tarball member.
struct Member {
    kind: tar::EntryType,
    mode: u32,
    uid: u64,
    gid: u64,
    data: Vec<u8>,
}

struct Fixture {
    _td: tempfile::TempDir,
    tmp: PathBuf,
    repo: PathBuf,
    out: PathBuf,
    fakebin: PathBuf,
    sysroot: PathBuf,
    cargo_env: PathBuf,
    key: KeyPair,
    other: KeyPair,
}

impl Fixture {
    /// A synthetic repo with every runtime file plus junk that must stay out.
    fn new() -> Option<Fixture> {
        Fixture::with_repo(|repo, key| {
            for rel in SHIPPED {
                write(&repo.join(rel), format!("{rel}\n"));
            }
            for rel in ["setup.sh", "scripts/apply-update", "scripts/wifi-setup"] {
                write_exe(&repo.join(rel), &format!("#!/bin/sh\n# {rel}\n"));
            }
            write(&repo.join("VERSION"), format!("{VERSION}\n"));
            fs::copy(&key.public, repo.join("keys/update_public.pem")).unwrap();
            for rel in NOT_SHIPPED {
                write(&repo.join(rel), format!("{rel} must not ship\n"));
            }
            // A stale binary at the repo root: the tarball gets a fresh one.
            write_exe(
                &repo.join("vesyl-print"),
                "#!/bin/sh\necho \"vesyl-print 0.0.0\"\n",
            );
        })
    }

    fn with_repo(populate: impl FnOnce(&Path, &KeyPair)) -> Option<Fixture> {
        let mut tools: Vec<&str> = BUILD_TOOLS.to_vec();
        tools.extend(SIGN_TOOLS);
        if !cfg!(target_os = "linux") || !have_tools(&tools) {
            return None;
        }
        let td = tempfile::tempdir().unwrap();
        let tmp = td.path().canonicalize().unwrap();
        fs::create_dir(tmp.join("keys")).unwrap();
        let key = KeyPair::generate(&tmp.join("keys"), "release");
        let other = KeyPair::generate(&tmp.join("keys"), "other");

        let repo = tmp.join("repo");
        populate(&repo, &key);
        fs::create_dir_all(repo.join("rust")).unwrap();
        let script = repo.join("scripts/build-release.sh");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::copy(repo_root().join("scripts/build-release.sh"), &script).unwrap();

        let fakebin = tmp.join("fakebin");
        write_exe(&fakebin.join("cargo"), FAKE_CARGO);
        write_exe(&fakebin.join("cargo-zigbuild"), "#!/bin/sh\nexit 0\n");
        write_exe(&fakebin.join("qemu-aarch64"), FAKE_QEMU);
        write_exe(
            &fakebin.join("readelf"),
            &FAKE_READELF.replace("__REAL__", REAL_GLIBC_NEEDS),
        );
        let tripwire_log = tmp.join("tripwire.log");
        for python in ["python3", "python"] {
            write_exe(
                &fakebin.join(python),
                &format!(
                    "#!/bin/sh\necho \"{python} $*\" >> {}\nexit 97\n",
                    sh_quote(&tripwire_log)
                ),
            );
        }
        let sysroot = tmp.join("sysroot");
        write(&sysroot.join("lib/ld-linux-aarch64.so.1"), "");
        Some(Fixture {
            _td: td,
            out: tmp.join("dist"),
            cargo_env: tmp.join("cargo.env"),
            repo,
            fakebin,
            sysroot,
            key,
            other,
            tmp,
        })
    }

    fn tarball(&self) -> PathBuf {
        self.out.join(tarball_name(VERSION))
    }

    fn manifest_path(&self) -> PathBuf {
        self.out.join(manifest_name(VERSION))
    }

    // -- running the script ----------------------------------------------------

    /// Minimal on purpose: nothing from the caller's shell (tokens, a real
    /// UPDATE_PRIVATE_KEY, CARGO_TARGET_DIR, ...) reaches the script.
    fn env(&self, path: String, extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = [
            ("PATH", path),
            ("HOME", path_str(&self.tmp).to_string()),
            ("OUT_DIR", path_str(&self.out).to_string()),
            ("AARCH64_SYSROOT", path_str(&self.sysroot).to_string()),
            ("FAKE_CARGO_ENV", path_str(&self.cargo_env).to_string()),
            ("TMPDIR", path_str(&self.tmp).to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        env
    }

    fn script_args<'a>(&'a self, script: &'a str, args: &[&'a str]) -> Vec<&'a str> {
        let mut all = vec![script];
        all.extend_from_slice(args);
        all
    }

    /// The script with the fake toolchain first on the normal PATH.
    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> Run {
        let path = format!(
            "{}:{}",
            path_str(&self.fakebin),
            std::env::var("PATH").unwrap_or_default()
        );
        let script = self.repo.join("scripts/build-release.sh");
        let r = run(
            &which("bash").unwrap(),
            &self.script_args(path_str(&script), args),
            &self.env(path, extra),
        );
        self.assert_no_tripwire();
        r
    }

    /// SIGN_ONLY=1 / VERIFY_ONLY=1 with PATH holding only the signing tools
    /// and tripwires for everything else.
    fn run_sandboxed(&self, mode: &str, args: &[&str], extra: &[(&str, &str)]) -> Run {
        let sandbox = self.tmp.join("signbin");
        if !sandbox.exists() {
            fs::create_dir(&sandbox).unwrap();
            for tool in SIGN_TOOLS {
                symlink(which(tool).unwrap(), sandbox.join(tool)).unwrap();
            }
            for tool in TRIPWIRES {
                write_exe(
                    &sandbox.join(tool),
                    &format!(
                        "#!/bin/sh\necho \"{tool} $*\" >> {}\nexit 97\n",
                        sh_quote(self.tmp.join("tripwire.log"))
                    ),
                );
            }
        }
        let mut extra = extra.to_vec();
        extra.push((mode, "1"));
        let script = self.repo.join("scripts/build-release.sh");
        let r = run(
            &which("bash").unwrap(),
            &self.script_args(path_str(&script), args),
            &self.env(path_str(&sandbox).to_string(), &extra),
        );
        self.assert_no_tripwire();
        r
    }

    fn sign_only(&self, extra: &[(&str, &str)]) -> Run {
        self.run_sandboxed("SIGN_ONLY", &[VERSION], extra)
    }

    fn verify_only(&self, extra: &[(&str, &str)]) -> Run {
        self.run_sandboxed("VERIFY_ONLY", &[VERSION], extra)
    }

    fn verify_only_version(&self, version: &str) -> Run {
        self.run_sandboxed("VERIFY_ONLY", &[version], &[])
    }

    fn assert_no_tripwire(&self) {
        let log = self.tmp.join("tripwire.log");
        assert!(
            !log.exists(),
            "ran a forbidden tool: {}",
            fs::read_to_string(&log).unwrap_or_default()
        );
    }

    fn build_only(&self) {
        let r = self.run(&[VERSION], &[("BUILD_ONLY", "1")]);
        assert!(r.ok(), "{}", r.log());
    }

    fn build_and_sign(&self) {
        let key = path_str(&self.key.private).to_string();
        let r = self.run(&[VERSION], &[("UPDATE_PRIVATE_KEY_FILE", &key)]);
        assert!(r.ok(), "{}", r.log());
    }

    // -- inspecting the output -------------------------------------------------

    fn members(&self) -> BTreeMap<String, Member> {
        let gz = flate2::read::GzDecoder::new(File::open(self.tarball()).unwrap());
        let mut archive = tar::Archive::new(gz);
        let mut out = BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let header = entry.header();
            let (kind, mode) = (header.entry_type(), header.mode().unwrap());
            let (uid, gid) = (header.uid().unwrap(), header.gid().unwrap());
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            out.insert(
                name.trim_end_matches('/').to_string(),
                Member {
                    kind,
                    mode,
                    uid,
                    gid,
                    data,
                },
            );
        }
        out
    }

    /// Regular files in the tarball, relative to its top-level directory.
    fn shipped_files(&self) -> BTreeSet<String> {
        let prefix = format!("vesyl-print-{VERSION}/");
        self.members()
            .into_iter()
            .filter(|(_, m)| m.kind == tar::EntryType::Regular)
            .map(|(name, _)| {
                name.strip_prefix(&prefix)
                    .unwrap_or_else(|| panic!("{name} outside {prefix}"))
                    .to_string()
            })
            .collect()
    }

    fn packaged_binary(&self) -> Vec<u8> {
        self.members()
            .remove(&format!("vesyl-print-{VERSION}/vesyl-print"))
            .expect("binary in tarball")
            .data
    }

    fn manifest_text(&self) -> String {
        fs::read_to_string(self.manifest_path()).unwrap()
    }

    fn manifest(&self) -> JsonObject {
        serde_json::from_str(&self.manifest_text()).unwrap()
    }

    fn write_manifest(&self, m: &JsonObject) {
        fs::write(
            self.manifest_path(),
            serde_json::to_string_pretty(m).unwrap(),
        )
        .unwrap();
    }

    fn assert_manifest_matches_tarball(&self) -> JsonObject {
        let m = self.manifest();
        assert_eq!(m["version"], VERSION);
        assert_eq!(
            m["artifact_sha256"],
            sha256_file(&self.tarball()).unwrap().as_str()
        );
        assert_eq!(m["artifact_url"], artifact_url(VERSION).as_str());
        m
    }

    /// The device-side check (update.rs) accepts the manifest with `key`, and
    /// openssl agrees on the bytes the device rebuilds.
    fn assert_device_accepts(&self, m: &JsonObject, key: &KeyPair) {
        let manifest = ReleaseManifest::from_dict(m).expect("manifest parses on the device");
        verify_manifest(&manifest, Some(&key.public_pem()), true)
            .unwrap_or_else(|e| panic!("device rejects the manifest: {e}"));
        let sig = m["signature"].as_str().expect("signed");
        assert!(openssl_verifies(
            &key.public,
            &manifest.canonical_bytes(),
            sig,
            &self.tmp
        ));
    }

    fn device_rejects(&self, m: &JsonObject, key: &KeyPair) -> bool {
        let manifest = ReleaseManifest::from_dict(m).expect("manifest parses on the device");
        verify_manifest(&manifest, Some(&key.public_pem()), true).is_err()
    }

    /// A variable from the environment the (fake) cargo build ran with.
    fn cargo_saw(&self, name: &str) -> Option<String> {
        fs::read_to_string(&self.cargo_env)
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name}=")).map(str::to_string))
    }

    /// The arguments the (fake) cargo build ran with.
    fn cargo_args(&self) -> Vec<String> {
        let args = format!("{}.args", path_str(&self.cargo_env));
        fs::read_to_string(args)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn assert_key_never_reached_cargo(&self) {
        // Report variable names / PEM markers only, never values.
        let leaked: Vec<String> = fs::read_to_string(&self.cargo_env)
            .unwrap()
            .lines()
            .filter(|l| l.contains("UPDATE_PRIVATE_KEY") || l.contains("PRIVATE KEY"))
            .map(|l| l.split('=').next().unwrap_or_default().to_string())
            .collect();
        assert!(
            leaked.is_empty(),
            "the signing key reached the cargo build: {leaked:?}"
        );
    }

    fn plant_stale_binary(&self, target_dir: &Path) -> PathBuf {
        let stale = target_dir.join("aarch64-unknown-linux-gnu/release/vesyl-print");
        write_exe(&stale, "#!/bin/sh\necho \"vesyl-print 0.0.0\"\n");
        stale
    }
}

macro_rules! fixture {
    () => {
        match Fixture::new() {
            Some(f) => f,
            None => return,
        }
    };
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

// --- one-shot build + sign ------------------------------------------------------

#[test]
fn builds_and_signs_in_one_go() {
    let f = fixture!();
    let r = f.run(&[VERSION], &[("UPDATE_PRIVATE_KEY", &f.key.private_pem())]);
    assert!(r.ok(), "{}", r.log());
    let m = f.assert_manifest_matches_tarball();
    f.assert_device_accepts(&m, &f.key);
    assert!(contains(
        &f.packaged_binary(),
        &format!("vesyl-print {VERSION}")
    ));
    assert!(
        r.stdout
            .contains(&format!("version: vesyl-print {VERSION}")),
        "{}",
        r.log()
    );
    assert!(
        r.stdout
            .contains("signature verifies against keys/update_public.pem"),
        "{}",
        r.log()
    );
    f.assert_key_never_reached_cargo();
}

#[test]
fn tarball_holds_exactly_the_runtime_files() {
    let f = fixture!();
    f.build_only();
    let mut want: BTreeSet<String> = SHIPPED.iter().map(|s| s.to_string()).collect();
    want.insert("vesyl-print".into());
    assert_eq!(f.shipped_files(), want);
    let binary = f.packaged_binary();
    assert!(contains(&binary, &format!("vesyl-print {VERSION}")));
    assert!(
        !contains(&binary, "vesyl-print 0.0.0"),
        "shipped the stale root binary"
    );

    let members = f.members();
    let top = format!("vesyl-print-{VERSION}");
    for (name, m) in &members {
        assert!(
            name == &top || name.starts_with(&format!("{top}/")),
            "{name}"
        );
        assert_eq!(
            (m.uid, m.gid),
            (0, 0),
            "{name} not root-owned in the archive"
        );
    }
    for exe in [
        "vesyl-print",
        "setup.sh",
        "scripts/apply-update",
        "scripts/wifi-setup",
    ] {
        let m = &members[&format!("{top}/{exe}")];
        assert_eq!(m.mode & 0o777, 0o755, "{exe} mode {:o}", m.mode);
    }
    assert_eq!(
        members[&format!("{top}/VERSION")].data,
        format!("{VERSION}\n").as_bytes()
    );
}

#[test]
fn key_file_signs() {
    let f = fixture!();
    f.build_and_sign();
    let m = f.assert_manifest_matches_tarball();
    f.assert_device_accepts(&m, &f.key);
    f.assert_key_never_reached_cargo();
}

#[test]
fn version_defaults_to_version_file() {
    let f = fixture!();
    let key = path_str(&f.key.private).to_string();
    let r = f.run(&[], &[("UPDATE_PRIVATE_KEY_FILE", &key)]);
    assert!(r.ok(), "{}", r.log());
    assert_eq!(f.manifest()["version"], VERSION);
}

#[test]
fn missing_key_file_is_an_error() {
    let f = fixture!();
    let missing = path_str(&f.tmp.join("nope.pem")).to_string();
    let r = f.run(&[VERSION], &[("UPDATE_PRIVATE_KEY_FILE", &missing)]);
    assert!(!r.ok());
    assert!(
        r.stderr.contains("UPDATE_PRIVATE_KEY_FILE not found"),
        "{}",
        r.log()
    );
    assert!(!f.cargo_env.exists(), "built before checking the key");
    assert!(!f.manifest_path().exists());
}

#[test]
fn no_key_writes_unsigned_manifest_with_warning() {
    let f = fixture!();
    if lab_key_present() {
        return;
    }
    let r = f.run(&[VERSION], &[]);
    assert!(r.ok(), "{}", r.log());
    assert!(r.stderr.contains("manifest unsigned"), "{}", r.log());
    assert!(!f
        .assert_manifest_matches_tarball()
        .contains_key("signature"));
}

#[test]
fn lab_key_mismatch_only_warns() {
    let f = fixture!();
    fs::copy(&f.other.public, f.repo.join("keys/update_public.pem")).unwrap();
    let key = path_str(&f.key.private).to_string();
    let r = f.run(&[VERSION], &[("UPDATE_PRIVATE_KEY_FILE", &key)]);
    assert!(r.ok(), "{}", r.log());
    assert!(
        r.stderr
            .contains("does not verify against keys/update_public.pem"),
        "{}",
        r.log()
    );
    let m = f.manifest();
    f.assert_device_accepts(&m, &f.key);
    assert!(f.device_rejects(&m, &f.other));
}

#[test]
fn every_release_ships_the_binary() {
    let f = fixture!();
    let r = f.run(
        &[VERSION],
        &[("SKIP_RUST_BINARY", "1"), ("BUILD_ONLY", "1")],
    );
    assert!(!r.ok());
    assert!(
        r.stderr.contains("SKIP_RUST_BINARY is no longer supported"),
        "{}",
        r.log()
    );
    assert!(!f.cargo_env.exists());
    assert!(!f.tarball().exists());
}

#[test]
fn missing_runtime_file_fails_the_build() {
    for rel in [
        "main.py",
        "scripts/apply-update",
        "assets/test-labels/vesyl-roadrunner-4x6.zpl",
        "vesyl-print-agent.service",
    ] {
        let f = fixture!();
        fs::remove_file(f.repo.join(rel)).unwrap();
        let r = f.run(&[VERSION], &[("BUILD_ONLY", "1")]);
        assert!(!r.ok(), "{rel}: {}", r.log());
        assert!(
            r.stderr.contains(&format!("release tree is missing {rel}")),
            "{}",
            r.log()
        );
        assert!(!f.tarball().exists());
    }
}

#[test]
fn real_tree_ships_the_lcd_assets_and_provisioning_files() {
    let real = repo_root();
    let f = match Fixture::with_repo(|repo, _| copy_tree(&real, repo, &real)) {
        Some(f) => f,
        None => return,
    };
    f.build_only();
    let shipped = f.shipped_files();

    let mut want: Vec<String> = [
        "vesyl-print",
        "VERSION",
        "setup.sh",
        "vesyl-print-agent.service",
        "vesyl-print-display.service",
        "scripts/apply-update",
        "scripts/wifi-setup",
        "scripts/bootstrap-fresh-pi.sh",
        "keys/update_public.pem",
        "base.jpg",
        "overlays/mhs35.dtbo",
        "main.py",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for entry in fs::read_dir(&f.repo).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name.ends_with(".py") {
            want.push(name); // the Python LCD display
        }
    }
    let mut assets = Vec::new();
    list_files(&f.repo.join("assets"), &f.repo, &mut assets);
    assert!(
        assets.contains(&"assets/test-labels/vesyl-roadrunner-4x6.pdf".to_string()),
        "{assets:?}"
    );
    want.extend(assets);
    for path in &want {
        assert!(shipped.contains(path), "{path} missing from the tarball");
    }
    for path in &shipped {
        let dev_only = ["rust/", "tests/", ".github/", ".git/"]
            .iter()
            .any(|p| path.starts_with(p));
        assert!(!dev_only, "{path} shipped");
        assert!(
            !path.starts_with("keys/") || path == "keys/update_public.pem",
            "{path} shipped"
        );
        assert!(
            !["requirements.txt", "scripts/build-release.sh", ".gitignore"]
                .contains(&path.as_str()),
            "{path} shipped"
        );
    }
}

/// Copy the repository for packaging, without VCS data, build output or
/// local caches (none of which a release ships).
fn copy_tree(src: &Path, dest: &Path, root: &Path) {
    const SKIP: &[&str] = &[
        ".git",
        ".claude",
        ".venv",
        "venv",
        ".pytest_cache",
        "__pycache__",
        "node_modules",
        "target",
        "dist",
    ];
    fs::create_dir_all(dest).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let path = entry.path();
        let skip = SKIP.iter().any(|n| name == *n) || (src == root && name == "vesyl-print");
        if skip {
            continue;
        }
        let meta = fs::symlink_metadata(&path).unwrap();
        let to = dest.join(&name);
        if meta.file_type().is_symlink() {
            symlink(fs::read_link(&path).unwrap(), &to).unwrap();
        } else if meta.is_dir() {
            copy_tree(&path, &to, root);
        } else {
            fs::copy(&path, &to).unwrap();
        }
    }
}

fn list_files(dir: &Path, base: &Path, out: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().is_some_and(|n| n == "__pycache__") {
            continue;
        }
        if path.is_dir() {
            list_files(&path, base, out);
        } else {
            out.push(
                path.strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
}

// --- manifest contents ----------------------------------------------------------

#[test]
fn manifest_fields() {
    let f = fixture!();
    f.build_and_sign();
    let m = f.manifest();
    let keys: Vec<&str> = m.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "artifact_sha256",
            "artifact_url",
            "channel",
            "min_agent_version",
            "released_at",
            "signature",
            "version"
        ]
    );
    assert_eq!(m["channel"], "stable");
    assert_eq!(m["min_agent_version"], "0.4.0");
    let released = m["released_at"].as_str().unwrap();
    let shape = regex::Regex::new(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\+00:00$").unwrap();
    assert!(shape.is_match(released), "{released}");
    // Written for people too: indented, one trailing newline.
    let text = f.manifest_text();
    assert!(text.starts_with("{\n  \"version\": "), "{text}");
    assert!(text.ends_with("}\n") && !text.ends_with("\n\n"), "{text:?}");
}

#[test]
fn channel_and_min_agent_version_overrides() {
    let f = fixture!();
    let key = path_str(&f.key.private).to_string();
    let r = f.run(
        &[VERSION],
        &[
            ("UPDATE_PRIVATE_KEY_FILE", &key),
            ("RELEASE_CHANNEL", "beta"),
            ("MIN_AGENT_VERSION", "0.9.0"),
        ],
    );
    assert!(r.ok(), "{}", r.log());
    let m = f.manifest();
    assert_eq!(m["channel"], "beta");
    assert_eq!(m["min_agent_version"], "0.9.0");
    f.assert_device_accepts(&m, &f.key);
}

#[test]
fn release_below_min_agent_version_is_refused() {
    let f = fixture!();
    for (version, min) in [
        ("0.3.18", None),
        ("0.3.99", Some("0.4.0")),
        ("1.2.3", Some("1.3.0")),
    ] {
        let mut extra = vec![("BUILD_ONLY", "1")];
        if let Some(min) = min {
            extra.push(("MIN_AGENT_VERSION", min));
        }
        let r = f.run(&[version], &extra);
        assert!(!r.ok(), "{version}: {}", r.log());
        assert!(
            r.stderr.contains("is below MIN_AGENT_VERSION"),
            "{}",
            r.log()
        );
        assert!(
            !f.cargo_env.exists(),
            "built before checking the version floor"
        );
    }
    let r = f.run(
        &[VERSION],
        &[("BUILD_ONLY", "1"), ("MIN_AGENT_VERSION", "latest")],
    );
    assert!(
        r.stderr.contains("invalid MIN_AGENT_VERSION"),
        "{}",
        r.log()
    );
    // Numeric, not lexicographic; suffixes ignored; equal is fine.
    for (version, min) in [
        ("0.10.0", "0.4.0"),
        ("1.0.0", "0.4.0"),
        ("0.4.0-rc.1", "0.4.0"),
        ("0.3.18", "0.3.18"),
    ] {
        let r = f.run(
            &[version],
            &[("BUILD_ONLY", "1"), ("MIN_AGENT_VERSION", min)],
        );
        assert!(r.ok(), "{version} >= {min}: {}", r.log());
    }
}

/// The refusal never suggests bumping `VERSION`: bumped before the tag, a
/// build numbered like the release would block it (OTA_UPDATES.md §4.8).
/// A checkout's VERSION below the floor points at the argument instead.
#[test]
fn version_below_the_floor_says_how_to_number_the_build() {
    let f = fixture!();
    write(&f.repo.join("VERSION"), "0.3.17\n");
    let r = f.run(&[], &[("BUILD_ONLY", "1")]);
    assert_eq!(r.code, Some(1), "{}", r.log());
    for want in [
        "version 0.3.17 from the VERSION file is below MIN_AGENT_VERSION 0.4.0",
        "Give the version to build as the argument (see OTA_UPDATES.md §4.8)",
    ] {
        assert!(r.stderr.contains(want), "{want}\n{}", r.log());
    }
    let r2 = f.run(&["0.3.18"], &[("BUILD_ONLY", "1")]);
    assert_eq!(r2.code, Some(1), "{}", r2.log());
    assert!(
        r2.stderr
            .contains("version 0.3.18 is below MIN_AGENT_VERSION 0.4.0")
            && r2.stderr.contains("OTA_UPDATES.md §4.8")
            && !r2.stderr.contains("VERSION file"),
        "{}",
        r2.log()
    );
    for r in [&r, &r2] {
        assert!(!r.stderr.contains("bump VERSION"), "{}", r.log());
    }
    assert!(!f.cargo_env.exists(), "built below the floor");
}

/// Names the version check must judge as update.rs `is_version` does
/// (ASCII only: the script runs in the C locale).
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
];

/// The script accepts exactly the versions devices accept: a release named
/// `X.Y.Z.staging` would be rejected by every device's manifest check
/// (`ReleaseManifest::from_dict`), and its slot would be another
/// version's staging dir.
#[test]
fn version_check_matches_update_rs_is_version() {
    let f = fixture!();
    for &v in VERSION_CANDIDATES {
        // VERIFY_ONLY with nothing built: a valid version gets as far as
        // looking for its tarball.
        let r = f.verify_only_version(v);
        assert_eq!(r.code, Some(1), "{v:?}: {}", r.log());
        let refused = r.stderr.contains(&format!("invalid version: {v}\n"));
        let looked = r.stderr.contains("VERIFY_ONLY=1: missing");
        assert!(refused != looked, "{v:?}: {}", r.log());
        assert_eq!(refused, !is_version(v), "{v:?}: {}", r.log());
    }
    let r = f.run(
        &[VERSION],
        &[("BUILD_ONLY", "1"), ("MIN_AGENT_VERSION", "0.4.0.staging")],
    );
    assert!(
        r.stderr
            .contains("invalid MIN_AGENT_VERSION: 0.4.0.staging"),
        "{}",
        r.log()
    );
    assert!(!f.cargo_env.exists());
}

/// Non-ASCII and escapes in a signed field: jq's canonical bytes (signer)
/// must equal update.rs ReleaseManifest::canonical_bytes (device).
const TRICKY: &str = "Caf\u{e9} \u{2014} \u{201c}quotes\u{201d} \u{1f680} \\ \" / \
                      tab\t newline\n cr\r del\u{7f} us\u{1f} ls\u{2028} \u{feff}end";

#[test]
fn non_ascii_changelog_verifies_on_the_device() {
    let f = fixture!();
    f.build_only();
    let key = path_str(&f.key.private).to_string();
    let r = f.sign_only(&[
        ("UPDATE_PRIVATE_KEY_FILE", &key),
        ("RELEASE_CHANGELOG", TRICKY),
    ]);
    assert!(r.ok(), "{}", r.log());
    let m = f.manifest();
    assert_eq!(m["changelog"], TRICKY);
    f.assert_device_accepts(&m, &f.key);
    let canonical = ReleaseManifest::from_dict(&m).unwrap().canonical_bytes();
    assert!(canonical.is_ascii());
    for escaped in [
        r"Caf\u00e9",
        r"\ud83d\ude80",
        r"\u007f",
        r"\u001f",
        r#"\\ \" /"#,
        r"\t",
        r"\n",
    ] {
        assert!(
            contains(&canonical, escaped),
            "{escaped} not in canonical form"
        );
    }
    // The manifest file itself is ASCII too.
    assert!(f.manifest_text().is_ascii());
    let r = f.verify_only(&[]);
    assert!(r.ok(), "{}", r.log());
}

// --- the packaged binary is the one this run built ------------------------------

#[test]
fn redirected_target_dir_ships_fresh_binary() {
    let f = fixture!();
    f.plant_stale_binary(&f.repo.join("rust/target"));
    let elsewhere = path_str(&f.tmp.join("elsewhere")).to_string();
    let key = path_str(&f.key.private).to_string();
    let r = f.run(
        &[VERSION],
        &[
            ("CARGO_TARGET_DIR", &elsewhere),
            ("UPDATE_PRIVATE_KEY_FILE", &key),
        ],
    );
    assert!(r.ok(), "{}", r.log());
    let binary = f.packaged_binary();
    assert!(contains(&binary, &format!("vesyl-print {VERSION}")));
    assert!(!contains(&binary, "vesyl-print 0.0.0"));
    assert_eq!(f.cargo_saw("CARGO_TARGET_DIR"), Some(elsewhere));
}

#[test]
fn previous_artifact_deleted_before_build() {
    let f = fixture!();
    let stale = f.plant_stale_binary(&f.repo.join("rust/target"));
    let key = path_str(&f.key.private).to_string();
    let r = f.run(
        &[VERSION],
        &[
            ("FAKE_CARGO_NO_OUTPUT", "1"),
            ("UPDATE_PRIVATE_KEY_FILE", &key),
        ],
    );
    assert!(!r.ok());
    assert!(r.stderr.contains("cargo did not produce"), "{}", r.log());
    assert!(!stale.exists());
    assert!(!f.tarball().exists());
    assert!(!f.manifest_path().exists());
}

#[test]
fn binary_must_report_release_version() {
    let f = fixture!();
    let key = path_str(&f.key.private).to_string();
    let r = f.run(
        &[VERSION],
        &[
            ("FAKE_CARGO_VERSION", "0.0.0"),
            ("UPDATE_PRIVATE_KEY_FILE", &key),
        ],
    );
    assert!(!r.ok());
    assert!(
        r.stderr.contains(&format!(
            "reports 'vesyl-print 0.0.0', expected 'vesyl-print {VERSION}'"
        )),
        "{}",
        r.log()
    );
    assert!(!f.tarball().exists());
    assert!(!f.manifest_path().exists());
}

#[test]
fn without_qemu_the_version_string_must_be_in_the_binary() {
    if std::env::consts::ARCH == "aarch64" {
        return; // aarch64 hosts run the binary
    }
    let f = fixture!();
    let no_sysroot = path_str(&f.tmp.join("no-sysroot")).to_string();
    let key = path_str(&f.key.private).to_string();
    let r = f.run(
        &[VERSION],
        &[
            ("AARCH64_SYSROOT", &no_sysroot),
            ("FAKE_CARGO_VERSION", "0.0.0"),
            ("UPDATE_PRIVATE_KEY_FILE", &key),
        ],
    );
    assert!(!r.ok());
    assert!(
        r.stderr
            .contains(&format!("does not contain version {VERSION}")),
        "{}",
        r.log()
    );
    let r = f.run(
        &[VERSION],
        &[
            ("AARCH64_SYSROOT", &no_sysroot),
            ("UPDATE_PRIVATE_KEY_FILE", &key),
        ],
    );
    assert!(r.ok(), "{}", r.log());
    assert!(
        r.stdout.contains(&format!("contains {VERSION} (not run")),
        "{}",
        r.log()
    );
}

/// CI installs qemu-user and the aarch64 sysroot: there, a binary the
/// script could not run fails the build instead of passing on a grep.
#[test]
fn in_ci_the_packaged_binary_must_run() {
    if std::env::consts::ARCH == "aarch64" {
        return; // aarch64 hosts run the binary
    }
    let f = fixture!();
    let no_sysroot = path_str(&f.tmp.join("no-sysroot")).to_string();
    let r = f.run(
        &[VERSION],
        &[
            ("BUILD_ONLY", "1"),
            ("CI", "true"),
            ("AARCH64_SYSROOT", &no_sysroot),
        ],
    );
    assert_eq!(r.code, Some(1), "{}", r.log());
    assert!(
        r.stderr
            .contains("cannot run the packaged binary: CI needs qemu-aarch64"),
        "{}",
        r.log()
    );
    assert!(!f.tarball().exists());
    let r = f.run(&[VERSION], &[("BUILD_ONLY", "1"), ("CI", "true")]);
    assert!(r.ok(), "{}", r.log());
    assert!(
        r.stdout
            .contains(&format!("version: vesyl-print {VERSION}\n")),
        "{}",
        r.log()
    );
}

/// The binary is linked for the glibc floor, and a binary that needs a newer
/// glibc symbol (which the build host's newer sysroot would still run) is
/// never packaged: a device on the floor could not start it.
#[test]
fn binary_must_not_need_a_glibc_newer_than_the_floor() {
    let f = fixture!();
    f.build_only();
    let args = f.cargo_args();
    assert!(
        args.windows(2)
            .any(|w| w == ["--target", "aarch64-unknown-linux-gnu.2.31"]),
        "{args:?}"
    );
    for (needs, newest) in [
        (REAL_GLIBC_NEEDS, "2.30"),
        ("2.17 2.31", "2.31"),
        // Numeric, not lexicographic: 2.4 is older than 2.31.
        ("2.17 2.4 2.31 2.9", "2.31"),
        ("2.3.4 2.17", "2.17"),
    ] {
        let r = f.run(&[VERSION], &[("BUILD_ONLY", "1"), ("FAKE_GLIBC", needs)]);
        assert!(r.ok(), "{needs}: {}", r.log());
        assert!(
            r.stdout
                .contains(&format!("glibc:   needs at most {newest} (floor 2.31)\n")),
            "{needs}: {}",
            r.log()
        );
    }
    for (needs, want) in [
        (
            "2.17 2.32",
            "needs glibc 2.32, newer than the 2.31 floor (not built for aarch64-unknown-linux-gnu.2.31?)",
        ),
        ("2.17 2.34 2.28", "needs glibc 2.34, newer than the 2.31 floor"),
        ("2.17 3.0", "needs glibc 3.0, newer than the 2.31 floor"),
        ("", "needs no GLIBC_ symbol version"),
    ] {
        let r = f.run(
            &[VERSION],
            &[("BUILD_ONLY", "1"), ("FAKE_GLIBC", needs)],
        );
        assert_eq!(r.code, Some(1), "{needs:?}: {}", r.log());
        assert!(r.stderr.contains(want), "{needs:?}: {}", r.log());
        assert!(!f.tarball().exists(), "{needs:?}: packaged anyway");
    }
}

// --- split CI modes ---------------------------------------------------------------

#[test]
fn build_only_then_sign_only_then_verify_only() {
    let f = fixture!();
    let r = f.run(
        &[VERSION],
        &[
            ("BUILD_ONLY", "1"),
            ("UPDATE_PRIVATE_KEY", &f.key.private_pem()),
        ],
    );
    assert!(r.ok(), "{}", r.log());
    assert!(
        r.stdout.contains("ignoring UPDATE_PRIVATE_KEY"),
        "{}",
        r.log()
    );
    assert!(f.tarball().is_file());
    assert!(!f.manifest_path().exists());
    f.assert_key_never_reached_cargo();
    let built_sha = sha256_file(&f.tarball()).unwrap();

    let key = path_str(&f.key.private).to_string();
    let public = path_str(&f.key.public).to_string();
    let r = f.sign_only(&[
        ("UPDATE_PRIVATE_KEY_FILE", &key),
        ("UPDATE_PUBLIC_KEY_FILE", &public),
    ]);
    assert!(r.ok(), "{}", r.log());
    assert_eq!(sha256_file(&f.tarball()).unwrap(), built_sha);
    let m = f.assert_manifest_matches_tarball();
    assert_eq!(m["artifact_sha256"], built_sha.as_str());
    f.assert_device_accepts(&m, &f.key);
    assert!(
        r.stdout
            .contains(&format!("signature verifies against {public}")),
        "{}",
        r.log()
    );

    let manifest_before = f.manifest_text();
    let r = f.verify_only(&[("UPDATE_PUBLIC_KEY_FILE", &public)]);
    assert!(r.ok(), "{}", r.log());
    assert!(r.stdout.contains("verifies against"), "{}", r.log());
    assert_eq!(
        f.manifest_text(),
        manifest_before,
        "VERIFY_ONLY changed the manifest"
    );
    assert_eq!(sha256_file(&f.tarball()).unwrap(), built_sha);
}

#[test]
fn sign_only_key_from_env() {
    let f = fixture!();
    f.build_only();
    let r = f.sign_only(&[("UPDATE_PRIVATE_KEY", &f.key.private_pem())]);
    assert!(r.ok(), "{}", r.log());
    let m = f.assert_manifest_matches_tarball();
    f.assert_device_accepts(&m, &f.key);
}

#[test]
fn build_only_removes_stale_manifest() {
    let f = fixture!();
    write(
        &f.manifest_path(),
        "{\"version\": \"0.9.1\", \"artifact_sha256\": \"old\"}\n",
    );
    f.build_only();
    assert!(!f.manifest_path().exists());
}

#[test]
fn sign_only_needs_a_key() {
    let f = fixture!();
    if lab_key_present() {
        return;
    }
    f.build_only();
    let r = f.sign_only(&[]);
    assert!(!r.ok());
    assert!(
        r.stderr.contains("SIGN_ONLY=1 needs UPDATE_PRIVATE_KEY"),
        "{}",
        r.log()
    );
    assert!(!f.manifest_path().exists());
}

#[test]
fn sign_only_needs_the_tarball() {
    let f = fixture!();
    let key = path_str(&f.key.private).to_string();
    let r = f.sign_only(&[("UPDATE_PRIVATE_KEY_FILE", &key)]);
    assert!(!r.ok());
    assert!(r.stderr.contains("missing"), "{}", r.log());
    assert!(!f.manifest_path().exists());
}

#[test]
fn sign_only_refuses_key_that_devices_would_reject() {
    let f = fixture!();
    f.build_only();
    write(&f.manifest_path(), "{}\n"); // left over from an earlier run
    let key = path_str(&f.key.private).to_string();
    let other = path_str(&f.other.public).to_string();
    let r = f.sign_only(&[
        ("UPDATE_PRIVATE_KEY_FILE", &key),
        ("UPDATE_PUBLIC_KEY_FILE", &other),
    ]);
    assert!(!r.ok());
    assert!(r.stderr.contains("does not verify against"), "{}", r.log());
    assert!(!f.manifest_path().exists());
    assert!(!f
        .out
        .join(format!("{}.partial", manifest_name(VERSION)))
        .exists());
}

#[test]
fn sign_only_checks_the_public_key_path_first() {
    let f = fixture!();
    f.build_only();
    let key = path_str(&f.key.private).to_string();
    let r = f.sign_only(&[
        ("UPDATE_PRIVATE_KEY_FILE", &key),
        ("UPDATE_PUBLIC_KEY_FILE", "/nonexistent/update_public.pem"),
    ]);
    assert!(!r.ok());
    assert!(
        r.stderr.contains("UPDATE_PUBLIC_KEY_FILE not found"),
        "{}",
        r.log()
    );
    assert!(!f.manifest_path().exists());
}

#[test]
fn modes_are_exclusive() {
    let f = fixture!();
    for pair in [
        [("BUILD_ONLY", "1"), ("SIGN_ONLY", "1")],
        [("BUILD_ONLY", "1"), ("VERIFY_ONLY", "1")],
        [("SIGN_ONLY", "1"), ("VERIFY_ONLY", "1")],
    ] {
        let r = f.run(&[VERSION], &pair);
        assert!(!r.ok());
        assert!(r.stderr.contains("mutually exclusive"), "{}", r.log());
    }
    assert!(!f.cargo_env.exists());
}

// --- VERIFY_ONLY: the publish job's check -----------------------------------------

/// One way a release can be wrong at publish time.
#[derive(Default)]
struct Tampered<'a> {
    /// Expected in the refusal message.
    want: &'a str,
    /// Manifest to publish instead of the signed one.
    manifest: Option<JsonObject>,
    /// Append a byte to the tarball after signing.
    grow_tarball: bool,
    /// Extra environment for the check.
    env: Vec<(&'a str, &'a str)>,
}

#[test]
fn verify_only_refuses_what_devices_would_refuse() {
    let f = fixture!();
    f.build_and_sign();
    let good_manifest = f.manifest();
    let good_tarball = fs::read(f.tarball()).unwrap();
    let restore = |f: &Fixture| {
        f.write_manifest(&good_manifest);
        fs::write(f.tarball(), &good_tarball).unwrap();
    };
    let r = f.verify_only(&[]);
    assert!(r.ok(), "{}", r.log());

    let tamper = |field: &str, value: Option<&str>| {
        let mut m = good_manifest.clone();
        match value {
            Some(v) => m.insert(field.into(), Value::String(v.into())),
            None => m.remove(field),
        };
        m
    };
    let other_key = path_str(&f.other.public).to_string();
    let cases = [
        Tampered {
            want: "version '0.9.2' != '0.9.1'",
            manifest: Some(tamper("version", Some("0.9.2"))),
            ..Tampered::default()
        },
        Tampered {
            want: "artifact_sha256 does not match",
            grow_tarball: true,
            ..Tampered::default()
        },
        Tampered {
            want: "artifact_url",
            env: vec![("GITHUB_REPOSITORY", "someone/else")],
            ..Tampered::default()
        },
        Tampered {
            want: "manifest is not signed",
            manifest: Some(tamper("signature", None)),
            ..Tampered::default()
        },
        Tampered {
            want: "signature does not verify",
            manifest: Some(tamper("channel", Some("beta"))),
            ..Tampered::default()
        },
        Tampered {
            want: "signature does not verify",
            manifest: Some(tamper("changelog", Some("extra"))),
            ..Tampered::default()
        },
        Tampered {
            want: "signature does not verify",
            env: vec![("UPDATE_PUBLIC_KEY_FILE", other_key.as_str())],
            ..Tampered::default()
        },
    ];
    for case in cases {
        let want = case.want;
        restore(&f);
        if let Some(m) = &case.manifest {
            f.write_manifest(m);
            if m.contains_key("signature") && m["version"] == VERSION {
                assert!(
                    f.device_rejects(m, &f.key),
                    "{want}: the device would accept it"
                );
            }
        }
        if case.grow_tarball {
            let mut bytes = good_tarball.clone();
            bytes.push(0);
            fs::write(f.tarball(), bytes).unwrap();
        }
        let before = (f.manifest_text(), fs::read(f.tarball()).unwrap());
        let r = f.verify_only(&case.env);
        assert!(!r.ok(), "{want}: accepted\n{}", r.log());
        assert!(r.stderr.contains("refusing to publish"), "{}", r.log());
        assert!(r.stderr.contains(want), "{want}: {}", r.log());
        assert_eq!(
            (f.manifest_text(), fs::read(f.tarball()).unwrap()),
            before,
            "VERIFY_ONLY changed its inputs"
        );
    }
}

#[test]
fn verify_only_uses_the_device_canonical_form() {
    // A manifest the signer never wrote: keys out of order, an unknown field,
    // a null, raw UTF-8. Signed over update.rs's canonical bytes, it must pass
    // the publish check; jq and the device must agree byte for byte.
    let f = fixture!();
    f.build_only();
    let sha = sha256_file(&f.tarball()).unwrap();
    let changelog = serde_json::to_string(TRICKY).unwrap();
    let text = |signature: &str| {
        format!(
            "{{\"zz_note\": \"x\", \"released_at\": null, \"signature\": \"{signature}\", \
             \"changelog\": {changelog}, \"artifact_sha256\": \"{sha}\", \
             \"version\": \"{VERSION}\", \"artifact_url\": \"{}\", \
             \"channel\": \"stable\", \"Min\": \"\u{e9}\", \"min_agent_version\": \"0.4.0\"}}",
            artifact_url(VERSION)
        )
    };
    let unsigned: JsonObject = serde_json::from_str(&text("")).unwrap();
    let canonical = ReleaseManifest::from_dict(&unsigned)
        .unwrap()
        .canonical_bytes();
    let signature = f.key.sign_b64(&canonical, &f.tmp);
    fs::write(f.manifest_path(), text(&signature)).unwrap();

    let public = path_str(&f.key.public).to_string();
    let r = f.verify_only(&[("UPDATE_PUBLIC_KEY_FILE", &public)]);
    assert!(r.ok(), "{}", r.log());
    f.assert_device_accepts(&serde_json::from_str(&text(&signature)).unwrap(), &f.key);
}

#[test]
fn verify_only_never_takes_a_private_key() {
    let f = fixture!();
    f.build_and_sign();
    let r = f.verify_only(&[("UPDATE_PRIVATE_KEY", "-----BEGIN PRIVATE KEY-----")]);
    assert!(r.ok(), "{}", r.log());
    assert!(
        r.stdout
            .contains("VERIFY_ONLY=1: ignoring UPDATE_PRIVATE_KEY"),
        "{}",
        r.log()
    );
}

#[test]
fn verify_only_needs_both_artifacts() {
    let f = fixture!();
    f.build_only();
    let r = f.verify_only(&[]);
    assert!(!r.ok());
    assert!(
        r.stderr
            .contains(&format!("missing {}", path_str(&f.manifest_path()))),
        "{}",
        r.log()
    );
}

#[test]
fn shipped_scripts_are_executable() {
    for rel in [
        "scripts/build-release.sh",
        "setup.sh",
        "scripts/bootstrap-fresh-pi.sh",
    ] {
        let mode = fs::metadata(repo_root().join(rel))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "{rel} must be executable");
    }
}

// --- the workflows that run it ----------------------------------------------------

/// The release (release.yml) and pull-request (rust.yml, lcd.yml) workflows
/// pin every action to a commit SHA, the same one in all of them; keep the
/// default token read-only and out of .git/config, where build scripts and
/// proc-macros could read it; and install cargo-zigbuild only from the one
/// hash-pinned requirements file, so the release build and the pull-request
/// build cannot drift apart.
#[test]
fn workflows_pin_their_tools_and_keep_the_token_read_only() {
    let workflows = repo_root().join(".github/workflows");
    let uses = regex::Regex::new(r"^\s*(?:- )?uses:\s*([^@\s]+)@(\S+)").unwrap();
    let sha = regex::Regex::new(r"^[0-9a-f]{40}$").unwrap();
    let mut pins: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut zigbuild_installs = BTreeSet::new();
    for entry in fs::read_dir(&workflows).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "yml") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\npermissions:\n  contents: read\n"),
            "{name}: no top-level `permissions: contents: read`"
        );
        assert!(!text.contains("ziglang=="), "{name} pins zig itself");
        if text.contains("-r .github/zigbuild-requirements.txt") {
            zigbuild_installs.insert(name.clone());
        }
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let Some(c) = uses.captures(line) else {
                continue;
            };
            let (action, rev) = (c[1].to_string(), c[2].to_string());
            assert!(
                sha.is_match(&rev),
                "{name}: {action}@{rev} is not a commit SHA"
            );
            if action == "actions/checkout" {
                // The rest of the step: up to the next one.
                let mut step = lines[i + 1..]
                    .iter()
                    .take_while(|l| !l.trim_start().starts_with("- "));
                assert!(
                    step.any(|l| l.trim() == "persist-credentials: false"),
                    "{name}:{}: checkout keeps the token in .git/config",
                    i + 1
                );
            }
            pins.entry(action).or_default().insert(rev);
        }
    }
    for (action, revs) in &pins {
        assert_eq!(revs.len(), 1, "{action} pinned to different SHAs: {revs:?}");
    }
    assert!(pins.contains_key("actions/checkout"), "{pins:?}");
    assert_eq!(
        zigbuild_installs,
        BTreeSet::from(["release.yml".to_string(), "rust.yml".to_string()])
    );
    let requirements =
        fs::read_to_string(repo_root().join(".github/zigbuild-requirements.txt")).unwrap();
    for pin in ["ziglang==", "cargo-zigbuild=="] {
        assert!(requirements.contains(pin), "{pin}\n{requirements}");
    }
    assert!(
        requirements.matches("--hash=sha256:").count() >= 4,
        "{requirements}"
    );
}
