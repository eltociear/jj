// Copyright 2024 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Git utilities shared by various commands.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Instant;
use std::{error, iter};

use jj_lib::git::{self, FailedRefExport, FailedRefExportReason, GitImportStats};
use jj_lib::git_backend::GitBackend;
use jj_lib::store::Store;

use crate::cli_util::{user_error, CommandError};
use crate::progress::Progress;
use crate::ui::Ui;

pub fn get_git_repo(store: &Store) -> Result<git2::Repository, CommandError> {
    match store.backend_impl().downcast_ref::<GitBackend>() {
        None => Err(user_error("The repo is not backed by a git repo")),
        Some(git_backend) => Ok(git_backend.open_git_repo()?),
    }
}

fn terminal_get_username(ui: &mut Ui, url: &str) -> Option<String> {
    ui.prompt(&format!("Username for {url}")).ok()
}

fn terminal_get_pw(ui: &mut Ui, url: &str) -> Option<String> {
    ui.prompt_password(&format!("Passphrase for {url}: ")).ok()
}

fn pinentry_get_pw(url: &str) -> Option<String> {
    // https://www.gnupg.org/documentation/manuals/assuan/Server-responses.html#Server-responses
    fn decode_assuan_data(encoded: &str) -> Option<String> {
        let encoded = encoded.as_bytes();
        let mut decoded = Vec::with_capacity(encoded.len());
        let mut i = 0;
        while i < encoded.len() {
            if encoded[i] != b'%' {
                decoded.push(encoded[i]);
                i += 1;
                continue;
            }
            i += 1;
            let byte =
                u8::from_str_radix(std::str::from_utf8(encoded.get(i..i + 2)?).ok()?, 16).ok()?;
            decoded.push(byte);
            i += 2;
        }
        String::from_utf8(decoded).ok()
    }

    let mut pinentry = std::process::Command::new("pinentry")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    #[rustfmt::skip]
    pinentry
        .stdin
        .take()
        .unwrap()
        .write_all(
            format!(
                "SETTITLE jj passphrase\n\
                 SETDESC Enter passphrase for {url}\n\
                 SETPROMPT Passphrase:\n\
                 GETPIN\n"
            )
            .as_bytes(),
        )
        .ok()?;
    let mut out = String::new();
    pinentry
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .ok()?;
    _ = pinentry.wait();
    for line in out.split('\n') {
        if !line.starts_with("D ") {
            continue;
        }
        let (_, encoded) = line.split_at(2);
        return decode_assuan_data(encoded);
    }
    None
}

#[tracing::instrument]
fn get_ssh_keys(_username: &str) -> Vec<PathBuf> {
    let mut paths = vec![];
    if let Some(home_dir) = dirs::home_dir() {
        let ssh_dir = Path::new(&home_dir).join(".ssh");
        for filename in ["id_ed25519_sk", "id_ed25519", "id_rsa"] {
            let key_path = ssh_dir.join(filename);
            if key_path.is_file() {
                tracing::info!(path = ?key_path, "found ssh key");
                paths.push(key_path);
            }
        }
    }
    if paths.is_empty() {
        tracing::info!("no ssh key found");
    }
    paths
}

pub fn with_remote_git_callbacks<T>(
    ui: &mut Ui,
    f: impl FnOnce(git::RemoteCallbacks<'_>) -> T,
) -> T {
    let mut ui = Mutex::new(ui);
    let mut callback = None;
    if let Some(mut output) = ui.get_mut().unwrap().progress_output() {
        let mut progress = Progress::new(Instant::now());
        callback = Some(move |x: &git::Progress| {
            _ = progress.update(Instant::now(), x, &mut output);
        });
    }
    let mut callbacks = git::RemoteCallbacks::default();
    callbacks.progress = callback
        .as_mut()
        .map(|x| x as &mut dyn FnMut(&git::Progress));
    let mut get_ssh_keys = get_ssh_keys; // Coerce to unit fn type
    callbacks.get_ssh_keys = Some(&mut get_ssh_keys);
    let mut get_pw = |url: &str, _username: &str| {
        pinentry_get_pw(url).or_else(|| terminal_get_pw(*ui.lock().unwrap(), url))
    };
    callbacks.get_password = Some(&mut get_pw);
    let mut get_user_pw = |url: &str| {
        let ui = &mut *ui.lock().unwrap();
        Some((terminal_get_username(ui, url)?, terminal_get_pw(ui, url)?))
    };
    callbacks.get_username_password = Some(&mut get_user_pw);
    f(callbacks)
}

pub fn print_git_import_stats(ui: &mut Ui, stats: &GitImportStats) -> Result<(), CommandError> {
    if !stats.abandoned_commits.is_empty() {
        writeln!(
            ui.stderr(),
            "Abandoned {} commits that are no longer reachable.",
            stats.abandoned_commits.len()
        )?;
    }
    Ok(())
}

pub fn print_failed_git_export(
    ui: &Ui,
    failed_branches: &[FailedRefExport],
) -> Result<(), std::io::Error> {
    if !failed_branches.is_empty() {
        writeln!(ui.warning(), "Failed to export some branches:")?;
        let mut formatter = ui.stderr_formatter();
        for FailedRefExport { name, reason } in failed_branches {
            formatter.write_str("  ")?;
            write!(formatter.labeled("branch"), "{name}")?;
            for err in iter::successors(Some(reason as &dyn error::Error), |err| err.source()) {
                write!(formatter, ": {err}")?;
            }
            writeln!(formatter)?;
        }
        drop(formatter);
        if failed_branches
            .iter()
            .any(|failed| matches!(failed.reason, FailedRefExportReason::FailedToSet(_)))
        {
            writeln!(
                ui.hint(),
                r#"Hint: Git doesn't allow a branch name that looks like a parent directory of
another (e.g. `foo` and `foo/bar`). Try to rename the branches that failed to
export or their "parent" branches."#,
            )?;
        }
    }
    Ok(())
}

/// Expands "~/" to "$HOME/" as Git seems to do for e.g. core.excludesFile.
pub fn expand_git_path(path_str: &str) -> PathBuf {
    if let Some(remainder) = path_str.strip_prefix("~/") {
        if let Ok(home_dir_str) = std::env::var("HOME") {
            return PathBuf::from(home_dir_str).join(remainder);
        }
    }
    PathBuf::from(path_str)
}
