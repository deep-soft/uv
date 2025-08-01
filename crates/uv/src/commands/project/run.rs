use std::borrow::Cow;
use std::env::VarError;
use std::ffi::OsString;
use std::fmt::Write;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use futures::StreamExt;
use itertools::Itertools;
use owo_colors::OwoColorize;
use thiserror::Error;
use tokio::process::Command;
use tracing::{debug, trace, warn};
use url::Url;

use uv_cache::Cache;
use uv_cli::ExternalCommand;
use uv_client::BaseClientBuilder;
use uv_configuration::{
    Concurrency, Constraints, DependencyGroups, DryRun, EditableMode, ExtrasSpecification,
    InstallOptions, Preview,
};
use uv_distribution_types::Requirement;
use uv_fs::which::is_executable;
use uv_fs::{PythonExt, Simplified, create_symlink};
use uv_installer::{SatisfiesResult, SitePackages};
use uv_normalize::{DefaultExtras, DefaultGroups, PackageName};
use uv_python::{
    EnvironmentPreference, Interpreter, PyVenvConfiguration, PythonDownloads, PythonEnvironment,
    PythonInstallation, PythonPreference, PythonRequest, PythonVersionFile,
    VersionFileDiscoveryOptions,
};
use uv_redacted::DisplaySafeUrl;
use uv_requirements::{RequirementsSource, RequirementsSpecification};
use uv_resolver::{Installable, Lock, Preference};
use uv_scripts::Pep723Item;
use uv_settings::PythonInstallMirrors;
use uv_shell::runnable::WindowsRunnable;
use uv_static::EnvVars;
use uv_warnings::warn_user;
use uv_workspace::{DiscoveryOptions, VirtualProject, Workspace, WorkspaceCache, WorkspaceError};

use crate::child::run_to_completion;
use crate::commands::pip::loggers::{
    DefaultInstallLogger, DefaultResolveLogger, SummaryInstallLogger, SummaryResolveLogger,
};
use crate::commands::pip::operations::Modifications;
use crate::commands::project::environment::{CachedEnvironment, EphemeralEnvironment};
use crate::commands::project::install_target::InstallTarget;
use crate::commands::project::lock::LockMode;
use crate::commands::project::lock_target::LockTarget;
use crate::commands::project::{
    EnvironmentSpecification, PreferenceLocation, ProjectEnvironment, ProjectError,
    ScriptEnvironment, ScriptInterpreter, UniversalState, WorkspacePython,
    default_dependency_groups, script_extra_build_requires, script_specification,
    update_environment, validate_project_requires_python,
};
use crate::commands::reporters::PythonDownloadReporter;
use crate::commands::{ExitStatus, diagnostics, project};
use crate::printer::Printer;
use crate::settings::{NetworkSettings, ResolverInstallerSettings};

