# phpstan-zed
PHPStan LSP/extension for the Zed editor built entirely by Claude Code

## Install

  Clone -> Build -> Zed -> Extensions -> Install Dev Extension -> Select Repo Folder

## Build

    rustup target add wasm32-wasip1
    cargo build -p phpstan --target wasm32-wasip1
    cargo build -p phpstan-lsp-server --release

## Settings

Change values to match your setup

    "lsp": {
        "phpstan": {
          "binary": {
            "path": "<cloned-location>/target/release/phpstan-lsp-server",
          },
          "settings": {
            "phpstan_path": "vendor/bin/phpstan",
            "phpstan_level": "max",
            "phpstan_memory_limit": "5G",
            "phpstan_config": "./phpstan.neon",
          },
        },
      },
    }

## Issues

  Probably. Happy to accept PRs. This is entirely vibe-coded and I dont know Rust.
