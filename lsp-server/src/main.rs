use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

const MAX_CONCURRENT_ANALYSES: usize = 2;
const ANALYSIS_TIMEOUT_SECS: u64 = 30;

// PHPStan JSON output structures
#[derive(Debug, Deserialize)]
struct PhpstanOutput {
    files: HashMap<String, PhpstanFileResult>,
}

#[derive(Debug, Deserialize)]
struct PhpstanFileResult {
    messages: Vec<PhpstanMessage>,
}

#[derive(Debug, Deserialize)]
struct PhpstanMessage {
    message: String,
    line: Option<u32>,
    #[allow(dead_code)]
    ignorable: Option<bool>,
    #[serde(default)]
    identifier: Option<String>,
    #[serde(default)]
    tip: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PhpstanSettings {
    #[serde(default = "default_phpstan_path", alias = "phpstan_path")]
    phpstan_path: String,
    #[serde(default, alias = "phpstan_config")]
    phpstan_config: Option<String>,
    #[serde(default, alias = "phpstan_level")]
    phpstan_level: Option<String>,
    #[serde(default, alias = "phpstan_memory_limit")]
    phpstan_memory_limit: Option<String>,
}

fn default_phpstan_path() -> String {
    "vendor/bin/phpstan".to_string()
}

impl Default for PhpstanSettings {
    fn default() -> Self {
        Self {
            phpstan_path: default_phpstan_path(),
            phpstan_config: None,
            phpstan_level: None,
            phpstan_memory_limit: None,
        }
    }
}

struct PhpstanLspServer {
    client: Client,
    settings: tokio::sync::RwLock<PhpstanSettings>,
    workspace_root: tokio::sync::RwLock<Option<PathBuf>>,
    semaphore: Arc<Semaphore>,
}

impl PhpstanLspServer {
    fn new(client: Client) -> Self {
        Self {
            client,
            settings: tokio::sync::RwLock::new(PhpstanSettings::default()),
            workspace_root: tokio::sync::RwLock::new(None),
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_ANALYSES)),
        }
    }

    /// Pull settings from the editor via workspace/configuration request.
    /// Tries with section "phpstan" first, then without a section as fallback.
    async fn fetch_settings(&self) {
        // Try with section first (Zed resolves this by looking up the key in
        // the value returned by language_server_workspace_configuration)
        let items = vec![ConfigurationItem {
            scope_uri: None,
            section: Some("phpstan".to_string()),
        }];

        if let Ok(values) = self.client.configuration(items).await {
            eprintln!("PHPStan: workspace/configuration (section=phpstan): {:?}", values);
            if let Some(value) = values.into_iter().next() {
                if !value.is_null() {
                    let settings = self.extract_settings(&value);
                    eprintln!("PHPStan: applied settings: {:?}", settings);
                    *self.settings.write().await = settings;
                    return;
                }
            }
        }

        // Fallback: request without section
        let items = vec![ConfigurationItem {
            scope_uri: None,
            section: None,
        }];

        match self.client.configuration(items).await {
            Ok(values) => {
                eprintln!("PHPStan: workspace/configuration (no section): {:?}", values);
                if let Some(value) = values.into_iter().next() {
                    let settings = self.extract_settings(&value);
                    eprintln!("PHPStan: applied settings: {:?}", settings);
                    *self.settings.write().await = settings;
                }
            }
            Err(e) => {
                eprintln!("PHPStan: failed to fetch configuration: {}", e);
            }
        }
    }

    fn extract_settings(&self, value: &serde_json::Value) -> PhpstanSettings {
        if value.is_null() || (value.is_object() && value.as_object().unwrap().is_empty()) {
            eprintln!("PHPStan: received null/empty settings, using defaults");
            return PhpstanSettings::default();
        }

        // Try nested under various keys, then flat
        let candidates: Vec<Option<&serde_json::Value>> = vec![
            value.get("phpstan").and_then(|v| v.get("settings")),
            value.get("phpstan"),
            value.get("settings"),
            Some(value),
        ];

        for candidate in candidates.into_iter().flatten() {
            if candidate.is_null() {
                continue;
            }
            if let Some(obj) = candidate.as_object() {
                if obj.is_empty() {
                    continue;
                }
            }
            match serde_json::from_value::<PhpstanSettings>(candidate.clone()) {
                Ok(settings) => {
                    // Check if we actually got non-default values
                    if settings.phpstan_path != default_phpstan_path()
                        || settings.phpstan_config.is_some()
                        || settings.phpstan_level.is_some()
                        || settings.phpstan_memory_limit.is_some()
                    {
                        eprintln!("PHPStan: matched settings from: {}", candidate);
                        return settings;
                    }
                }
                Err(e) => {
                    eprintln!("PHPStan: failed to parse candidate {}: {}", candidate, e);
                }
            }
        }

        eprintln!("PHPStan: no settings matched, using defaults");
        PhpstanSettings::default()
    }

    async fn analyse_file(&self, uri: &Url) {
        let file_path = match uri.to_file_path() {
            Ok(path) => path,
            Err(_) => {
                eprintln!("PHPStan: cannot convert URI to file path: {}", uri);
                return;
            }
        };

        let _permit = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.semaphore.acquire(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            _ => {
                eprintln!("PHPStan: timed out waiting for analysis slot");
                return;
            }
        };

        let settings = self.settings.read().await.clone();
        let workspace_root = self.workspace_root.read().await.clone();

        let diagnostics = match self
            .run_phpstan(&file_path, &settings, workspace_root.as_deref())
            .await
        {
            Ok(diags) => diags,
            Err(e) => {
                eprintln!("PHPStan analysis error: {}", e);
                return;
            }
        };

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }

    async fn run_phpstan(
        &self,
        file_path: &Path,
        settings: &PhpstanSettings,
        workspace_root: Option<&Path>,
    ) -> std::result::Result<Vec<Diagnostic>, String> {
        let phpstan_path = self.resolve_phpstan_path(&settings.phpstan_path, workspace_root);

        let mut args = vec![
            "analyse".to_string(),
            file_path.to_string_lossy().to_string(),
            "--error-format=json".to_string(),
            "--no-progress".to_string(),
            "--no-ansi".to_string(),
        ];

        // Add config file if specified, or auto-detect
        if let Some(ref config) = settings.phpstan_config {
            args.push(format!("--configuration={}", config));
        } else if let Some(root) = workspace_root {
            if let Some(config_path) = self.find_config_file(root) {
                args.push(format!(
                    "--configuration={}",
                    config_path.to_string_lossy()
                ));
            }
        }

        if let Some(ref level) = settings.phpstan_level {
            args.push(format!("--level={}", level));
        }

        if let Some(ref memory_limit) = settings.phpstan_memory_limit {
            args.push(format!("--memory-limit={}", memory_limit));
        }

        eprintln!(
            "PHPStan: running {} {}",
            phpstan_path,
            args.join(" ")
        );

        let mut cmd = Command::new(&phpstan_path);
        cmd.args(&args);

        if let Some(root) = workspace_root {
            cmd.current_dir(root);
        }

        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(ANALYSIS_TIMEOUT_SECS),
            cmd.output(),
        )
        .await
        {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return Err(format!("Failed to run PHPStan: {}. Is '{}' installed and accessible?", e, phpstan_path));
            }
            Err(_) => {
                return Err(format!(
                    "PHPStan analysis timed out after {}s",
                    ANALYSIS_TIMEOUT_SECS
                ));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !stderr.is_empty() {
            eprintln!("PHPStan stderr: {}", stderr);
        }

        // PHPStan exits with code 1 when it finds errors - that's expected
        if !output.status.success() && output.status.code() != Some(1) {
            return Err(format!(
                "PHPStan exited with code {:?}: {}",
                output.status.code(),
                stderr
            ));
        }

        self.parse_phpstan_output(&stdout, file_path)
    }

    fn resolve_phpstan_path(&self, configured_path: &str, workspace_root: Option<&Path>) -> String {
        if let Some(root) = workspace_root {
            let vendor_path = root.join(configured_path);
            if vendor_path.exists() {
                return vendor_path.to_string_lossy().to_string();
            }
        }

        // Fallback to the configured path (may be an absolute path or in PATH)
        configured_path.to_string()
    }

    fn find_config_file(&self, root: &Path) -> Option<PathBuf> {
        let candidates = [
            "phpstan.neon",
            "phpstan.neon.dist",
            "phpstan.dist.neon",
        ];

        for candidate in &candidates {
            let path = root.join(candidate);
            if path.exists() {
                return Some(path);
            }
        }

        None
    }

    fn parse_phpstan_output(
        &self,
        output: &str,
        file_path: &Path,
    ) -> std::result::Result<Vec<Diagnostic>, String> {
        let trimmed = output.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let phpstan_output: PhpstanOutput =
            serde_json::from_str(trimmed).map_err(|e| {
                format!("Failed to parse PHPStan JSON output: {}. Output was: {}", e, trimmed)
            })?;

        let file_key = file_path.to_string_lossy().to_string();

        let file_result = phpstan_output
            .files
            .get(&file_key)
            .or_else(|| {
                // PHPStan may use a different path format, try to find a match
                phpstan_output.files.values().next()
            });

        let messages = match file_result {
            Some(result) => &result.messages,
            None => return Ok(Vec::new()),
        };

        let diagnostics = messages
            .iter()
            .map(|msg| {
                let line = msg.line.unwrap_or(1).saturating_sub(1);

                let mut message = msg.message.clone();
                if let Some(ref tip) = msg.tip {
                    message.push_str(&format!("\n\nTip: {}", tip));
                }
                if let Some(ref identifier) = msg.identifier {
                    message.push_str(&format!("\n\nIdentifier: {}", identifier));
                }

                Diagnostic {
                    range: Range {
                        start: Position {
                            line,
                            character: 0,
                        },
                        end: Position {
                            line,
                            character: u32::MAX,
                        },
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("PHPStan".to_string()),
                    message,
                    code: msg.identifier.as_ref().map(|id| {
                        NumberOrString::String(id.clone())
                    }),
                    ..Default::default()
                }
            })
            .collect();

        Ok(diagnostics)
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for PhpstanLspServer {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Store workspace root
        if let Some(root_uri) = params.root_uri {
            if let Ok(path) = root_uri.to_file_path() {
                *self.workspace_root.write().await = Some(path);
            }
        }

        // Parse initialization options if provided
        if let Some(options) = params.initialization_options {
            eprintln!("PHPStan: raw initialization options: {}", serde_json::to_string_pretty(&options).unwrap_or_default());
            let settings = self.extract_settings(&options);
            eprintln!("PHPStan: init settings: {:?}", settings);
            *self.settings.write().await = settings;
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::NONE),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        eprintln!("PHPStan LSP server initialized, fetching configuration...");
        self.fetch_settings().await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.analyse_file(&params.text_document.uri).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.analyse_file(&params.text_document.uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // Clear diagnostics when file is closed
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }

    async fn did_change_configuration(&self, _params: DidChangeConfigurationParams) {
        // Zed sends {} — pull settings via workspace/configuration
        eprintln!("PHPStan: configuration changed, re-fetching...");
        self.fetch_settings().await;
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(PhpstanLspServer::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