/// Run a command.
#[allow(clippy::fn_params_excessive_bools)]
pub(crate) async fn run(
    project_dir: &Path,
    script: Option<Pep723Item>,
    command: Option<RunCommand>,
    requirements: Vec<RequirementsSource>,
    show_resolution: bool,
    locked: bool,
    frozen: bool,
    active: Option<bool>,
    no_sync: bool,
    isolated: bool,
    all_packages: bool,
    package: Option<PackageName>,
    no_project: bool,
    no_config: bool,
    extras: ExtrasSpecification,
    groups: DependencyGroups,
    editable: EditableMode,
    modifications: Modifications,
    python: Option<String>,
    install_mirrors: PythonInstallMirrors,
    settings: ResolverInstallerSettings,
    network_settings: NetworkSettings,
    python_preference: PythonPreference,
    python_downloads: PythonDownloads,
    installer_metadata: bool,
    concurrency: Concurrency,
    cache: &Cache,
    printer: Printer,
    env_file: Vec<PathBuf>,
    no_env_file: bool,
    preview: Preview,
    max_recursion_depth: u32,
) -> anyhow::Result<ExitStatus> {
    // Check if max recursion depth was exceeded. This most commonly happens
    // for scripts with a shebang line like `#!/usr/bin/env -S uv run`, so try
    // to provide guidance for that case.
    let recursion_depth = read_recursion_depth_from_environment_variable()?;
    if recursion_depth > max_recursion_depth {
        bail!(
            r"
`uv run` was recursively invoked {recursion_depth} times which exceeds the limit of {max_recursion_depth}.

hint: If you are running a script with `{}` in the shebang, you may need to include the `{}` flag.",
            "uv run".green(),
            "--script".green(),
        );
    }

    // These cases seem quite complex because (in theory) they should change the "current package".
    // Let's ban them entirely for now.
    let mut requirements_from_stdin: bool = false;
    for source in &requirements {
        match source {
            RequirementsSource::PyprojectToml(_) => {
                bail!("Adding requirements from a `pyproject.toml` is not supported in `uv run`");
            }
            RequirementsSource::SetupPy(_) => {
                bail!("Adding requirements from a `setup.py` is not supported in `uv run`");
            }
            RequirementsSource::SetupCfg(_) => {
                bail!("Adding requirements from a `setup.cfg` is not supported in `uv run`");
            }
            RequirementsSource::RequirementsTxt(path) => {
                if path == Path::new("-") {
                    requirements_from_stdin = true;
                }
            }
            _ => {}
        }
    }

    // Fail early if stdin is used for multiple purposes.
    if matches!(
        command,
        Some(RunCommand::PythonStdin(..) | RunCommand::PythonGuiStdin(..))
    ) && requirements_from_stdin
    {
        bail!("Cannot read both requirements file and script from stdin");
    }

    // Initialize any shared state.
    let lock_state = UniversalState::default();
    let sync_state = lock_state.fork();
    let workspace_cache = WorkspaceCache::default();

    // Read from the `.env` file, if necessary.
    if !no_env_file {
        for env_file_path in env_file.iter().rev().map(PathBuf::as_path) {
            match dotenvy::from_path(env_file_path) {
                Err(dotenvy::Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                    bail!(
                        "No environment file found at: `{}`",
                        env_file_path.simplified_display()
                    );
                }
                Err(dotenvy::Error::Io(err)) => {
                    bail!(
                        "Failed to read environment file `{}`: {err}",
                        env_file_path.simplified_display()
                    );
                }
                Err(dotenvy::Error::LineParse(content, position)) => {
                    warn_user!(
                        "Failed to parse environment file `{}` at position {position}: {content}",
                        env_file_path.simplified_display(),
                    );
                }
                Err(err) => {
                    warn_user!(
                        "Failed to parse environment file `{}`: {err}",
                        env_file_path.simplified_display(),
                    );
                }
                Ok(()) => {
                    debug!(
                        "Read environment file at: `{}`",
                        env_file_path.simplified_display()
                    );
                }
            }
        }
    }

    // Initialize any output reporters.
    let download_reporter = PythonDownloadReporter::single(printer);

    // The lockfile used for the base environment.
    let mut base_lock: Option<(Lock, PathBuf)> = None;

    // Determine whether the command to execute is a PEP 723 script.
    let temp_dir;
    let script_interpreter = if let Some(script) = script {
        match &script {
            Pep723Item::Script(script) => {
                debug!(
                    "Reading inline script metadata from `{}`",
                    script.path.user_display()
                );
            }
            Pep723Item::Stdin(..) => {
                if requirements_from_stdin {
                    bail!("Cannot read both requirements file and script from stdin");
                }
                debug!("Reading inline script metadata from stdin");
            }
            Pep723Item::Remote(..) => {
                debug!("Reading inline script metadata from remote URL");
            }
        }

        // If a lockfile already exists, lock the script.
        if let Some(target) = script
            .as_script()
            .map(LockTarget::from)
            .filter(|target| target.lock_path().is_file())
        {
            debug!("Found existing lockfile for script");

            // Discover the interpreter for the script.
            let environment = ScriptEnvironment::get_or_init(
                (&script).into(),
                python.as_deref().map(PythonRequest::parse),
                &network_settings,
                python_preference,
                python_downloads,
                &install_mirrors,
                no_sync,
                no_config,
                active.map_or(Some(false), Some),
                cache,
                DryRun::Disabled,
                printer,
                preview,
            )
            .await?
            .into_environment()?;

            let _lock = environment
                .lock()
                .await
                .inspect_err(|err| {
                    warn!("Failed to acquire environment lock: {err}");
                })
                .ok();

            // Determine the lock mode.
            let mode = if frozen {
                LockMode::Frozen
            } else if locked {
                LockMode::Locked(environment.interpreter())
            } else {
                LockMode::Write(environment.interpreter())
            };

            // Generate a lockfile.
            let lock = match project::lock::LockOperation::new(
                mode,
                &settings.resolver,
                &network_settings,
                &lock_state,
                if show_resolution {
                    Box::new(DefaultResolveLogger)
                } else {
                    Box::new(SummaryResolveLogger)
                },
                concurrency,
                cache,
                &workspace_cache,
                printer,
                preview,
            )
            .execute(target)
            .await
            {
                Ok(result) => result.into_lock(),
                Err(ProjectError::Operation(err)) => {
                    return diagnostics::OperationDiagnostic::native_tls(
                        network_settings.native_tls,
                    )
                    .with_context("script")
                    .report(err)
                    .map_or(Ok(ExitStatus::Failure), |err| Err(err.into()));
                }
                Err(err) => return Err(err.into()),
            };

            // Sync the environment.
            let target = InstallTarget::Script {
                script: script.as_script().unwrap(),
                lock: &lock,
            };

            let install_options = InstallOptions::default();

            match project::sync::do_sync(
                target,
                &environment,
                &extras.with_defaults(DefaultExtras::default()),
                &groups.with_defaults(DefaultGroups::default()),
                editable,
                install_options,
                modifications,
                None,
                (&settings).into(),
                &network_settings,
                &sync_state,
                if show_resolution {
                    Box::new(DefaultInstallLogger)
                } else {
                    Box::new(SummaryInstallLogger)
                },
                installer_metadata,
                concurrency,
                cache,
                workspace_cache.clone(),
                DryRun::Disabled,
                printer,
                preview,
            )
            .await
            {
                Ok(()) => {}
                Err(ProjectError::Operation(err)) => {
                    return diagnostics::OperationDiagnostic::native_tls(
                        network_settings.native_tls,
                    )
                    .with_context("script")
                    .report(err)
                    .map_or(Ok(ExitStatus::Failure), |err| Err(err.into()));
                }
                Err(err) => return Err(err.into()),
            }

            // Respect any locked preferences when resolving `--with` dependencies downstream.
            let install_path = target.install_path().to_path_buf();
            base_lock = Some((lock, install_path));

            Some(environment.into_interpreter())
        } else {
            // If no lockfile is found, warn against `--locked` and `--frozen`.
            if locked {
                warn_user!(
                    "No lockfile found for Python script (ignoring `--locked`); run `{}` to generate a lockfile",
                    "uv lock --script".green(),
                );
            }
            if frozen {
                warn_user!(
                    "No lockfile found for Python script (ignoring `--frozen`); run `{}` to generate a lockfile",
                    "uv lock --script".green(),
                );
            }

            // Install the script requirements, if necessary. Otherwise, use an isolated environment.
            if let Some(spec) = script_specification((&script).into(), &settings.resolver)? {
                let script_extra_build_requires =
                    script_extra_build_requires((&script).into(), &settings.resolver)?;
                let environment = ScriptEnvironment::get_or_init(
                    (&script).into(),
                    python.as_deref().map(PythonRequest::parse),
                    &network_settings,
                    python_preference,
                    python_downloads,
                    &install_mirrors,
                    no_sync,
                    no_config,
                    active.map_or(Some(false), Some),
                    cache,
                    DryRun::Disabled,
                    printer,
                    preview,
                )
                .await?
                .into_environment()?;

                let build_constraints = script
                    .metadata()
                    .tool
                    .as_ref()
                    .and_then(|tool| {
                        tool.uv
                            .as_ref()
                            .and_then(|uv| uv.build_constraint_dependencies.as_ref())
                    })
                    .map(|constraints| {
                        Constraints::from_requirements(
                            constraints
                                .iter()
                                .map(|constraint| Requirement::from(constraint.clone())),
                        )
                    });

                let _lock = environment
                    .lock()
                    .await
                    .inspect_err(|err| {
                        warn!("Failed to acquire environment lock: {err}");
                    })
                    .ok();

                match update_environment(
                    environment,
                    spec,
                    modifications,
                    build_constraints.unwrap_or_default(),
                    script_extra_build_requires,
                    &settings,
                    &network_settings,
                    &sync_state,
                    if show_resolution {
                        Box::new(DefaultResolveLogger)
                    } else {
                        Box::new(SummaryResolveLogger)
                    },
                    if show_resolution {
                        Box::new(DefaultInstallLogger)
                    } else {
                        Box::new(SummaryInstallLogger)
                    },
                    installer_metadata,
                    concurrency,
                    cache,
                    workspace_cache.clone(),
                    DryRun::Disabled,
                    printer,
                    preview,
                )
                .await
                {
                    Ok(update) => Some(update.into_environment().into_interpreter()),
                    Err(ProjectError::Operation(err)) => {
                        return diagnostics::OperationDiagnostic::native_tls(
                            network_settings.native_tls,
                        )
                        .with_context("script")
                        .report(err)
                        .map_or(Ok(ExitStatus::Failure), |err| Err(err.into()));
                    }
                    Err(err) => return Err(err.into()),
                }
            } else {
                // Create a virtual environment.
                let interpreter = ScriptInterpreter::discover(
                    (&script).into(),
                    python.as_deref().map(PythonRequest::parse),
                    &network_settings,
                    python_preference,
                    python_downloads,
                    &install_mirrors,
                    no_sync,
                    no_config,
                    active.map_or(Some(false), Some),
                    cache,
                    printer,
                    preview,
                )
                .await?
                .into_interpreter();

                temp_dir = cache.venv_dir()?;
                let environment = uv_virtualenv::create_venv(
                    temp_dir.path(),
                    interpreter,
                    uv_virtualenv::Prompt::None,
                    false,
                    uv_virtualenv::OnExisting::Remove,
                    false,
                    false,
                    false,
                    preview,
                )?;

                Some(environment.into_interpreter())
            }
        }
    } else {
        None
    };

    // Discover and sync the base environment.
    let temp_dir;
    let base_interpreter = if let Some(script_interpreter) = script_interpreter {
        // If we found a PEP 723 script and the user provided a project-only setting, warn.
        if no_project {
            debug!(
                "`--no-project` is a no-op for Python scripts with inline metadata; ignoring..."
            );
        }
        if !extras.is_empty() {
            warn_user!("Extras are not supported for Python scripts with inline metadata");
        }
        for flag in groups.history().as_flags_pretty() {
            warn_user!("`{flag}` is not supported for Python scripts with inline metadata");
        }
        if all_packages {
            warn_user!(
                "`--all-packages` is a no-op for Python scripts with inline metadata, which always run in isolation"
            );
        }
        if package.is_some() {
            warn_user!(
                "`--package` is a no-op for Python scripts with inline metadata, which always run in isolation"
            );
        }
        if no_sync {
            warn_user!(
                "`--no-sync` is a no-op for Python scripts with inline metadata, which always run in isolation"
            );
        }
        if isolated {
            warn_user!(
                "`--isolated` is a no-op for Python scripts with inline metadata, which always run in isolation"
            );
        }

        script_interpreter
    } else {
        let project = if let Some(package) = package.as_ref() {
            // We need a workspace, but we don't need to have a current package, we can be e.g. in
            // the root of a virtual workspace and then switch into the selected package.
            Some(VirtualProject::Project(
                Workspace::discover(project_dir, &DiscoveryOptions::default(), &workspace_cache)
                    .await?
                    .with_current_project(package.clone())
                    .with_context(|| format!("Package `{package}` not found in workspace"))?,
            ))
        } else {
            match VirtualProject::discover(
                project_dir,
                &DiscoveryOptions::default(),
                &workspace_cache,
            )
            .await
            {
                Ok(project) => {
                    if no_project {
                        debug!("Ignoring discovered project due to `--no-project`");
                        None
                    } else {
                        Some(project)
                    }
                }
                Err(WorkspaceError::MissingPyprojectToml | WorkspaceError::NonWorkspace(_)) => {
                    // If the user runs with `--no-project` and we can't find a project, warn.
                    if no_project {
                        warn!("`--no-project` was provided, but no project was found");
                    }
                    None
                }
                Err(err) => {
                    // If the user runs with `--no-project`, ignore the error.
                    if no_project {
                        warn!("Ignoring project discovery error due to `--no-project`: {err}");
                        None
                    } else {
                        return Err(err.into());
                    }
                }
            }
        };

        if no_project {
            // If the user ran with `--no-project` and provided a project-only setting, warn.
            for flag in extras.history().as_flags_pretty() {
                warn_user!("`{flag}` has no effect when used alongside `--no-project`");
            }
            for flag in groups.history().as_flags_pretty() {
                warn_user!("`{flag}` has no effect when used alongside `--no-project`");
            }
            if locked {
                warn_user!("`--locked` has no effect when used alongside `--no-project`");
            }
            if frozen {
                warn_user!("`--frozen` has no effect when used alongside `--no-project`");
            }
            if no_sync {
                warn_user!("`--no-sync` has no effect when used alongside `--no-project`");
            }
        } else if project.is_none() {
            // If we can't find a project and the user provided a project-only setting, warn.
            for flag in extras.history().as_flags_pretty() {
                warn_user!("`{flag}` has no effect when used outside of a project");
            }
            for flag in groups.history().as_flags_pretty() {
                warn_user!("`{flag}` has no effect when used outside of a project");
            }
            if locked {
                warn_user!("`--locked` has no effect when used outside of a project");
            }
            if no_sync {
                warn_user!("`--no-sync` has no effect when used outside of a project");
            }
        }

        if let Some(project) = project {
            if let Some(project_name) = project.project_name() {
                debug!(
                    "Discovered project `{project_name}` at: {}",
                    project.workspace().install_path().display()
                );
            } else {
                debug!(
                    "Discovered virtual workspace at: {}",
                    project.workspace().install_path().display()
                );
            }
            // Determine the groups and extras to include.
            let default_groups = default_dependency_groups(project.pyproject_toml())?;
            let default_extras = DefaultExtras::default();
            let groups = groups.with_defaults(default_groups);
            let extras = extras.with_defaults(default_extras);

            let venv = if isolated {
                debug!("Creating isolated virtual environment");

                // If we're isolating the environment, use an ephemeral virtual environment as the
                // base environment for the project.
                let client_builder = BaseClientBuilder::new()
                    .retries_from_env()?
                    .connectivity(network_settings.connectivity)
                    .native_tls(network_settings.native_tls)
                    .allow_insecure_host(network_settings.allow_insecure_host.clone());

                // Resolve the Python request and requirement for the workspace.
                let WorkspacePython {
                    source,
                    python_request,
                    requires_python,
                } = WorkspacePython::from_request(
                    python.as_deref().map(PythonRequest::parse),
                    Some(project.workspace()),
                    &groups,
                    project_dir,
                    no_config,
                )
                .await?;

                let interpreter = PythonInstallation::find_or_download(
                    python_request.as_ref(),
                    EnvironmentPreference::Any,
                    python_preference,
                    python_downloads,
                    &client_builder,
                    cache,
                    Some(&download_reporter),
                    install_mirrors.python_install_mirror.as_deref(),
                    install_mirrors.pypy_install_mirror.as_deref(),
                    install_mirrors.python_downloads_json_url.as_deref(),
                    preview,
                )
                .await?
                .into_interpreter();

                if let Some(requires_python) = requires_python.as_ref() {
                    validate_project_requires_python(
                        &interpreter,
                        Some(project.workspace()),
                        &groups,
                        requires_python,
                        &source,
                    )?;
                }

                // Create a virtual environment
                temp_dir = cache.venv_dir()?;
                uv_virtualenv::create_venv(
                    temp_dir.path(),
                    interpreter,
                    uv_virtualenv::Prompt::None,
                    false,
                    uv_virtualenv::OnExisting::Remove,
                    false,
                    false,
                    false,
                    preview,
                )?
            } else {
                // If we're not isolating the environment, reuse the base environment for the
                // project.
                ProjectEnvironment::get_or_init(
                    project.workspace(),
                    &groups,
                    python.as_deref().map(PythonRequest::parse),
                    &install_mirrors,
                    &network_settings,
                    python_preference,
                    python_downloads,
                    no_sync,
                    no_config,
                    active,
                    cache,
                    DryRun::Disabled,
                    printer,
                    preview,
                )
                .await?
                .into_environment()?
            };

            if no_sync {
                debug!("Skipping environment synchronization due to `--no-sync`");

                // If we're not syncing, we should still attempt to respect the locked preferences
                // in any `--with` requirements.
                if !isolated && !requirements.is_empty() {
                    base_lock = LockTarget::from(project.workspace())
                        .read()
                        .await
                        .ok()
                        .flatten()
                        .map(|lock| (lock, project.workspace().install_path().to_owned()));
                }
            } else {
                let _lock = venv
                    .lock()
                    .await
                    .inspect_err(|err| {
                        warn!("Failed to acquire environment lock: {err}");
                    })
                    .ok();

                // Determine the lock mode.
                let mode = if frozen {
                    LockMode::Frozen
                } else if locked {
                    LockMode::Locked(venv.interpreter())
                } else {
                    LockMode::Write(venv.interpreter())
                };

                let result = match project::lock::LockOperation::new(
                    mode,
                    &settings.resolver,
                    &network_settings,
                    &lock_state,
                    if show_resolution {
                        Box::new(DefaultResolveLogger)
                    } else {
                        Box::new(SummaryResolveLogger)
                    },
                    concurrency,
                    cache,
                    &workspace_cache,
                    printer,
                    preview,
                )
                .execute(project.workspace().into())
                .await
                {
                    Ok(result) => result,
                    Err(ProjectError::Operation(err)) => {
                        return diagnostics::OperationDiagnostic::native_tls(
                            network_settings.native_tls,
                        )
                        .report(err)
                        .map_or(Ok(ExitStatus::Failure), |err| Err(err.into()));
                    }
                    Err(err) => return Err(err.into()),
                };

                // Identify the installation target.
                let target = match &project {
                    VirtualProject::Project(project) => {
                        if all_packages {
                            InstallTarget::Workspace {
                                workspace: project.workspace(),
                                lock: result.lock(),
                            }
                        } else if let Some(package) = package.as_ref() {
                            InstallTarget::Project {
                                workspace: project.workspace(),
                                name: package,
                                lock: result.lock(),
                            }
                        } else {
                            // By default, install the root package.
                            InstallTarget::Project {
                                workspace: project.workspace(),
                                name: project.project_name(),
                                lock: result.lock(),
                            }
                        }
                    }
                    VirtualProject::NonProject(workspace) => {
                        if all_packages {
                            InstallTarget::NonProjectWorkspace {
                                workspace,
                                lock: result.lock(),
                            }
                        } else if let Some(package) = package.as_ref() {
                            InstallTarget::Project {
                                workspace,
                                name: package,
                                lock: result.lock(),
                            }
                        } else {
                            // By default, install the entire workspace.
                            InstallTarget::NonProjectWorkspace {
                                workspace,
                                lock: result.lock(),
                            }
                        }
                    }
                };

                let install_options = InstallOptions::default();
                // Validate that the set of requested extras and development groups are defined in the lockfile.
                target.validate_extras(&extras)?;
                target.validate_groups(&groups)?;

                match project::sync::do_sync(
                    target,
                    &venv,
                    &extras,
                    &groups,
                    editable,
                    install_options,
                    modifications,
                    None,
                    (&settings).into(),
                    &network_settings,
                    &sync_state,
                    if show_resolution {
                        Box::new(DefaultInstallLogger)
                    } else {
                        Box::new(SummaryInstallLogger)
                    },
                    installer_metadata,
                    concurrency,
                    cache,
                    workspace_cache.clone(),
                    DryRun::Disabled,
                    printer,
                    preview,
                )
                .await
                {
                    Ok(()) => {}
                    Err(ProjectError::Operation(err)) => {
                        return diagnostics::OperationDiagnostic::native_tls(
                            network_settings.native_tls,
                        )
                        .report(err)
                        .map_or(Ok(ExitStatus::Failure), |err| Err(err.into()));
                    }
                    Err(err) => return Err(err.into()),
                }

                base_lock = Some((
                    result.into_lock(),
                    project.workspace().install_path().to_owned(),
                ));
            }

            venv.into_interpreter()
        } else {
            debug!("No project found; searching for Python interpreter");

            let interpreter = {
                let client_builder = BaseClientBuilder::new()
                    .retries_from_env()?
                    .connectivity(network_settings.connectivity)
                    .native_tls(network_settings.native_tls)
                    .allow_insecure_host(network_settings.allow_insecure_host.clone());

                // (1) Explicit request from user
                let python_request = if let Some(request) = python.as_deref() {
                    Some(PythonRequest::parse(request))
                // (2) Request from `.python-version`
                } else {
                    PythonVersionFile::discover(
                        &project_dir,
                        &VersionFileDiscoveryOptions::default().with_no_config(no_config),
                    )
                    .await?
                    .and_then(PythonVersionFile::into_version)
                };

                let python = PythonInstallation::find_or_download(
                    python_request.as_ref(),
                    // No opt-in is required for system environments, since we are not mutating it.
                    EnvironmentPreference::Any,
                    python_preference,
                    python_downloads,
                    &client_builder,
                    cache,
                    Some(&download_reporter),
                    install_mirrors.python_install_mirror.as_deref(),
                    install_mirrors.pypy_install_mirror.as_deref(),
                    install_mirrors.python_downloads_json_url.as_deref(),
                    preview,
                )
                .await?;

                python.into_interpreter()
            };

            if isolated {
                debug!("Creating isolated virtual environment");

                // If we're isolating the environment, use an ephemeral virtual environment.
                temp_dir = cache.venv_dir()?;
                let venv = uv_virtualenv::create_venv(
                    temp_dir.path(),
                    interpreter,
                    uv_virtualenv::Prompt::None,
                    false,
                    uv_virtualenv::OnExisting::Remove,
                    false,
                    false,
                    false,
                    preview,
                )?;
                venv.into_interpreter()
            } else {
                interpreter
            }
        }
    };

    debug!(
        "Using Python {} interpreter at: {}",
        base_interpreter.python_version(),
        base_interpreter.sys_executable().display()
    );

    // Read the requirements.
    let spec = if requirements.is_empty() {
        None
    } else {
        let client_builder = BaseClientBuilder::new()
            .retries_from_env()?
            .connectivity(network_settings.connectivity)
            .native_tls(network_settings.native_tls)
            .allow_insecure_host(network_settings.allow_insecure_host.clone());

        let spec =
            RequirementsSpecification::from_simple_sources(&requirements, &client_builder).await?;

        Some(spec)
    };

    // If necessary, create an environment for the ephemeral requirements or command.
    let base_site_packages = SitePackages::from_interpreter(&base_interpreter)?;
    let requirements_env = match spec {
        None => None,
        Some(spec)
            if can_skip_ephemeral(&spec, &base_interpreter, &base_site_packages, &settings) =>
        {
            None
        }
        Some(spec) => {
            debug!("Syncing `--with` requirements to cached environment");

            // Read the build constraints from the lock file.
            let build_constraints = base_lock
                .as_ref()
                .map(|(lock, path)| lock.build_constraints(path));

            // Read the preferences.
            let spec = EnvironmentSpecification::from(spec).with_preferences(
                if let Some((lock, install_path)) = base_lock.as_ref() {
                    // If we have a lockfile, use the locked versions as preferences.
                    PreferenceLocation::Lock { lock, install_path }
                } else {
                    // Otherwise, extract preferences from the base environment.
                    PreferenceLocation::Entries(
                        base_site_packages
                            .iter()
                            .filter_map(Preference::from_installed)
                            .collect::<Vec<_>>(),
                    )
                },
            );

            let result = CachedEnvironment::from_spec(
                spec,
                build_constraints.unwrap_or_default(),
                &base_interpreter,
                &settings,
                &network_settings,
                &sync_state,
                if show_resolution {
                    Box::new(DefaultResolveLogger)
                } else {
                    Box::new(SummaryResolveLogger)
                },
                if show_resolution {
                    Box::new(DefaultInstallLogger)
                } else {
                    Box::new(SummaryInstallLogger)
                },
                installer_metadata,
                concurrency,
                cache,
                printer,
                preview,
            )
            .await;

            let environment = match result {
                Ok(resolution) => resolution,
                Err(ProjectError::Operation(err)) => {
                    return diagnostics::OperationDiagnostic::native_tls(
                        network_settings.native_tls,
                    )
                    .with_context("`--with`")
                    .report(err)
                    .map_or(Ok(ExitStatus::Failure), |err| Err(err.into()));
                }
                Err(err) => return Err(err.into()),
            };

            Some(PythonEnvironment::from(environment))
        }
    };

    // If we're layering requirements atop the project environment, run the command in an ephemeral,
    // isolated environment. Otherwise, modifications to the "active virtual environment" would
    // poison the cache.
    let ephemeral_dir = requirements_env
        .as_ref()
        .map(|_| cache.venv_dir())
        .transpose()?;

    let ephemeral_env = ephemeral_dir
        .as_ref()
        .map(|dir| {
            debug!(
                "Creating ephemeral environment at: `{}`",
                dir.path().simplified_display()
            );

            uv_virtualenv::create_venv(
                dir.path(),
                base_interpreter.clone(),
                uv_virtualenv::Prompt::None,
                false,
                uv_virtualenv::OnExisting::Remove,
                false,
                false,
                false,
                preview,
            )
        })
        .transpose()?
        .map(EphemeralEnvironment::from);

    // If we're running in an ephemeral environment, add a path file to enable loading from the
    // `--with` requirements environment and the project environment site packages.
    //
    // Setting `PYTHONPATH` is insufficient, as it doesn't resolve `.pth` files in the base
    // environment. Adding `sitecustomize.py` would be an alternative, but it can be shadowed by an
    // existing such module in the python installation.
    if let Some(ephemeral_env) = ephemeral_env.as_ref() {
        if let Some(requirements_env) = requirements_env.as_ref() {
            let requirements_site_packages =
                requirements_env.site_packages().next().ok_or_else(|| {
                    anyhow!("Requirements environment has no site packages directory")
                })?;
            let base_site_packages = base_interpreter
                .site_packages()
                .next()
                .ok_or_else(|| anyhow!("Base environment has no site packages directory"))?;

            ephemeral_env.set_overlay(format!(
                "import site; site.addsitedir(\"{}\"); site.addsitedir(\"{}\");",
                requirements_site_packages.escape_for_python(),
                base_site_packages.escape_for_python(),
            ))?;

            // N.B. The order here matters — earlier interpreters take precedence over the
            // later ones.
            for interpreter in [requirements_env.interpreter(), &base_interpreter] {
                // Copy each entrypoint from the base environments to the ephemeral environment,
                // updating the Python executable target to ensure they run in the ephemeral
                // environment.
                for entry in fs_err::read_dir(interpreter.scripts())? {
                    let entry = entry?;
                    if !entry.file_type()?.is_file() {
                        continue;
                    }
                    match copy_entrypoint(
                        &entry.path(),
                        &ephemeral_env.scripts().join(entry.file_name()),
                        interpreter.sys_executable(),
                        ephemeral_env.sys_executable(),
                    ) {
                        Ok(()) => {}
                        // If the entrypoint already exists, skip it.
                        Err(CopyEntrypointError::Io(err))
                            if err.kind() == std::io::ErrorKind::AlreadyExists =>
                        {
                            trace!(
                                "Skipping copy of entrypoint `{}`: already exists",
                                &entry.path().display()
                            );
                        }
                        Err(err) => return Err(err.into()),
                    }
                }

                // Link data directories from the base environment to the ephemeral environment.
                //
                // This is critical for Jupyter Lab, which cannot operate without the files it
                // writes to `<prefix>/share/jupyter`.
                //
                // See https://github.com/jupyterlab/jupyterlab/issues/17716
                for dir in &["etc/jupyter", "share/jupyter"] {
                    let source = interpreter.sys_prefix().join(dir);
                    if !matches!(source.try_exists(), Ok(true)) {
                        continue;
                    }
                    if !source.is_dir() {
                        continue;
                    }
                    let target = ephemeral_env.sys_prefix().join(dir);
                    if let Some(parent) = target.parent() {
                        fs_err::create_dir_all(parent)?;
                    }
                    match create_symlink(&source, &target) {
                        Ok(()) => trace!(
                            "Created link for {} -> {}",
                            target.user_display(),
                            source.user_display()
                        ),
                        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(err) => return Err(err.into()),
                    }
                }
            }

            // Write the `sys.prefix` of the parent environment to the `extends-environment` key of the `pyvenv.cfg`
            // file. This helps out static-analysis tools such as ty (see docs on
            // `CachedEnvironment::set_parent_environment`).
            //
            // Note that we do this even if the parent environment is not a virtual environment.
            // For ephemeral environments created by `uv run --with`, the parent environment's
            // `site-packages` directory is added to `sys.path` even if the parent environment is not
            // a virtual environment and even if `--system-site-packages` was not explicitly selected.
            ephemeral_env.set_parent_environment(base_interpreter.sys_prefix())?;

            // If `--system-site-packages` is enabled, add the system site packages to the ephemeral
            // environment.
            if base_interpreter.is_virtualenv()
                && PyVenvConfiguration::parse(base_interpreter.sys_prefix().join("pyvenv.cfg"))
                    .is_ok_and(|cfg| cfg.include_system_site_packages())
            {
                ephemeral_env.set_system_site_packages()?;
            }
        }
    }

    // Cast to `PythonEnvironment`.
    let ephemeral_env = ephemeral_env.map(PythonEnvironment::from);

    // Determine the Python interpreter to use for the command, if necessary.
    let interpreter = ephemeral_env
        .as_ref()
        .or(requirements_env.as_ref())
        .map_or_else(|| &base_interpreter, |env| env.interpreter());

    // Check if any run command is given.
    // If not, print the available scripts for the current interpreter.
    let Some(command) = command else {
        writeln!(
            printer.stdout(),
            "Provide a command or script to invoke with `uv run <command>` or `uv run <script>.py`.\n"
        )?;

        #[allow(clippy::map_identity)]
        let commands = interpreter
            .scripts()
            .read_dir()
            .ok()
            .into_iter()
            .flatten()
            .map(|entry| match entry {
                Ok(entry) => Ok(entry),
                Err(err) => {
                    // If we can't read the entry, fail.
                    // This could be a symptom of a more serious problem.
                    warn!("Failed to read entry: {}", err);
                    Err(err)
                }
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| {
                entry
                    .file_type()
                    .is_ok_and(|file_type| file_type.is_file() || file_type.is_symlink())
            })
            .map(|entry| entry.path())
            .filter(|path| is_executable(path))
            .map(|path| {
                if cfg!(windows)
                    && path
                        .extension()
                        .is_some_and(|exe| exe == std::env::consts::EXE_EXTENSION)
                {
                    // Remove the extensions.
                    path.with_extension("")
                } else {
                    path
                }
            })
            .map(|path| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            })
            .filter(|command| {
                !command.starts_with("activate") && !command.starts_with("deactivate")
            })
            .sorted()
            .collect_vec();

        if !commands.is_empty() {
            writeln!(
                printer.stdout(),
                "The following commands are available in the environment:\n"
            )?;
            for command in commands {
                writeln!(printer.stdout(), "- {command}")?;
            }
        }
        let help = format!("See `{}` for more information.", "uv run --help".bold());
        writeln!(printer.stdout(), "\n{help}")?;
        return Ok(ExitStatus::Error);
    };

    debug!("Running `{command}`");
    let mut process = command.as_command(interpreter);

    // Construct the `PATH` environment variable.
    let new_path = std::env::join_paths(
        ephemeral_env
            .as_ref()
            .map(PythonEnvironment::scripts)
            .into_iter()
            .chain(
                requirements_env
                    .as_ref()
                    .map(PythonEnvironment::scripts)
                    .into_iter(),
            )
            .chain(std::iter::once(base_interpreter.scripts()))
            .chain(
                // On Windows, non-virtual Python distributions put `python.exe` in the top-level
                // directory, rather than in the `Scripts` subdirectory.
                cfg!(windows)
                    .then(|| base_interpreter.sys_executable().parent())
                    .flatten()
                    .into_iter(),
            )
            .dedup()
            .map(PathBuf::from)
            .chain(
                std::env::var_os(EnvVars::PATH)
                    .as_ref()
                    .iter()
                    .flat_map(std::env::split_paths),
            ),
    )?;
    process.env(EnvVars::PATH, new_path);

    // Increment recursion depth counter.
    process.env(
        EnvVars::UV_RUN_RECURSION_DEPTH,
        (recursion_depth + 1).to_string(),
    );

    // Ensure `VIRTUAL_ENV` is set.
    if interpreter.is_virtualenv() {
        process.env(EnvVars::VIRTUAL_ENV, interpreter.sys_prefix().as_os_str());
    }

    // Spawn and wait for completion
    // Standard input, output, and error streams are all inherited
    // TODO(zanieb): Throw a nicer error message if the command is not found
    let handle = process
        .spawn()
        .with_context(|| format!("Failed to spawn: `{}`", command.display_executable()))?;

    run_to_completion(handle).await
}

