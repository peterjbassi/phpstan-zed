use std::fs;
use zed_extension_api::{self as zed, serde_json, settings::LspSettings, LanguageServerId, Result, Worktree};

struct PhpstanExtension {
    cached_binary_path: Option<String>,
}

impl PhpstanExtension {
    fn language_server_binary_path(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<String> {
        // Return cached path if still valid
        if let Some(ref path) = self.cached_binary_path {
            if fs::metadata(path).map_or(false, |m| m.is_file()) {
                return Ok(path.clone());
            }
        }

        // Check if user configured a custom binary path in settings
        if let Ok(lsp_settings) = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
        {
            if let Some(binary_settings) = lsp_settings.binary {
                if let Some(path) = binary_settings.path {
                    if fs::metadata(&path).map_or(false, |m| m.is_file()) {
                        self.cached_binary_path = Some(path.clone());
                        return Ok(path);
                    }
                    return Err(format!(
                        "Configured PHPStan LSP binary not found at: {}",
                        path
                    )
                    .into());
                }
            }
        }

        // Check for a pre-built binary in the extension's working directory
        let binary_name = "phpstan-lsp-server";
        if fs::metadata(binary_name).map_or(false, |m| m.is_file()) {
            self.cached_binary_path = Some(binary_name.to_string());
            return Ok(binary_name.to_string());
        }

        // Try downloading from GitHub releases
        let (platform, arch) = zed::current_platform();
        let os = match platform {
            zed::Os::Mac => "apple-darwin",
            zed::Os::Linux => "unknown-linux-gnu",
            zed::Os::Windows => "pc-windows-msvc",
        };
        let architecture = match arch {
            zed::Architecture::Aarch64 => "aarch64",
            zed::Architecture::X8664 => "x86_64",
            _ => return Err("Unsupported architecture".into()),
        };

        let release_binary_name = if matches!(platform, zed::Os::Windows) {
            "phpstan-lsp-server.exe"
        } else {
            "phpstan-lsp-server"
        };

        let release = zed::latest_github_release(
            "phpstan/phpstan-zed",
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )
        .map_err(|_| {
            "PHPStan LSP server binary not found. Either:\n\
             1. Set the binary path in Zed settings under lsp.phpstan.binary.path\n\
             2. Build the LSP server and place it in the extension directory\n\
             3. Publish releases to github.com/phpstan/phpstan-zed\n\n\
             To build locally:\n  \
               cd phpstan-zed && cargo build -p phpstan-lsp-server --release\n  \
               Then set lsp.phpstan.binary.path to the built binary path."
        })?;

        let asset_name = format!(
            "phpstan-lsp-server-{}-{}.tar.gz",
            architecture, os
        );

        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| format!("No asset found for platform: {}", asset_name))?;

        let version_dir = format!("phpstan-lsp-server-{}", release.version);
        let binary_path = format!("{}/{}", version_dir, release_binary_name);

        if !fs::metadata(&binary_path).map_or(false, |m| m.is_file()) {
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Downloading,
            );

            zed::download_file(
                &asset.download_url,
                &version_dir,
                zed::DownloadedFileType::GzipTar,
            )
            .map_err(|e| format!("Failed to download PHPStan LSP server: {}", e))?;

            zed::make_file_executable(&binary_path)?;
        }

        self.cached_binary_path = Some(binary_path.clone());
        Ok(binary_path)
    }
}

impl zed::Extension for PhpstanExtension {
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<zed::Command> {
        let binary_path = self.language_server_binary_path(language_server_id, worktree)?;

        Ok(zed::Command {
            command: binary_path,
            args: Vec::new(),
            env: Default::default(),
        })
    }

    fn language_server_initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<serde_json::Value>> {
        let settings = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .ok()
            .and_then(|lsp_settings| lsp_settings.settings);

        Ok(settings)
    }

    fn language_server_workspace_configuration(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<serde_json::Value>> {
        let settings = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .ok()
            .and_then(|lsp_settings| lsp_settings.settings);

        // Wrap settings under "phpstan" key so Zed can resolve
        // workspace/configuration requests with section: "phpstan"
        Ok(Some(serde_json::json!({
            "phpstan": settings.unwrap_or(serde_json::Value::Object(Default::default()))
        })))
    }
}

zed::register_extension!(PhpstanExtension);
