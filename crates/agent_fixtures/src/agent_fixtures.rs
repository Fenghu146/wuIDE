//! Fake agent executables for the harness tests.
//!
//! The `fake-claude` binary in this crate plays the scripted stream-json
//! transcripts that `crates/agent_harness/tests/fixtures/fake-claude.sh` used to
//! play, but as a native executable so the tests that drive it also run on
//! Windows.
//!
//! `cargo test` only builds binaries of the package under test, so a test that
//! spawns the fake has to build it itself. [`fake_claude`] does that once per
//! test process and returns the path of the binary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const BINARY_NAME: &str = "fake-claude";

/// Builds that take at least this long are announced, so a slow first test run
/// is not mistaken for a hang.
const NOTICEABLE_BUILD: Duration = Duration::from_secs(1);

/// Path of the built `fake-claude` binary, building it on first use.
///
/// # Panics
///
/// Panics with the command to run by hand when the build fails or the binary
/// cannot be found next to the running test executable.
pub fn fake_claude() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| build().unwrap_or_else(|error| panic!("{error}")))
}

/// Builds the fake once, before any test spawns it.
///
/// This blocks on purpose: the tests that need the binary cannot proceed without
/// it, and it is built at most once per test process.
#[allow(clippy::disallowed_methods)]
fn build() -> Result<PathBuf, String> {
    let profile_directory = profile_directory()?;
    let profile = profile_directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();

    let mut command = Command::new(env!("CARGO"));
    command.args(["build", "-p", "agent_fixtures", "--bin", BINARY_NAME]);
    match profile.as_str() {
        "debug" => {}
        "release" => {
            command.arg("--release");
        }
        // Custom profiles such as `release-fast` are addressed by name.
        other => {
            command.args(["--profile", other]);
        }
    }

    let started = Instant::now();
    let output = command
        .output()
        .map_err(|error| format!("could not run {}: {error}", env!("CARGO")))?;
    if !output.status.success() {
        return Err(format!(
            "could not build {BINARY_NAME}, so the agent tests cannot run:\n{}\nRun `cargo build -p agent_fixtures --bin {BINARY_NAME}` to see the error.",
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    let elapsed = started.elapsed();
    if elapsed >= NOTICEABLE_BUILD {
        eprintln!(
            "built {BINARY_NAME} for the agent tests in {:.1}s",
            elapsed.as_secs_f32()
        );
    }

    let binary = profile_directory.join(format!("{BINARY_NAME}{}", std::env::consts::EXE_SUFFIX));
    if !binary.is_file() {
        return Err(format!(
            "built {BINARY_NAME} but {} does not exist; run `cargo build -p agent_fixtures --bin {BINARY_NAME}` and check the target directory",
            binary.display()
        ));
    }
    Ok(binary)
}

/// The `<target>/<profile>` directory holding the running test executable.
fn profile_directory() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate the test executable: {error}"))?;
    let mut directory = executable.parent();
    while let Some(path) = directory {
        if path.file_name().is_some_and(|name| name == "deps")
            && let Some(profile) = path.parent()
        {
            return Ok(profile.to_path_buf());
        }
        directory = path.parent();
    }
    Err(format!(
        "could not find the build directory above {}; run `cargo build -p agent_fixtures --bin {BINARY_NAME}` by hand",
        executable.display()
    ))
}