/// Returns `true` if we can skip creating an additional ephemeral environment in `uv run`.
fn can_skip_ephemeral(
    spec: &RequirementsSpecification,
    interpreter: &Interpreter,
    site_packages: &SitePackages,
    settings: &ResolverInstallerSettings,
) -> bool {
    if !(settings.reinstall.is_none() && settings.reinstall.is_none()) {
        return false;
    }

    match site_packages.satisfies_spec(
        &spec.requirements,
        &spec.constraints,
        &spec.overrides,
        &interpreter.resolver_marker_environment(),
    ) {
        // If the requirements are already satisfied, we're done.
        Ok(SatisfiesResult::Fresh {
            recursive_requirements,
        }) => {
            debug!(
                "Base environment satisfies requirements: {}",
                recursive_requirements
                    .iter()
                    .map(ToString::to_string)
                    .sorted()
                    .join(" | ")
            );
            true
        }
        Ok(SatisfiesResult::Unsatisfied(requirement)) => {
            debug!(
                "At least one requirement is not satisfied in the base environment: {requirement}"
            );
            false
        }
        Err(err) => {
            debug!("Failed to check requirements against base environment: {err}");
            false
        }
    }
}

#[derive(Debug)]
pub(crate) enum RunCommand {
    /// Execute `python`.
    Python(Vec<OsString>),
    /// Execute a `python` script.
    PythonScript(PathBuf, Vec<OsString>),
    /// Search `sys.path` for the named module and execute its contents as the `__main__` module.
    /// Equivalent to `python -m module`.
    PythonModule(OsString, Vec<OsString>),
    /// Execute a `pythonw` GUI script.
    PythonGuiScript(PathBuf, Vec<OsString>),
    /// Execute a Python package containing a `__main__.py` file.
    /// If an entrypoint with the target name is installed in the environment, it is preferred.
    PythonPackage(OsString, PathBuf, Vec<OsString>),
    /// Execute a Python [zipapp].
    /// [zipapp]: <https://docs.python.org/3/library/zipapp.html>
    PythonZipapp(PathBuf, Vec<OsString>),
    /// Execute a `python` script provided via `stdin`.
    PythonStdin(Vec<u8>, Vec<OsString>),
    /// Execute a `pythonw` script provided via `stdin`.
    PythonGuiStdin(Vec<u8>, Vec<OsString>),
    /// Execute a Python script provided via a remote URL.
    PythonRemote(DisplaySafeUrl, tempfile::NamedTempFile, Vec<OsString>),
    /// Execute an external command.
    External(OsString, Vec<OsString>),
    /// Execute an empty command (in practice, `python` with no arguments).
    Empty,
}

