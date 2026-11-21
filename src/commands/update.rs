//! `naspeed update` / `ns update`: bring this install to the latest release
//! through the channel it came from (Homebrew tap, `cargo install`, or a
//! manually downloaded archive), then refresh the `~/.cargo/bin` copy and the
//! `ns` alias via `naspeed setup`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::UpdateArgs;
use crate::config;
use crate::update::{self, CURRENT, InstallMethod, RELEASES_URL};

pub fn run(args: UpdateArgs) -> Result<u8> {
    let method = update::detect_install_method();
    let channel = match method {
        InstallMethod::Brew => "Homebrew tap",
        InstallMethod::Cargo => "cargo install",
        InstallMethod::Other => "manual install",
    };
    // `ns update` always asks the channel; the 20 h cache is for the passive
    // `ns dev` notice.
    let latest = update::latest_for(method).context("checking the latest release")?;
    crate::ui_info!(format!("naspeed {CURRENT} ({channel}), latest {latest}"));
    if !update::newer_than(&latest, CURRENT) {
        crate::ui_info!("already up to date");
        return Ok(0);
    }
    if args.check {
        crate::ui_info!(format!("update with: {}", method.instructions()));
        return Ok(0);
    }
    crate::ui_info!(format!("updating with: {}", method.instructions()));
    match method {
        InstallMethod::Brew => {
            run_cmd(Command::new("brew").args(["upgrade", "dacsang97/tap/naspeed"]))?;
            // Run the *fresh* binary's setup so it copies itself over the
            // ~/.cargo/bin copy cargo is configured to load.
            let prefix = String::from_utf8(
                Command::new("brew")
                    .arg("--prefix")
                    .output()
                    .context("brew --prefix")?
                    .stdout,
            )?;
            let fresh = Path::new(prefix.trim())
                .join("bin")
                .join(crate::platform::exe_name("naspeed"));
            run_cmd(Command::new(fresh).arg("setup"))?;
        }
        InstallMethod::Cargo => {
            // Windows cannot replace the executable of a running process; the
            // installed copy in ~/.cargo/bin may be this very process.
            #[cfg(windows)]
            if crate::platform::canonicalize(&std::env::current_exe()?)?
                == crate::platform::canonicalize(&crate::setup::installed_binary())?
            {
                bail!(
                    "cargo cannot replace the running naspeed.exe — run `cargo install naspeed --locked` from another shell, then `naspeed setup`"
                );
            }
            run_cmd(Command::new("cargo").args(["install", "naspeed", "--locked"]))?;
            // Refreshes the Windows `ns.exe` alias (a hard link keeps the old
            // bytes); on Unix it is a symlink and this is a fast no-op.
            run_cmd(
                Command::new(
                    config::cargo_home()
                        .join("bin")
                        .join(crate::platform::exe_name("naspeed")),
                )
                .arg("setup"),
            )?;
        }
        InstallMethod::Other => {
            println!("download the latest release from {RELEASES_URL}");
            println!("then run `naspeed setup` to refresh the cargo wrapper copy");
            return Ok(0);
        }
    }
    crate::ui_info!(format!("updated to {latest}"));
    Ok(0)
}

fn run_cmd(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to start {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("{:?} exited with {status}", cmd.get_program());
    }
    Ok(())
}
