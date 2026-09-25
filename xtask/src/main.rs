//! Cross-platform task runner: `cargo xtask <task>` (alias in `.cargo/config.toml`).
//!
//! CI calls these same subcommands, so local runs and CI can't drift.
//! See CONTRIBUTING.md#task-runner.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};
use std::sync::OnceLock;

const CONTRACTS: [&str; 3] = [
    "multisig-account",
    "attester-registry",
    "attestation-registry",
];
const WASM_TARGET: &str = "wasm32v1-none";

type Result<T = ()> = std::result::Result<T, String>;

const USAGE: &str = "\
Usage: cargo xtask <task> [options]

Tasks:
  check                     fmt --check, clippy, test, wasm, conformance
                            (conformance skipped if stellar-cli is missing)
  ci                        check, plus `test --all-features` (what CI runs)
  fmt [--check]             rustfmt the workspace
  clippy                    clippy with -D warnings
  test [--all-features]     run the workspace tests
  wasm [--reproducible]     build the contract wasm (reproducible: remapped
                            paths, SOURCE_DATE_EPOCH, LAFIYA_GIT_COMMIT)
  bindings                  regenerate TypeScript bindings (needs stellar-cli)
  conformance [--update]    interface snapshot / error docs / events doc checks
  budgets                   run the large-allowlist resource-budget load test
  fuzz                      proptest fuzz suites (PROPTEST_CASES, default 256)
  release-manifest          build wasm, generate + validate release-manifest.json
  docs                      cargo doc with -D warnings
";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let (task, flags) = match args.split_first() {
        Some((t, f)) => (t.as_str(), f),
        None => {
            eprint!("{USAGE}");
            exit(2);
        }
    };
    let has = |flag: &str| flags.iter().any(|f| f == flag);
    let result = match task {
        "check" => check(false),
        "ci" => check(true),
        "fmt" => fmt(has("--check")),
        "clippy" => clippy(),
        "test" => test(has("--all-features")),
        "wasm" => wasm(has("--reproducible")),
        "bindings" => bindings(),
        "conformance" => conformance(has("--update")),
        "budgets" => cargo(&[
            "test",
            "-p",
            "attester-registry",
            "large_attester_allowlist_load",
            "--",
            "--nocapture",
        ]),
        "fuzz" => fuzz(),
        "release-manifest" => release_manifest(),
        "docs" => docs(),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown task `{other}`\n\n{USAGE}")),
    };
    if let Err(e) = result {
        eprintln!("xtask: {e}");
        exit(1);
    }
}

fn check(ci: bool) -> Result {
    fmt(true)?;
    clippy()?;
    test(false)?;
    if ci {
        test(true)?;
    }
    wasm(false)?;
    // The conformance scripts decode the contract spec with stellar-cli; don't
    // make it a hard requirement of the core loop (see CONTRIBUTING.md).
    if Command::new("stellar").arg("--version").output().is_ok() {
        conformance(false)
    } else {
        eprintln!("xtask: WARNING: `stellar` not on PATH, skipping conformance");
        Ok(())
    }
}

fn fmt(check: bool) -> Result {
    if check {
        cargo(&["fmt", "--all", "--", "--check"])
    } else {
        cargo(&["fmt", "--all"])
    }
}

fn clippy() -> Result {
    cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--locked",
        "--",
        "-D",
        "warnings",
    ])
}

fn test(all_features: bool) -> Result {
    if all_features {
        cargo(&["test", "--workspace", "--all-features", "--locked"])
    } else {
        cargo(&["test", "--workspace", "--locked"])
    }
}