impl RunCommand {
    /// Return the name of the target executable, for display purposes.
    fn display_executable(&self) -> Cow<'_, str> {
        match self {
            Self::Python(_)
            | Self::PythonScript(..)
            | Self::PythonZipapp(..)
            | Self::PythonRemote(..)
            | Self::Empty => Cow::Borrowed("python"),
            // N.B. We can't know if we'll invoke `<target>` or `python <target>` without checking
            // the available scripts in the interpreter — we could improve this message
            Self::PythonPackage(target, ..) => target.to_string_lossy(),
            Self::PythonModule(..) => Cow::Borrowed("python -m"),
            Self::PythonGuiScript(..) => {
                if cfg!(windows) {
                    Cow::Borrowed("pythonw")
                } else {
                    Cow::Borrowed("python")
                }
            }
            Self::PythonStdin(..) => Cow::Borrowed("python -c"),
            Self::PythonGuiStdin(..) => {
                if cfg!(windows) {
                    Cow::Borrowed("pythonw -c")
                } else {
                    Cow::Borrowed("python -c")
                }
            }
            Self::External(executable, _) => executable.to_string_lossy(),
        }
    }

    /// Convert a [`RunCommand`] into a [`Command`].
    fn as_command(&self, interpreter: &Interpreter) -> Command {
        match self {
            Self::Python(args) => {
                let mut process = Command::new(interpreter.sys_executable());
                process.args(args);
                process
            }
            Self::PythonPackage(target, path, args) => {
                let name = PathBuf::from(target).with_extension(std::env::consts::EXE_EXTENSION);
                let entrypoint = interpreter.scripts().join(name);

                // If the target is an installed, executable script — prefer that
                if uv_fs::which::is_executable(&entrypoint) {
                    let mut process = Command::new(entrypoint);
                    process.args(args);
                    process
                // Otherwise, invoke `python <module>`
                } else {
                    let mut process = Command::new(interpreter.sys_executable());
                    process.arg(path);
                    process.args(args);
                    process
                }
            }
            Self::PythonScript(target, args) | Self::PythonZipapp(target, args) => {
                let mut process = Command::new(interpreter.sys_executable());
                process.arg(target);
                process.args(args);
                process
            }
            Self::PythonRemote(.., target, args) => {
                let mut process = Command::new(interpreter.sys_executable());
                process.arg(target.path());
                process.args(args);
                process
            }
            Self::PythonModule(module, args) => {
                let mut process = Command::new(interpreter.sys_executable());
                process.arg("-m");
                process.arg(module);
                process.args(args);
                process
            }
            Self::PythonGuiScript(target, args) => {
                let python_executable = interpreter.sys_executable();

                // Use `pythonw.exe` if it exists, otherwise fall back to `python.exe`.
                // See `install-wheel-rs::get_script_executable`.gd
                let pythonw_executable = python_executable
                    .file_name()
                    .map(|name| {
                        let new_name = name.to_string_lossy().replace("python", "pythonw");
                        python_executable.with_file_name(new_name)
                    })
                    .filter(|path| path.is_file())
                    .unwrap_or_else(|| python_executable.to_path_buf());

                let mut process = Command::new(&pythonw_executable);
                process.arg(target);
                process.args(args);
                process
            }
            Self::PythonStdin(script, args) => {
                let mut process = Command::new(interpreter.sys_executable());
                process.arg("-c");

                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt;
                    process.arg(OsString::from_vec(script.clone()));
                }

                #[cfg(not(unix))]
                {
                    let script = String::from_utf8(script.clone()).expect("script is valid UTF-8");
                    process.arg(script);
                }
                process.args(args);

                process
            }
            Self::PythonGuiStdin(script, args) => {
                let python_executable = interpreter.sys_executable();

                // Use `pythonw.exe` if it exists, otherwise fall back to `python.exe`.
                // See `install-wheel-rs::get_script_executable`.gd
                let pythonw_executable = python_executable
                    .file_name()
                    .map(|name| {
                        let new_name = name.to_string_lossy().replace("python", "pythonw");
                        python_executable.with_file_name(new_name)
                    })
                    .filter(|path| path.is_file())
                    .unwrap_or_else(|| python_executable.to_path_buf());

                let mut process = Command::new(&pythonw_executable);
                process.arg("-c");

                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt;
                    process.arg(OsString::from_vec(script.clone()));
                }

                #[cfg(not(unix))]
                {
                    let script = String::from_utf8(script.clone()).expect("script is valid UTF-8");
                    process.arg(script);
                }
                process.args(args);

                process
            }
            Self::External(executable, args) => {
                let mut process = if cfg!(windows) {
                    WindowsRunnable::from_script_path(interpreter.scripts(), executable).into()
                } else {
                    Command::new(executable)
                };
                process.args(args);
                process
            }
            Self::Empty => Command::new(interpreter.sys_executable()),
        }
    }
}

