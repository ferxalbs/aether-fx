use crate::ObscuraError;

/// MCP protocol version implemented by the Obscura release pinned by AETHER Fx.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
/// Obscura release selected for the first external-provider integration.
pub const OBSCURA_VERSION: &str = "0.2.1";

/// Archive format used by one release asset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveFormat {
    TarGz,
    Zip,
}

/// Immutable release metadata compiled into one AETHER Fx release.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObscuraArtifact {
    pub version: &'static str,
    pub target: &'static str,
    pub asset_name: &'static str,
    pub url: &'static str,
    pub archive_size: u64,
    pub sha256: &'static str,
    pub version_output: &'static str,
    pub mcp_protocol_version: &'static str,
    pub archive_format: ArchiveFormat,
    pub binary_name: &'static str,
    pub worker_name: &'static str,
    pub launch_args: &'static [&'static str],
    /// `None` is intentional for v0.2.1: its `mcp` subcommand does not consume `--storage-dir`.
    pub profile_argument: Option<&'static str>,
}

#[cfg(any(
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"),
    all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"),
    all(target_os = "windows", target_arch = "x86_64"),
))]
macro_rules! artifact {
    ($target:literal, $asset:literal, $url:literal, $size:literal, $sha:literal, $format:expr, $binary:literal, $worker:literal) => {
        ObscuraArtifact {
            version: OBSCURA_VERSION,
            target: $target,
            asset_name: $asset,
            url: $url,
            archive_size: $size,
            sha256: $sha,
            version_output: "obscura 0.2.1",
            mcp_protocol_version: MCP_PROTOCOL_VERSION,
            archive_format: $format,
            binary_name: $binary,
            worker_name: $worker,
            launch_args: &["mcp"],
            profile_argument: None,
        }
    };
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub const CURRENT_ARTIFACT: ObscuraArtifact = artifact!(
    "x86_64-apple-darwin",
    "obscura-x86_64-macos-no-render.tar.gz",
    "https://github.com/h4ckf0r0day/obscura/releases/download/v0.2.1/obscura-x86_64-macos-no-render.tar.gz",
    45_342_726,
    "40f6551104b6ede43026710b9fb1a922db000524744fa50b7f2baac77d42f526",
    ArchiveFormat::TarGz,
    "obscura",
    "obscura-worker"
);

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const CURRENT_ARTIFACT: ObscuraArtifact = artifact!(
    "aarch64-apple-darwin",
    "obscura-aarch64-macos-no-render.tar.gz",
    "https://github.com/h4ckf0r0day/obscura/releases/download/v0.2.1/obscura-aarch64-macos-no-render.tar.gz",
    43_552_683,
    "c14d0565a2c4c432551957e5cef5df120a1290901492569aa148957a5e637d25",
    ArchiveFormat::TarGz,
    "obscura",
    "obscura-worker"
);

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
pub const CURRENT_ARTIFACT: ObscuraArtifact = artifact!(
    "x86_64-unknown-linux-gnu",
    "obscura-x86_64-linux-no-render.tar.gz",
    "https://github.com/h4ckf0r0day/obscura/releases/download/v0.2.1/obscura-x86_64-linux-no-render.tar.gz",
    47_658_644,
    "bf60fff504f15bf6e16b22cbbeefe99348f247d7f95d6bde5d06b34a7d9d9d9c",
    ArchiveFormat::TarGz,
    "obscura",
    "obscura-worker"
);

#[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
pub const CURRENT_ARTIFACT: ObscuraArtifact = artifact!(
    "aarch64-unknown-linux-gnu",
    "obscura-aarch64-linux-no-render.tar.gz",
    "https://github.com/h4ckf0r0day/obscura/releases/download/v0.2.1/obscura-aarch64-linux-no-render.tar.gz",
    49_404_469,
    "e8cf49330a56e695e75c15b5680cfd4293966f3493ccc1698b538350e4f9c112",
    ArchiveFormat::TarGz,
    "obscura",
    "obscura-worker"
);

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub const CURRENT_ARTIFACT: ObscuraArtifact = artifact!(
    "x86_64-pc-windows-msvc",
    "obscura-x86_64-windows-no-render.zip",
    "https://github.com/h4ckf0r0day/obscura/releases/download/v0.2.1/obscura-x86_64-windows-no-render.zip",
    40_480_291,
    "323e0f317af25aaeb18bc2cd0c27db9025361d9b11c7ce278beee42d39073afe",
    ArchiveFormat::Zip,
    "obscura.exe",
    "obscura-worker.exe"
);

/// Return the fixed artifact for the compilation target, if v0.2.1 publishes one.
pub fn current_artifact() -> Result<&'static ObscuraArtifact, ObscuraError> {
    #[cfg(any(
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"),
        all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"),
        all(target_os = "windows", target_arch = "x86_64"),
    ))]
    {
        return Ok(&CURRENT_ARTIFACT);
    }

    #[allow(unreachable_code)]
    Err(ObscuraError::UnsupportedTarget { target: current_target().to_owned() })
}

/// Return a stable target label without probing the network or the host package manager.
#[allow(unreachable_code)]
pub const fn current_target() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return "x86_64-apple-darwin";
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return "aarch64-apple-darwin";
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    {
        return "x86_64-unknown-linux-gnu";
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    {
        return "aarch64-unknown-linux-gnu";
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        return "x86_64-pc-windows-msvc";
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        return "aarch64-pc-windows-msvc";
    }
    "unsupported-target"
}