/// Only the Soroban contract crates target wasm32v1-none; the std-only
/// workspace members can't build for this no_std target.
fn wasm(reproducible: bool) -> Result {
    let mut args = vec!["build", "--release", "--locked", "--target", WASM_TARGET];
    for c in CONTRACTS {
        args.extend(["-p", c]);
    }
    let mut cmd = cargo_cmd(&args);
    if reproducible {
        let root = root();
        let cargo_home = env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(".cargo")))
            .ok_or("cannot locate CARGO_HOME")?;
        // Unit-separator encoding keeps paths with spaces (Windows) intact.
        let flags = [
            format!("--remap-path-prefix={}=/lafiya", root.display()),
            format!("--remap-path-prefix={}=/cargo", cargo_home.display()),
        ]
        .join("\x1f");
        // Pre-set values win so builds without git (e.g. the container) work.
        let commit = env_or_git("LAFIYA_GIT_COMMIT", &["rev-parse", "HEAD"])?;
        let epoch = env_or_git("SOURCE_DATE_EPOCH", &["log", "-1", "--format=%ct"])?;
        cmd.env_remove("RUSTFLAGS")
            .env("CARGO_ENCODED_RUSTFLAGS", flags)
            .env("SOURCE_DATE_EPOCH", epoch)
            .env("LAFIYA_GIT_COMMIT", &commit);
        println!("xtask: reproducible build of {commit}");
    }
    run(&mut cmd)
}

fn bindings() -> Result {
    wasm(false)?;
    for (wasm, dir) in [
        ("attester_registry", "attester-registry"),
        ("attestation_registry", "attestation-registry"),
    ] {
        let wasm_path = format!("target/{WASM_TARGET}/release/{wasm}.wasm");
        let out_dir = format!("bindings/{dir}");
        run(Command::new("stellar").current_dir(root()).args([
            "contract",
            "bindings",
            "typescript",
            "--wasm",
            &wasm_path,
            "--output-dir",
            &out_dir,
            "--overwrite",
        ]))?;
    }
    Ok(())
}

fn conformance(update: bool) -> Result {
    wasm(false)?;
    let scripts: &[&[&str]] = if update {
        &[
            &["scripts/conformance/check_snapshot.py", "--update"],
            &["scripts/conformance/gen_events_doc.py"],
        ]
    } else {
        &[
            &["scripts/conformance/check_snapshot.py"],
            &["scripts/conformance/check_error_docs.py"],
            &["scripts/conformance/gen_events_doc.py", "--check"],
            &["scripts/conformance/check_bindings_drift.py"],
        ]
    };
    for script in scripts {
        python(script)?;
    }
    Ok(())
}

fn fuzz() -> Result {
    let mut cmd = cargo_cmd(&[
        "test",
        "--workspace",
        "--locked",
        "fuzz_test",
        "--",
        "--nocapture",
    ]);
    if env::var_os("PROPTEST_CASES").is_none() {
        cmd.env("PROPTEST_CASES", "256");
    }
    run(&mut cmd)
}

fn release_manifest() -> Result {
    wasm(false)?;
    python(&[
        "scripts/generate_release_manifest.py",
        "--pretty",
        "-o",
        "release-manifest.json",
    ])?;
    python(&[
        "scripts/validate_release_manifest.py",
        "release-manifest.json",
    ])
}

fn docs() -> Result {
    run(cargo_cmd(&["doc", "--workspace", "--no-deps"]).env("RUSTDOCFLAGS", "-D warnings"))
}

/// Resolved at runtime, not via `env!("CARGO_MANIFEST_DIR")`: the xtask
/// binary is shared between host and container builds of the same checkout.
fn root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let manifest = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["locate-project", "--workspace", "--message-format", "plain"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
        match manifest.as_deref().and_then(Path::parent) {
            Some(dir) => dir.to_path_buf(),
            None => env::current_dir().expect("current directory"),
        }
    })
    .clone()
}

fn home() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn cargo_cmd(args: &[&str]) -> Command {
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")));
    cmd.current_dir(root()).args(args);
    cmd
}

fn cargo(args: &[&str]) -> Result {
    run(&mut cargo_cmd(args))
}

/// `python3` on Unix, `python` on Windows (where `python3` is often a
/// Microsoft Store stub).
fn python(args: &[&str]) -> Result {
    let exe = if cfg!(windows) { "python" } else { "python3" };
    run(Command::new(exe).current_dir(root()).args(args))
}

fn env_or_git(var: &str, args: &[&str]) -> Result<String> {
    match env::var(var) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => git(args),
    }
}

fn git(args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(root())
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {} failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run(cmd: &mut Command) -> Result {
    println!("$ {cmd:?}");
    let status = cmd.status().map_err(|e| format!("{cmd:?}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd:?} exited with {status}"))
    }
}