impl std::fmt::Display for RunCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Python(args) => {
                write!(f, "python")?;
                for arg in args {
                    write!(f, " {}", arg.to_string_lossy())?;
                }
                Ok(())
            }
            Self::PythonPackage(target, _path, args) => {
                write!(f, "{}", target.to_string_lossy())?;
                for arg in args {
                    write!(f, " {}", arg.to_string_lossy())?;
                }
                Ok(())
            }
            Self::PythonScript(target, args) | Self::PythonZipapp(target, args) => {
                write!(f, "python {}", target.display())?;
                for arg in args {
                    write!(f, " {}", arg.to_string_lossy())?;
                }
                Ok(())
            }
            Self::PythonModule(module, args) => {
                write!(f, "python -m")?;
                write!(f, " {}", module.to_string_lossy())?;
                for arg in args {
                    write!(f, " {}", arg.to_string_lossy())?;
                }
                Ok(())
            }
            Self::PythonGuiScript(target, args) => {
                write!(f, "pythonw {}", target.display())?;
                for arg in args {
                    write!(f, " {}", arg.to_string_lossy())?;
                }
                Ok(())
            }
            Self::PythonStdin(..) | Self::PythonRemote(..) => {
                write!(f, "python -c")?;
                Ok(())
            }
            Self::PythonGuiStdin(..) => {
                write!(f, "pythonw -c")?;
                Ok(())
            }
            Self::External(executable, args) => {
                write!(f, "{}", executable.to_string_lossy())?;
                for arg in args {
                    write!(f, " {}", arg.to_string_lossy())?;
                }
                Ok(())
            }
            Self::Empty => {
                write!(f, "python")?;
                Ok(())
            }
        }
    }
}

