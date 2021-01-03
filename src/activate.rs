// SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
// SPDX-FileCopyrightText: 2020 Andreas Fuchs <asf@boinkor.net>
//
// SPDX-License-Identifier: MPL-2.0

use clap::Clap;

use tokio::{fs, net::UnixListener};
use tokio::{io::AsyncReadExt, time::timeout};
use tokio::{net::UnixStream, process::Command};

use std::{net::Shutdown, time::Duration};

use std::path::Path;

use thiserror::Error;

extern crate pretty_env_logger;
#[macro_use]
extern crate log;

#[macro_use]
extern crate serde_derive;

#[macro_use]
mod utils;

/// Activation portion of the simple Rust Nix deploy tool
#[derive(Clap, Debug)]
#[clap(version = "1.0", author = "Serokell <https://serokell.io/>")]
enum Opts {
    Activate(Options),
    Confirm(Options),
}

#[derive(Clap, Debug)]
struct Options {
    profile_path: String,
    closure: String,

    /// Temp path for any temporary files that may be needed during activation
    #[clap(long)]
    temp_path: String,

    /// Maximum time to wait for confirmation after activation
    #[clap(long)]
    confirm_timeout: u16,

    /// Wait for confirmation after deployment and rollback if not confirmed
    #[clap(long)]
    magic_rollback: bool,

    /// Auto rollback if failure
    #[clap(long)]
    auto_rollback: bool,
}

