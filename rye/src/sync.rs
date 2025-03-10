use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use std::{env, fs};

use anyhow::{bail, Context, Error};
use console::style;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::bootstrap::{ensure_self_venv, fetch, get_pip_module};
use crate::config::{get_py_bin, load_python_version};
use crate::lock::{
    update_single_project_lockfile, update_workspace_lockfile, LockMode, LockOptions,
};
use crate::pyproject::PyProject;
use crate::sources::PythonVersion;
use crate::utils::CommandOutput;

/// Controls the sync mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum SyncMode {
    /// Just ensures Python is there
    #[default]
    PythonOnly,
    /// Lock only
    LockOnly,
    /// Update dependencies
    Regular,
    /// recreate everything
    Full,
}

/// Updates the virtualenv based on the pyproject.toml
#[derive(Debug, Default)]
pub struct SyncOptions {
    /// How verbose should the sync be?
    pub output: CommandOutput,
    /// Include dev dependencies?
    pub dev: bool,
    /// Which sync mode should be used?
    pub mode: SyncMode,
    /// Forces venv creation even when unsafe.
    pub force: bool,
    /// Controls locking.
    pub lock_options: LockOptions,
}

impl SyncOptions {
    /// Only sync the Python itself.
    pub fn python_only() -> SyncOptions {
        SyncOptions {
            mode: SyncMode::PythonOnly,
            ..Default::default()
        }
    }
}

/// Config written into the virtualenv for sync purposes.
#[derive(Serialize, Deserialize, Debug)]
struct VenvMarker {
    python: PythonVersion,
}