impl RunCommand {
    /// Determine the [`RunCommand`] for a given set of arguments.
    #[allow(clippy::fn_params_excessive_bools)]
    pub(crate) async fn from_args(
        command: &ExternalCommand,
        network_settings: NetworkSettings,
        module: bool,
        script: bool,
        gui_script: bool,
    ) -> anyhow::Result<Self> {
        let (target, args) = command.split();
        let Some(target) = target else {
            return Ok(Self::Empty);
        };

        if target.eq_ignore_ascii_case("-") {
            let mut buf = Vec::with_capacity(1024);
            std::io::stdin().read_to_end(&mut buf)?;

            return if module {
                Err(anyhow!("Cannot run a Python module from stdin"))
            } else if gui_script {
                Ok(Self::PythonGuiStdin(buf, args.to_vec()))
            } else {
                Ok(Self::PythonStdin(buf, args.to_vec()))
            };
        }

        let target_path = PathBuf::from(target);

        // Determine whether the user provided a remote script.
        if target_path.starts_with("http://") || target_path.starts_with("https://") {
            // Only continue if we are absolutely certain no local file exists.
            //
            // We don't do this check on Windows since the file path would
            // be invalid anyway, and thus couldn't refer to a local file.
            if !cfg!(unix) || matches!(target_path.try_exists(), Ok(false)) {
                let url = DisplaySafeUrl::parse(&target.to_string_lossy())?;

                let file_stem = url
                    .path_segments()
                    .and_then(Iterator::last)
                    .and_then(|segment| segment.strip_suffix(".py"))
                    .unwrap_or("script");
                let file = tempfile::Builder::new()
                    .prefix(file_stem)
                    .suffix(".py")
                    .tempfile()?;

                let client = BaseClientBuilder::new()
                    .retries_from_env()?
                    .connectivity(network_settings.connectivity)
                    .native_tls(network_settings.native_tls)
                    .allow_insecure_host(network_settings.allow_insecure_host.clone())
                    .build();
                let response = client
                    .for_host(&url)
                    .get(Url::from(url.clone()))
                    .send()
                    .await?;

                // Stream the response to the file.
                let mut writer = file.as_file();
                let mut reader = response.bytes_stream();
                while let Some(chunk) = reader.next().await {
                    use std::io::Write;
                    writer.write_all(&chunk?)?;
                }

                return Ok(Self::PythonRemote(url, file, args.to_vec()));
            }
        }

        if module {
            return Ok(Self::PythonModule(target.clone(), args.to_vec()));
        } else if gui_script {
            return Ok(Self::PythonGuiScript(target.clone().into(), args.to_vec()));
        } else if script {
            return Ok(Self::PythonScript(target.clone().into(), args.to_vec()));
        }

        let metadata = target_path.metadata();
        let is_file = metadata.as_ref().is_ok_and(std::fs::Metadata::is_file);
        let is_dir = metadata.as_ref().is_ok_and(std::fs::Metadata::is_dir);

        if target.eq_ignore_ascii_case("python") {
            Ok(Self::Python(args.to_vec()))
        } else if target_path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("py") || ext.eq_ignore_ascii_case("pyc"))
            && is_file
        {
            Ok(Self::PythonScript(target_path, args.to_vec()))
        } else if target_path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pyw"))
            && is_file
        {
            Ok(Self::PythonGuiScript(target_path, args.to_vec()))
        } else if is_dir && target_path.join("__main__.py").is_file() {
            Ok(Self::PythonPackage(
                target.clone(),
                target_path,
                args.to_vec(),
            ))
        } else if is_file && is_python_zipapp(&target_path) {
            Ok(Self::PythonZipapp(target_path, args.to_vec()))
        } else {
            Ok(Self::External(
                target.clone(),
                args.iter().map(std::clone::Clone::clone).collect(),
            ))
        }
    }
}