#[derive(Error, Debug)]
pub enum DeactivateError {
    #[error("Failed to execute the rollback command: {0}")]
    RollbackError(std::io::Error),
    #[error("The rollback resulted in a bad exit code: {0:?}")]
    RollbackExitError(Option<i32>),
    #[error("Failed to run command for listing generations: {0}")]
    ListGenError(std::io::Error),
    #[error("Command for listing generations resulted in a bad exit code: {0:?}")]
    ListGenExitError(Option<i32>),
    #[error("Error converting generation list output to utf8: {0}")]
    DecodeListGenUtf8Error(#[from] std::string::FromUtf8Error),
    #[error("Failed to run command for deleting generation: {0}")]
    DeleteGenError(std::io::Error),
    #[error("Command for deleting generations resulted in a bad exit code: {0:?}")]
    DeleteGenExitError(Option<i32>),
    #[error("Failed to run command for re-activating the last generation: {0}")]
    ReactivateError(std::io::Error),
    #[error("Command for re-activating the last generation resulted in a bad exit code: {0:?}")]
    ReactivateExitError(Option<i32>),
}

pub async fn deactivate(profile_path: &str) -> Result<(), DeactivateError> {
    warn!("De-activating due to error");

    let nix_env_rollback_exit_status = Command::new("nix-env")
        .arg("-p")
        .arg(&profile_path)
        .arg("--rollback")
        .status()
        .await
        .map_err(DeactivateError::RollbackError)?;

    match nix_env_rollback_exit_status.code() {
        Some(0) => (),
        a => return Err(DeactivateError::RollbackExitError(a)),
    };

    debug!("Listing generations");

    let nix_env_list_generations_out = Command::new("nix-env")
        .arg("-p")
        .arg(&profile_path)
        .arg("--list-generations")
        .output()
        .await
        .map_err(DeactivateError::ListGenError)?;

    match nix_env_list_generations_out.status.code() {
        Some(0) => (),
        a => return Err(DeactivateError::ListGenExitError(a)),
    };

    let generations_list = String::from_utf8(nix_env_list_generations_out.stdout)?;

    let last_generation_line = generations_list
        .lines()
        .last()
        .expect("Expected to find a generation in list");

    let last_generation_id = last_generation_line
        .split_whitespace()
        .next()
        .expect("Expected to get ID from generation entry");

    debug!("Removing generation entry {}", last_generation_line);
    warn!("Removing generation by ID {}", last_generation_id);

    let nix_env_delete_generation_exit_status = Command::new("nix-env")
        .arg("-p")
        .arg(&profile_path)
        .arg("--delete-generations")
        .arg(last_generation_id)
        .status()
        .await
        .map_err(DeactivateError::DeleteGenError)?;

    match nix_env_delete_generation_exit_status.code() {
        Some(0) => (),
        a => return Err(DeactivateError::DeleteGenExitError(a)),
    };

    info!("Attempting to re-activate the last generation");

    let re_activate_exit_status = Command::new(format!("{}/deploy-rs-activate", profile_path))
        .env("PROFILE", &profile_path)
        .current_dir(&profile_path)
        .status()
        .await
        .map_err(DeactivateError::ReactivateError)?;

    match re_activate_exit_status.code() {
        Some(0) => (),
        a => return Err(DeactivateError::ReactivateExitError(a)),
    };

    Ok(())
}

#[derive(Error, Debug)]
pub enum ActivationConfirmationError {
    #[error("Failed to create activation confirmation directory: {0}")]
    CreateConfirmDirError(std::io::Error),
    #[error("Failed to create activation confirmation file: {0}")]
    CreateConfirmFileError(std::io::Error),
    #[error("Error forking process: {0}")]
    ForkError(i32),
    #[error("Could not create activation sentinel: {0}")]
    WatcherError(#[from] std::io::Error),
}

#[derive(Error, Debug)]
pub enum DangerZoneError {
    #[error("Timeout elapsed for confirmation")]
    TimesUp,
    #[error("Unix domain socket listener encountered an error: {0}")]
    WatchError(std::io::Error),
}

async fn wait_for_confirmation(
    listener: UnixListener,
    confirm_timeout: u16,
) -> Result<(), DangerZoneError> {
    info!("Waiting for confirmation event...");

    match timeout(
        Duration::from_secs(confirm_timeout as u64),
        listener.accept(),
    )
    .await
    {
        Ok(Ok((conn, _))) => Ok(conn
            .shutdown(Shutdown::Both)
            .map_err(DangerZoneError::WatchError)?),
        Ok(Err(e)) => Err(DangerZoneError::WatchError(e)),
        Err(_) => Err(DangerZoneError::TimesUp),
    }
}

fn lock_path(temp_path: String, closure: String) -> String {
    let lock_hash = &closure["/nix/store/".len()..];
    format!("{}/deploy-rs-canary-{}", temp_path, lock_hash)
}

pub async fn activation_confirmation(
    profile_path: String,
    temp_path: String,
    confirm_timeout: u16,
    closure: String,
) -> Result<(), ActivationConfirmationError> {
    let lock_path = lock_path(temp_path, closure);

    if let Some(parent) = Path::new(&lock_path).parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(ActivationConfirmationError::CreateConfirmDirError)?;
    }

    let listener = UnixListener::bind(&lock_path)?;

    if let fork::Fork::Child =
        fork::daemon(false, false).map_err(ActivationConfirmationError::ForkError)?
    {
        std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();

                rt.block_on(async move {
                    if let Err(err) = wait_for_confirmation(listener, confirm_timeout).await {
                        if let Err(err) = deactivate(&profile_path).await {
                            good_panic!("Error de-activating due to another error in confirmation thread, oh no...: {}", err);
                        }

                        good_panic!("Error in confirmation: {}", err);
                    }
                });
            })
            .join()
            .unwrap();

        info!("Confirmation successful!");
        std::process::exit(0);
    }
    Ok(())
}

#[derive(Error, Debug)]
pub enum ActivateError {
    #[error("Failed to execute the command for setting profile: {0}")]
    SetProfileError(std::io::Error),
    #[error("The command for setting profile resulted in a bad exit code: {0:?}")]
    SetProfileExitError(Option<i32>),

    #[error("Failed to execute the activation script: {0}")]
    RunActivateError(std::io::Error),
    #[error("The activation script resulted in a bad exit code: {0:?}")]
    RunActivateExitError(Option<i32>),

