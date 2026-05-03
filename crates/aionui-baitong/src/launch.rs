//! How to start the local Baitong adapter process.

use std::path::{Path, PathBuf};

/// Directory name of the packaged sidecar, next to `aioncore.exe`.
///
/// A packaged AionUi ships `bundled-baitong/win32-x64/baitong-sidecar/`, which
/// electron-builder copies under `resources/bundled-aioncore/`. The supervisor
/// resolves the executable there so a packaged install never needs a system
/// Python.
pub const BUNDLED_SIDECAR_DIR: &str = "baitong-sidecar";
pub const BUNDLED_SIDECAR_EXE: &str = "baitong-sidecar.exe";

/// Resolved launch plan for the adapter.
///
/// In development the adapter is the Python package
/// (`python -m opencode_v4_app.desktop_sidecar`); in a packaged build the same
/// entry point is a PyInstaller onedir sidecar. Both are described by the same
/// plan so supervision code does not branch on packaging.
#[derive(Debug, Clone)]
pub struct AdapterLaunch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl AdapterLaunch {
    /// Resolve the launch plan.
    ///
    /// Order matters: an explicit environment override always wins (that is how
    /// development and CI point at a checkout), then a sidecar shipped next to
    /// the running `aioncore` binary, then the development module entry point.
    pub fn from_env() -> Self {
        if let Some(program) = non_empty_env("BAITONG_ADAPTER_PROGRAM") {
            let args = non_empty_env("BAITONG_ADAPTER_ARGS")
                .map(|value| value.split_whitespace().map(str::to_string).collect())
                .unwrap_or_else(dev_args);
            return Self {
                program: PathBuf::from(program),
                args,
                cwd: non_empty_env("BAITONG_ADAPTER_ROOT").map(PathBuf::from),
            };
        }

        if let Some(sidecar) = Self::bundled_sidecar_next_to(std::env::current_exe().ok().as_deref()) {
            return Self::sidecar(sidecar);
        }

        Self {
            program: PathBuf::from("python"),
            args: dev_args(),
            cwd: non_empty_env("BAITONG_ADAPTER_ROOT").map(PathBuf::from),
        }
    }

    /// Plan that runs an already-built sidecar executable.
    pub fn sidecar(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
        }
    }

    /// Locate the packaged sidecar relative to a `aioncore` executable.
    ///
    /// Both layouts are accepted because the packaged tree and a `cargo` build
    /// place the binary in different depths:
    ///
    /// - `<exe dir>/baitong-sidecar/baitong-sidecar.exe` (sidecar beside exe)
    /// - `<exe dir>/bundled-aioncore/<runtime>/baitong-sidecar/...` (resources)
    pub fn bundled_sidecar_next_to(exe: Option<&Path>) -> Option<PathBuf> {
        let exe = exe?;
        let dir = exe.parent()?;
        let candidates = [
            dir.join(BUNDLED_SIDECAR_DIR).join(BUNDLED_SIDECAR_EXE),
            dir.join("bundled-aioncore")
                .join(runtime_key())
                .join(BUNDLED_SIDECAR_DIR)
                .join(BUNDLED_SIDECAR_EXE),
        ];
        candidates.into_iter().find(|path| path.is_file())
    }
}

fn dev_args() -> Vec<String> {
    vec!["-m".to_string(), "opencode_v4_app.desktop_sidecar".to_string()]
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn runtime_key() -> String {
    let os = match std::env::consts::OS {
        "windows" => "win32",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_plan_has_no_python_arguments() {
        let plan = AdapterLaunch::sidecar("C:/app/baitong-sidecar.exe");
        assert!(plan.args.is_empty());
        assert!(plan.cwd.is_none());
    }

    #[test]
    fn finds_a_sidecar_beside_the_executable() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("aioncore.exe");
        std::fs::write(&exe, b"stub").unwrap();
        let sidecar_dir = dir.path().join(BUNDLED_SIDECAR_DIR);
        std::fs::create_dir_all(&sidecar_dir).unwrap();
        let sidecar = sidecar_dir.join(BUNDLED_SIDECAR_EXE);
        std::fs::write(&sidecar, b"stub").unwrap();

        let found = AdapterLaunch::bundled_sidecar_next_to(Some(&exe));
        assert_eq!(found.as_deref(), Some(sidecar.as_path()));
    }

    #[test]
    fn missing_sidecar_falls_back_instead_of_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("aioncore.exe");
        std::fs::write(&exe, b"stub").unwrap();

        assert!(AdapterLaunch::bundled_sidecar_next_to(Some(&exe)).is_none());
        assert!(AdapterLaunch::bundled_sidecar_next_to(None).is_none());
    }
}