/// Returns `true` if the target is a ZIP archive containing a `__main__.py` file.
fn is_python_zipapp(target: &Path) -> bool {
    if let Ok(file) = fs_err::File::open(target) {
        if let Ok(mut archive) = zip::ZipArchive::new(file) {
            return archive.by_name("__main__.py").is_ok_and(|f| f.is_file());
        }
    }
    false
}

/// Read and parse recursion depth from the environment.
///
/// Returns Ok(0) if `EnvVars::UV_RUN_RECURSION_DEPTH` is not set.
///
/// Returns an error if `EnvVars::UV_RUN_RECURSION_DEPTH` is set to a value
/// that cannot ber parsed as an integer.
fn read_recursion_depth_from_environment_variable() -> anyhow::Result<u32> {
    let envvar = match std::env::var(EnvVars::UV_RUN_RECURSION_DEPTH) {
        Ok(val) => val,
        Err(VarError::NotPresent) => return Ok(0),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("invalid value for {}", EnvVars::UV_RUN_RECURSION_DEPTH));
        }
    };

    envvar
        .parse::<u32>()
        .with_context(|| format!("invalid value for {}", EnvVars::UV_RUN_RECURSION_DEPTH))
}

#[derive(Error, Debug)]
enum CopyEntrypointError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[cfg(windows)]
    #[error(transparent)]
    Trampoline(#[from] uv_trampoline_builder::Error),
}