    #[error("There was an error de-activating after an error was encountered: {0}")]
    DeactivateError(#[from] DeactivateError),

    #[error("Failed to get activation confirmation: {0}")]
    ActivationConfirmationError(#[from] ActivationConfirmationError),
}

pub async fn activate(
    profile_path: String,
    closure: String,
    auto_rollback: bool,
    temp_path: String,
    confirm_timeout: u16,
    magic_rollback: bool,
) -> Result<(), ActivateError> {
    info!("Activating profile");

    let nix_env_set_exit_status = Command::new("nix-env")
        .arg("-p")
        .arg(&profile_path)
        .arg("--set")
        .arg(&closure)
        .status()
        .await
        .map_err(ActivateError::SetProfileError)?;

    match nix_env_set_exit_status.code() {
        Some(0) => (),
        a => {
            if auto_rollback {
                deactivate(&profile_path).await?;
            }
            return Err(ActivateError::SetProfileExitError(a));
        }
    };

    let activate_status = match Command::new(format!("{}/deploy-rs-activate", profile_path))
        .env("PROFILE", &profile_path)
        .current_dir(&profile_path)
        .status()
        .await
        .map_err(ActivateError::RunActivateError)
    {
        Ok(x) => x,
        Err(e) => {
            if auto_rollback {
                deactivate(&profile_path).await?;
            }
            return Err(e);
        }
    };

    match activate_status.code() {
        Some(0) => (),
        a => {
            if auto_rollback {
                deactivate(&profile_path).await?;
            }
            return Err(ActivateError::RunActivateExitError(a));
        }
    };

    info!("Activation succeeded!");

    if magic_rollback {
        info!("Magic rollback is enabled, setting up confirmation hook...");
        match activation_confirmation(profile_path.clone(), temp_path, confirm_timeout, closure)
            .await
        {
            Ok(()) => {}
            Err(err) => {
                deactivate(&profile_path).await?;
                return Err(ActivateError::ActivationConfirmationError(err));
            }
        };
    }

    Ok(())
}

#[derive(Error, Debug)]
pub enum ConfirmError {
    #[error("Could not connect to activation socket: {0}")]
    ConnectError(std::io::Error),

    #[error("Did not get acknowledged by confirmation process: {0}")]
    CouldNotRead(std::io::Error),
}

async fn confirm(temp_path: String, closure: String) -> Result<(), ConfirmError> {
    let mut buf = [0; 1];
    match UnixStream::connect(lock_path(temp_path, closure)).await {
        Err(err) => Err(ConfirmError::ConnectError(err)),
        Ok(mut conn) => match conn.read(&mut buf[..]).await {
            Err(e) => Err(ConfirmError::CouldNotRead(e)),
            Ok(_) => Ok(()),
        },
    }
}

#[derive(Error, Debug)]
enum CommandError {
    #[error("Could not activate: {0}")]
    CanNotActivate(#[from] ActivateError),

    #[error("Could not confirm deployment; you may have to roll back yourself: {0}")]
    CanNotConfirm(#[from] ConfirmError),
}

async fn command(cmd: Opts) -> Result<(), CommandError> {
    match cmd {
        Opts::Activate(opts) => {
            activate(
                opts.profile_path,
                opts.closure,
                opts.auto_rollback,
                opts.temp_path,
                opts.confirm_timeout,
                opts.magic_rollback,
            )
            .await?;
        }
        Opts::Confirm(opts) => {
            if !opts.magic_rollback {
                good_panic!("Trying to confirm a deployment that should not use magic rollback");
            }
            confirm(opts.temp_path, opts.closure).await?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("DEPLOY_LOG").is_err() {
        std::env::set_var("DEPLOY_LOG", "info");
    }

    pretty_env_logger::init_custom_env("DEPLOY_LOG");

    if let Err(err) = command(Opts::parse()).await {
        good_panic!("{}", err);
    }

    Ok(())
}