/// Synchronizes a project's virtualenv.
pub fn sync(cmd: SyncOptions) -> Result<(), Error> {
    let pyproject = PyProject::discover()?;
    let lockfile = pyproject.workspace_path().join("requirements.lock");
    let dev_lockfile = pyproject.workspace_path().join("requirements-dev.lock");
    let venv = pyproject.venv_path();
    let py_ver = load_python_version().unwrap_or_else(PythonVersion::latest_cpython);
    let marker_file = venv.join("rye-venv.json");
    let output = cmd.output;

    // ensure we are bootstrapped
    let self_venv = ensure_self_venv(output).context("could not sync because bootstrap failed")?;

    let mut recreate = cmd.mode == SyncMode::Full;
    if venv.is_dir() {
        if marker_file.is_file() {
            let contents = fs::read(&marker_file).context("could not read venv marker file")?;
            let marker: VenvMarker =
                serde_json::from_slice(&contents).context("malformed venv marker file")?;
            if marker.python != py_ver {
                if cmd.output != CommandOutput::Quiet {
                    eprintln!(
                        "Python version mismatch (found {}, expect {}), recreating.",
                        marker.python, py_ver
                    );
                }
                recreate = true;
            }
        } else if cmd.force {
            if cmd.output != CommandOutput::Quiet {
                eprintln!("Forcing re-creation of non rye managed virtualenv");
            }
            recreate = true;
        } else {
            bail!("virtualenv is not managed by rye. Run `rye sync -f` to force.");
        }
    }

    // make sure we have a compatible python version
    let py_ver =
        fetch(&py_ver.into(), output).context("failed fetching toolchain ahead of sync")?;

    // kill the virtualenv if it's there and we need to get rid of it.
    if recreate {
        fs::remove_dir_all(&venv).ok();
    }

    if venv.is_dir() {
        // we only care about this output if regular syncs are used
        if !matches!(cmd.mode, SyncMode::PythonOnly | SyncMode::LockOnly)
            && output != CommandOutput::Quiet
        {
            eprintln!("Reusing already existing virtualenv");
        }
    } else {
        if output != CommandOutput::Quiet {
            eprintln!(
                "Initializing new virtualenv in {}",
                style(venv.display()).cyan()
            );
            eprintln!("Python version: {}", style(&py_ver).cyan());
        }
        create_virtualenv(output, &self_venv, &py_ver, &venv)
            .context("failed creating virtualenv ahead of sync")?;
        fs::write(
            &marker_file,
            serde_json::to_string_pretty(&VenvMarker { python: py_ver })?,
        )
        .context("failed writing venv marker file")?;
    }

    // prepare necessary utilities for pip-sync.  This is a super crude
    // hack to make this work for now.  We basically sym-link pip itself
    // into a folder all by itself and place a second file in there which we
    // can pass to pip-sync to install the local package.
    if recreate || cmd.mode != SyncMode::PythonOnly {
        let dir = TempDir::new()?;
        symlink(get_pip_module(&self_venv), dir.path().join("pip"))
            .context("failed linking pip module into for pip-sync")?;

        if let Some(workspace) = pyproject.workspace() {
            // make sure we have an up-to-date lockfile
            update_workspace_lockfile(
                workspace,
                LockMode::Production,
                &lockfile,
                cmd.output,
                &cmd.lock_options,
            )
            .context("could not write production lockfile for workspace")?;
            update_workspace_lockfile(
                workspace,
                LockMode::Dev,
                &dev_lockfile,
                cmd.output,
                &cmd.lock_options,
            )
            .context("could not write dev lockfile for workspace")?;
        } else {
            // make sure we have an up-to-date lockfile
            update_single_project_lockfile(
                &pyproject,
                LockMode::Production,
                &lockfile,
                cmd.output,
                &cmd.lock_options,
            )
            .context("could not write production lockfile for project")?;
            update_single_project_lockfile(
                &pyproject,
                LockMode::Dev,
                &dev_lockfile,
                cmd.output,
                &cmd.lock_options,
            )
            .context("could not write dev lockfile for project")?;
        }

        // run pip install with the lockfile.
        if cmd.mode != SyncMode::LockOnly {
            if output != CommandOutput::Quiet {
                eprintln!("Installing dependencies");
            }
            let mut pip_sync_cmd = Command::new(self_venv.join("bin/pip-sync"));
            pip_sync_cmd
                .env("PYTHONPATH", dir.path())
                .current_dir(pyproject.workspace_path())
                .arg("--python-executable")
                .arg(venv.join("bin/python"))
                // note that the double quotes are necessary to properly handle
                // spaces in paths
                .arg(format!(
                    "--pip-args=\"--python={}\"",
                    venv.join("bin/python").display()
                ));

            if cmd.dev && dev_lockfile.is_file() {
                pip_sync_cmd.arg(&dev_lockfile);
            } else {
                pip_sync_cmd.arg(&lockfile);
            }

            if output == CommandOutput::Verbose {
                pip_sync_cmd.arg("--verbose");
                if env::var("PIP_VERBOSE").is_err() {
                    pip_sync_cmd.env("PIP_VERBOSE", "2");
                }
            } else if output != CommandOutput::Quiet {
                pip_sync_cmd.env("PYTHONWARNINGS", "ignore");
            } else {
                pip_sync_cmd.arg("-q");
            }
            let status = pip_sync_cmd.status().context("unable to run pip-sync")?;
            if !status.success() {
                bail!("Installation of dependencies failed");
            }
        }
    }

    if output != CommandOutput::Quiet && cmd.mode != SyncMode::PythonOnly {
        eprintln!("Done!");
    }

    Ok(())
}

pub fn create_virtualenv(
    output: CommandOutput,
    self_venv: &Path,
    py_ver: &PythonVersion,
    venv: &Path,
) -> Result<(), Error> {
    let py_bin = get_py_bin(py_ver)?;
    let mut venv_cmd = Command::new(self_venv.join("bin/virtualenv"));
    if output == CommandOutput::Verbose {
        venv_cmd.arg("--verbose");
    } else {
        venv_cmd.arg("-q");
        venv_cmd.env("PYTHONWARNINGS", "ignore");
    }
    venv_cmd.arg("-p");
    venv_cmd.arg(&py_bin);
    venv_cmd.arg("--no-seed");
    venv_cmd.arg("--");
    venv_cmd.arg(venv);
    let status = venv_cmd
        .status()
        .context("unable to invoke virtualenv command")?;
    if !status.success() {
        bail!("failed to initialize virtualenv");
    }
    Ok(())
}
