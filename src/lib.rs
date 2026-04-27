use std::{fs, path::PathBuf};

use zed_extension_api::{self as zed, Result, serde_json::Value, settings::LspSettings};

struct LeanToExtension;

impl LeanToExtension {
    fn find_lean4_lsp(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<String> {
        let shell_env = worktree.shell_env();
        let lsp_settings = LspSettings::for_worktree(language_server_id.as_ref(), worktree)?;

        if let Some(binary) = lsp_settings.binary
            && let Some(path) = binary.path
        {
            return Ok(path);
        }

        // Check $PATH
        if let Some(path) = worktree.which("lake") {
            return Ok(path);
        }

        // Check $ELAN_HOME or default directory
        let elan_home = shell_env
            .iter()
            .find_map(|(k, v)| (k == "ELAN_HOME").then_some(PathBuf::from(v)))
            .or_else(|| {
                let home = shell_env
                    .iter()
                    .find_map(|(k, v)| (k == "HOME" || k == "USERPROFILE").then_some(v))?;
                Some(PathBuf::from(home).join(".elan"))
            })
            .ok_or("Failed to find ELAN_HOME, HOME, or USERPROFILE")?;

        let (platform, arch) = zed::current_platform();

        let lake_path = elan_home.join("bin").join(match platform {
            zed::Os::Windows => "lake.exe",
            zed::Os::Mac | zed::Os::Linux => "lake",
        });
        let lake_path_str = lake_path.to_string_lossy().to_string();

        if worktree.which(&lake_path_str).is_some() {
            return Ok(lake_path_str);
        }

        // Check lsp.lean4-lsp.settings
        let elan_auto_install = lsp_settings
            .settings
            .as_ref()
            .and_then(|s| s.pointer("/elan_auto_install"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if !elan_auto_install {
            return Err(
                        "Failed to find lake in PATH or ELAN_HOME/default path. Enable lsp.lean4-lsp.settings.elan_auto_install or configure lsp.lean4-lsp.binary.path."
                            .into(),
                    );
        }

        let elan_default_toolchain = lsp_settings
            .settings
            .as_ref()
            .and_then(|s| s.pointer("/elan_default_toolchain"))
            .and_then(Value::as_str)
            .unwrap_or("stable");

        // Install elan and lean 4 toolchain
        let release = zed::latest_github_release(
            "leanprover/elan",
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;

        let asset_name: String = format!(
            "elan-{}-{}.{}",
            match arch {
                zed::Architecture::Aarch64 => "aarch64",
                zed::Architecture::X8664 => "x86_64",
                zed::Architecture::X86 =>
                    return Err("32-bit x86 architecture is not supported by elan".into()),
            },
            match platform {
                zed::Os::Windows => "pc-windows-msvc",
                zed::Os::Mac => "apple-darwin",
                zed::Os::Linux => "unknown-linux-gnu",
            },
            match platform {
                zed::Os::Windows => "zip",
                zed::Os::Mac | zed::Os::Linux => "tar.gz",
            },
        );

        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| format!("No asset found matching: {asset_name:?}"))?;

        let version_dir = format!("elan-{}", release.version);
        let elan_init_name = match platform {
            zed::Os::Windows => "elan-init.exe",
            zed::Os::Mac | zed::Os::Linux => "elan-init",
        };

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::Downloading,
        );
        zed::download_file(
            &asset.download_url,
            &version_dir,
            match platform {
                zed::Os::Windows => zed::DownloadedFileType::Zip,
                zed::Os::Mac | zed::Os::Linux => zed::DownloadedFileType::GzipTar,
            },
        )
        .map_err(|e| format!("Failed to download file: {e}"))?;

        let cwd =
            std::env::current_dir().map_err(|e| format!("Failed to get current directory: {e}"))?;
        let elan_init_path = cwd.join(&version_dir).join(elan_init_name);
        let elan_init_path_str = elan_init_path.to_string_lossy().to_string();

        zed::make_file_executable(&elan_init_path_str)?;
        zed::Command::new(&elan_init_path_str)
            .args([
                "-y",
                "--default-toolchain",
                &format!("leanprover/lean4:{elan_default_toolchain}"),
            ])
            .output()?;
        fs::remove_dir_all(&version_dir).ok();

        Ok(elan_init_path_str)
    }

    fn find_or_download_proxy(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<String> {
        let lsp_settings = LspSettings::for_worktree(language_server_id.as_ref(), worktree)?;
        if let Some(path) = lsp_settings
            .settings
            .as_ref()
            .and_then(|s| s.pointer("/proxy_path"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Ok(path.to_string());
        }

        let (platform, arch) = zed::current_platform();
        let arch_str = match arch {
            zed::Architecture::Aarch64 => "aarch64",
            zed::Architecture::X8664 => "x86_64",
            zed::Architecture::X86 => {
                return Err("32-bit x86 architecture is not supported".into());
            }
        };
        let platform_str = match platform {
            zed::Os::Windows => "pc-windows-msvc",
            zed::Os::Mac => "apple-darwin",
            zed::Os::Linux => "unknown-linux-gnu",
        };
        let ext = if matches!(platform, zed::Os::Windows) {
            ".exe"
        } else {
            ""
        };
        let asset_name = format!("leander-proxy-{arch_str}-{platform_str}{ext}");
        let binary_name = format!("leander-proxy{ext}");

        let cwd =
            std::env::current_dir().map_err(|e| format!("Failed to get current directory: {e}"))?;

        let release_result = zed::latest_github_release(
            "SrGaabriel/leander",
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        );

        let release = match release_result {
            Ok(r) => r,
            Err(release_err) => {
                if let Some(cached) = find_latest_cached_proxy(&cwd, &binary_name) {
                    return Ok(cached);
                }
                return Err(format!(
                    "Failed to fetch latest leander release ({release_err})"
                ));
            }
        };

        let version_dir = cwd.join(format!("proxy-{}", release.version));
        let proxy_path = version_dir.join(&binary_name);
        let proxy_path_str = proxy_path.to_string_lossy().to_string();

        if proxy_path.exists() {
            return Ok(proxy_path_str);
        }

        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| {
                format!(
                    "leander release {} has no asset named {asset_name}",
                    release.version
                )
            })?;

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::Downloading,
        );
        fs::create_dir_all(&version_dir).map_err(|e| format!("Failed to create proxy dir: {e}"))?;
        zed::download_file(
            &asset.download_url,
            &proxy_path_str,
            zed::DownloadedFileType::Uncompressed,
        )
        .map_err(|e| format!("Failed to download proxy: {e}"))?;
        zed::make_file_executable(&proxy_path_str)?;

        if let Ok(entries) = fs::read_dir(&cwd) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path != version_dir
                    && path.is_dir()
                    && path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("proxy-"))
                {
                    let _ = fs::remove_dir_all(&path);
                }
            }
        }

        Ok(proxy_path_str)
    }
}

fn find_latest_cached_proxy(cwd: &PathBuf, binary_name: &str) -> Option<String> {
    let entries = fs::read_dir(cwd).ok()?;
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if !name.starts_with("proxy-") || !path.is_dir() {
                return None;
            }
            let bin = path.join(binary_name);
            if !bin.exists() {
                return None;
            }
            let mtime = entry.metadata().ok()?.modified().ok()?;
            Some((mtime, bin))
        })
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p.to_string_lossy().into_owned())
}

impl zed::Extension for LeanToExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let lean4_lsp_path = self.find_lean4_lsp(language_server_id, worktree)?;
        let proxy_path = self.find_or_download_proxy(language_server_id, worktree)?;
        Ok(zed::Command {
            command: proxy_path,
            args: vec!["--lsp".to_string(), lean4_lsp_path],
            env: vec![],
        })
    }
}

zed::register_extension!(LeanToExtension);
