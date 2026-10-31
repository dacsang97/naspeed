//! `naspeed uninstall`: undo exactly what `setup` changed.

use anyhow::{Context, Result, bail};

use super::setup::{FIRST_BACKUP_NAME, backups_dir};
use crate::cli::UninstallArgs;
use crate::config::Settings;
use crate::setup::cargo_config;
use crate::setup::{cargo_config_path, installed_binary};

pub fn run(args: UninstallArgs) -> Result<u8> {
    let settings = Settings::load();
    let config_dir = &settings.config_dir;
    let cfg_path = cargo_config_path();

    match cargo_config::load_state(config_dir) {
        Some(state) => {
            let (text, mut doc) = cargo_config::read_config(&cfg_path)?;
            let untouched_since_setup = state.written_hash.as_deref()
                == Some(blake3::hash(text.as_bytes()).to_hex().as_str());
            if untouched_since_setup && !state.config_existed {
                let _ = std::fs::remove_file(&cfg_path);
                println!(
                    "removed {} (it did not exist before setup)",
                    cfg_path.display()
                );
            } else if untouched_since_setup && let Some(first) = &state.first_backup {
                // Nothing changed after setup: the backup is the exact original.
                std::fs::copy(first, &cfg_path)
                    .with_context(|| format!("restore {}", first.display()))?;
                println!("restored {} from {}", cfg_path.display(), first.display());
            } else {
                let kept = cargo_config::revert(&mut doc, &state)?;
                std::fs::write(&cfg_path, doc.to_string())?;
                println!("reverted naspeed keys in {}", cfg_path.display());
                for path in kept {
                    println!("  kept {} (changed after setup)", path.join("."));
                }
            }
            for shim in &state.shims {
                let _ = std::fs::remove_file(shim);
            }
            std::fs::remove_file(cargo_config::state_path(config_dir))?;
        }
        None => {
            let first = backups_dir(config_dir).join(FIRST_BACKUP_NAME);
            if !first.exists() {
                bail!(
                    "no setup state and no {} backup: nothing to undo",
                    FIRST_BACKUP_NAME
                );
            }
            std::fs::copy(&first, &cfg_path)?;
            println!(
                "setup state missing; restored {} from {}",
                cfg_path.display(),
                first.display()
            );
        }
    }

    let alias = crate::setup::ns_alias();
    if crate::setup::is_our_ns_alias(&alias) {
        std::fs::remove_file(&alias)?;
        println!("removed {}", alias.display());
    }

    let bin = installed_binary();
    if bin.exists() {
        std::fs::remove_file(&bin)?;
        println!("removed {}", bin.display());
    }

    if args.purge {
        for store in &settings.stores {
            if store.join("objects").is_dir() && store.join("entries").is_dir() {
                std::fs::remove_dir_all(store)
                    .with_context(|| format!("remove store {}", store.display()))?;
                println!("removed store {}", store.display());
            }
        }
    } else {
        for store in &settings.stores {
            println!(
                "store kept at {} (uninstall --purge removes it)",
                store.display()
            );
        }
    }
    println!(
        "Artifacts naspeed placed into target dirs stay valid. To drop them: `naspeed purge --target <dir>` (before uninstalling) or `cargo clean`."
    );
    Ok(0)
}