/// Create a copy of the entrypoint at `source` at `target`, if it has a Python shebang, replacing
/// the previous Python executable with a new one.
///
/// This is a no-op if the target already exists.
///
/// Note on Windows, the entrypoints do not use shebangs and require a rewrite of the trampoline.
#[cfg(unix)]
fn copy_entrypoint(
    source: &Path,
    target: &Path,
    previous_executable: &Path,
    python_executable: &Path,
) -> Result<(), CopyEntrypointError> {
    use std::io::{Seek, Write};
    use std::os::unix::fs::PermissionsExt;

    use fs_err::os::unix::fs::OpenOptionsExt;

    let mut file = fs_err::File::open(source)?;
    let mut buffer = [0u8; 2];
    if file.read_exact(&mut buffer).is_err() {
        // File is too small to have a shebang
        trace!(
            "Skipping copy of entrypoint `{}`: file is too small to contain a shebang",
            source.user_display()
        );
        return Ok(());
    }

    // Check if it starts with `#!` to avoid reading binary files and such into memory
    if &buffer != b"#!" {
        trace!(
            "Skipping copy of entrypoint `{}`: does not start with #!",
            source.user_display()
        );
        return Ok(());
    }

    let mut contents = String::new();
    file.seek(std::io::SeekFrom::Start(0))?;
    match file.read_to_string(&mut contents) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
            // If the file is not valid UTF-8, we skip it in case it was a binary file with `#!` at
            // the start (which seems pretty niche, but being defensive here seems safe)
            trace!(
                "Skipping copy of entrypoint `{}`: is not valid UTF-8",
                source.user_display()
            );
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    }

    let Some(contents) = contents
        // Check for a relative path or relocatable shebang
        .strip_prefix(
            r#"#!/bin/sh
'''exec' "$(dirname -- "$(realpath -- "$0")")"/'python' "$0" "$@"
' '''
"#,
        )
        // Or, an absolute path shebang
        .or_else(|| contents.strip_prefix(&format!("#!{}\n", previous_executable.display())))
        // If the previous executable ends with `python3`, check for a shebang with `python` too
        .or_else(|| {
            previous_executable
                .to_str()
                .and_then(|path| path.strip_suffix("3"))
                .and_then(|path| contents.strip_prefix(&format!("#!{path}\n")))
        })
    else {
        // If it's not a Python shebang, we'll skip it
        trace!(
            "Skipping copy of entrypoint `{}`: does not start with expected shebang",
            source.user_display()
        );
        return Ok(());
    };

    let contents = format!("#!{}\n{}", python_executable.display(), contents);
    let mode = fs_err::metadata(source)?.permissions().mode();
    let mut file = fs_err::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(mode)
        .open(target)?;
    file.write_all(contents.as_bytes())?;

    trace!("Updated entrypoint at {}", target.user_display());

    Ok(())
}

/// Create a copy of the entrypoint at `source` at `target`, if it's a Python script launcher,
/// replacing the target Python executable with a new one.
#[cfg(windows)]
fn copy_entrypoint(
    source: &Path,
    target: &Path,
    _previous_executable: &Path,
    python_executable: &Path,
) -> Result<(), CopyEntrypointError> {
    use uv_trampoline_builder::Launcher;

    let Some(launcher) = Launcher::try_from_path(source)? else {
        return Ok(());
    };

    let launcher = launcher.with_python_path(python_executable.to_path_buf());
    let mut file = fs_err::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)?;
    launcher.write_to_file(&mut file)?;

    trace!("Updated entrypoint at {}", target.user_display());

    Ok(())
}
