use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use itertools::Itertools;
use owo_colors::OwoColorize;
use tracing::{debug, trace, warn};

use uv_cache::{Cache, CacheBucket};
use uv_cache_key::cache_digest;
use uv_client::{BaseClientBuilder, FlatIndexClient, RegistryClientBuilder};
use uv_configuration::{
    Concurrency, Constraints, DependencyGroupsWithDefaults, DryRun, ExtrasSpecification, Preview,
    PreviewFeatures, Reinstall, Upgrade,
};
use uv_dispatch::{BuildDispatch, SharedState};
use uv_distribution::{DistributionDatabase, LoweredRequirement};
use uv_distribution_types::{
    Index, Requirement, RequiresPython, Resolution, UnresolvedRequirement,
    UnresolvedRequirementSpecification,
};
use uv_fs::{CWD, LockedFile, Simplified};
use uv_git::ResolvedRepositoryReference;
use uv_installer::{SatisfiesResult, SitePackages};
use uv_normalize::{DEV_DEPENDENCIES, DefaultGroups, ExtraName, GroupName, PackageName};
use uv_pep440::{TildeVersionSpecifier, Version, VersionSpecifiers};
use uv_pep508::MarkerTreeContents;
use uv_pypi_types::{ConflictPackage, ConflictSet, Conflicts};
use uv_python::{
    EnvironmentPreference, Interpreter, InvalidEnvironmentKind, PythonDownloads, PythonEnvironment,
    PythonInstallation, PythonPreference, PythonRequest, PythonSource, PythonVariant,
    PythonVersionFile, VersionFileDiscoveryOptions, VersionRequest, satisfies_python_preference,
};
use uv_requirements::upgrade::{LockedRequirements, read_lock_requirements};
use uv_requirements::{NamedRequirementsResolver, RequirementsSpecification};
use uv_resolver::{
    FlatIndex, Lock, OptionsBuilder, Preference, PythonRequirement, ResolverEnvironment,
    ResolverOutput,
};
use uv_scripts::Pep723ItemRef;
use uv_settings::PythonInstallMirrors;
use uv_static::EnvVars;
use uv_types::{BuildIsolation, EmptyInstalledPackages, HashStrategy};
use uv_virtualenv::remove_virtualenv;
use uv_warnings::{warn_user, warn_user_once};
use uv_workspace::dependency_groups::DependencyGroupError;
use uv_workspace::pyproject::ExtraBuildDependencies;
use uv_workspace::pyproject::PyProjectToml;
use uv_workspace::{RequiresPythonSources, Workspace, WorkspaceCache};

use crate::commands::pip::loggers::{InstallLogger, ResolveLogger};
use crate::commands::pip::operations::{Changelog, Modifications};
use crate::commands::reporters::{PythonDownloadReporter, ResolverReporter};
use crate::commands::{capitalize, conjunction, pip};
use crate::printer::Printer;
use crate::settings::{
    InstallerSettingsRef, NetworkSettings, ResolverInstallerSettings, ResolverSettings,
};

pub(crate) mod add;
pub(crate) mod environment;
pub(crate) mod export;
pub(crate) mod init;
mod install_target;
pub(crate) mod lock;
mod lock_target;
pub(crate) mod remove;
pub(crate) mod run;
pub(crate) mod sync;
pub(crate) mod tree;
pub(crate) mod version;

#[derive(thiserror::Error, Debug)]
pub(crate) enum ProjectError {
    #[error(
        "The lockfile at `uv.lock` needs to be updated, but `--locked` was provided. To update the lockfile, run `uv lock`."
    )]
    LockMismatch(Option<Box<Lock>>, Box<Lock>),

    #[error(
        "Unable to find lockfile at `uv.lock`. To create a lockfile, run `uv lock` or `uv sync`."
    )]
    MissingLockfile,

    #[error(
        "The lockfile at `uv.lock` uses an unsupported schema version (v{1}, but only v{0} is supported). Downgrade to a compatible uv version, or remove the `uv.lock` prior to running `uv lock` or `uv sync`."
    )]
    UnsupportedLockVersion(u32, u32),

    #[error(
        "Failed to parse `uv.lock`, which uses an unsupported schema version (v{1}, but only v{0} is supported). Downgrade to a compatible uv version, or remove the `uv.lock` prior to running `uv lock` or `uv sync`."
    )]
    UnparsableLockVersion(u32, u32, #[source] toml::de::Error),

    #[error("Failed to serialize `uv.lock`")]
    LockSerialization(#[from] toml_edit::ser::Error),

    #[error(
        "The current Python version ({0}) is not compatible with the locked Python requirement: `{1}`"
    )]
    LockedPythonIncompatibility(Version, RequiresPython),

    #[error(
        "The current Python platform is not compatible with the lockfile's supported environments: {0}"
    )]
    LockedPlatformIncompatibility(String),

    #[error(transparent)]
    Conflict(#[from] ConflictError),

    #[error(
        "The requested interpreter resolved to Python {_0}, which is incompatible with the project's Python requirement: `{_1}`{}",
        format_optional_requires_python_sources(_2, *_3)
    )]
    RequestedPythonProjectIncompatibility(Version, RequiresPython, RequiresPythonSources, bool),

    #[error(
        "The Python request from `{_0}` resolved to Python {_1}, which is incompatible with the project's Python requirement: `{_2}`{}\nUse `uv python pin` to update the `.python-version` file to a compatible version",
        format_optional_requires_python_sources(_3, *_4)
    )]
    DotPythonVersionProjectIncompatibility(
        String,
        Version,
        RequiresPython,
        RequiresPythonSources,
        bool,
    ),

    #[error(
        "The resolved Python interpreter (Python {_0}) is incompatible with the project's Python requirement: `{_1}`{}",
        format_optional_requires_python_sources(_2, *_3)
    )]
    RequiresPythonProjectIncompatibility(Version, RequiresPython, RequiresPythonSources, bool),

    #[error(
        "The requested interpreter resolved to Python {0}, which is incompatible with the script's Python requirement: `{1}`"
    )]
    RequestedPythonScriptIncompatibility(Version, RequiresPython),

    #[error(
        "The Python request from `{0}` resolved to Python {1}, which is incompatible with the script's Python requirement: `{2}`"
    )]
    DotPythonVersionScriptIncompatibility(String, Version, RequiresPython),

    #[error(
        "The resolved Python interpreter (Python {0}) is incompatible with the script's Python requirement: `{1}`"
    )]
    RequiresPythonScriptIncompatibility(Version, RequiresPython),

    #[error("Group `{0}` is not defined in the project's `dependency-groups` table")]
    MissingGroupProject(GroupName),

    #[error("Group `{0}` is not defined in any project's `dependency-groups` table")]
    MissingGroupWorkspace(GroupName),

    #[error("PEP 723 scripts do not support dependency groups, but group `{0}` was specified")]
    MissingGroupScript(GroupName),

    #[error(
        "Default group `{0}` (from `tool.uv.default-groups`) is not defined in the project's `dependency-groups` table"
    )]
    MissingDefaultGroup(GroupName),

    #[error("Extra `{0}` is not defined in the project's `optional-dependencies` table")]
    MissingExtraProject(ExtraName),

    #[error("Extra `{0}` is not defined in any project's `optional-dependencies` table")]
    MissingExtraWorkspace(ExtraName),

    #[error("PEP 723 scripts do not support optional dependencies, but extra `{0}` was specified")]
    MissingExtraScript(ExtraName),

    #[error("Supported environments must be disjoint, but the following markers overlap: `{0}` and `{1}`.\n\n{hint}{colon} replace `{1}` with `{2}`.", hint = "hint".bold().cyan(), colon = ":".bold())]
    OverlappingMarkers(String, String, String),

    #[error("Environment markers `{0}` don't overlap with Python requirement `{1}`")]
    DisjointEnvironment(MarkerTreeContents, VersionSpecifiers),

    #[error(
        "Found conflicting Python requirements:\n{}",
        format_requires_python_sources(_0)
    )]
    DisjointRequiresPython(BTreeMap<(PackageName, Option<GroupName>), VersionSpecifiers>),

    #[error("Environment marker is empty")]
    EmptyEnvironment,

    #[error("Project virtual environment directory `{0}` cannot be used because {1}")]
    InvalidProjectEnvironmentDir(PathBuf, String),

    #[error("Failed to parse `uv.lock`")]
    UvLockParse(#[source] toml::de::Error),

    #[error("Failed to parse `pyproject.toml`")]
    PyprojectTomlParse(#[source] toml::de::Error),

    #[error("Failed to update `pyproject.toml`")]
    PyprojectTomlUpdate,

    #[error("Failed to parse PEP 723 script metadata")]
    Pep723ScriptTomlParse(#[source] toml::de::Error),

    #[error("Failed to find `site-packages` directory for environment")]
    NoSitePackages,

    #[error("Attempted to drop a temporary virtual environment while still in-use")]
    DroppedEnvironment,

    #[error(transparent)]
    DependencyGroup(#[from] DependencyGroupError),

    #[error(transparent)]
    Python(#[from] uv_python::Error),

    #[error(transparent)]
    Virtualenv(#[from] uv_virtualenv::Error),

    #[error(transparent)]
    HashStrategy(#[from] uv_types::HashStrategyError),

    #[error(transparent)]
    Tags(#[from] uv_platform_tags::TagsError),

    #[error(transparent)]
    FlatIndex(#[from] uv_client::FlatIndexError),

    #[error(transparent)]
    Lock(#[from] uv_resolver::LockError),

    #[error(transparent)]
    Operation(#[from] pip::operations::Error),

    #[error(transparent)]
    Interpreter(#[from] uv_python::InterpreterError),

    #[error(transparent)]
    Tool(#[from] uv_tool::Error),

    #[error(transparent)]
    Name(#[from] uv_normalize::InvalidNameError),

    #[error(transparent)]
    Requirements(#[from] uv_requirements::Error),

    #[error(transparent)]
    Metadata(#[from] uv_distribution::MetadataError),

    #[error(transparent)]
    Lowering(#[from] uv_distribution::LoweringError),

    #[error(transparent)]
    PyprojectMut(#[from] uv_workspace::pyproject_mut::Error),

    #[error(transparent)]
    Fmt(#[from] std::fmt::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
}

#[derive(Debug)]
pub(crate) struct ConflictError {
    /// The set from which the conflict was derived.
    pub(crate) set: ConflictSet,
    /// The items from the set that were enabled, and thus create the conflict.
    pub(crate) conflicts: Vec<ConflictPackage>,
    /// Enabled dependency groups with defaults applied.
    pub(crate) groups: DependencyGroupsWithDefaults,
}

impl std::fmt::Display for ConflictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Format the set itself.
        let set = self
            .set
            .iter()
            .map(|item| match item.conflict() {
                ConflictPackage::Extra(extra) => format!("`{}[{}]`", item.package(), extra),
                ConflictPackage::Group(group) => format!("`{}:{}`", item.package(), group),
            })
            .join(", ");

        // If all the conflicts are of the same kind, show a more succinct error.
        if self
            .conflicts
            .iter()
            .all(|conflict| matches!(conflict, ConflictPackage::Extra(..)))
        {
            write!(
                f,
                "Extras {} are incompatible with the declared conflicts: {{{set}}}",
                conjunction(
                    self.conflicts
                        .iter()
                        .map(|conflict| match conflict {
                            ConflictPackage::Extra(extra) => format!("`{extra}`"),
                            ConflictPackage::Group(..) => unreachable!(),
                        })
                        .collect()
                )
            )
        } else if self
            .conflicts
            .iter()
            .all(|conflict| matches!(conflict, ConflictPackage::Group(..)))
        {
            let conflict_source = if self.set.is_inferred_conflict() {
                "transitively inferred"
            } else {
                "declared"
            };
            write!(
                f,
                "Groups {} are incompatible with the {conflict_source} conflicts: {{{set}}}",
                conjunction(
                    self.conflicts
                        .iter()
                        .map(|conflict| match conflict {
                            ConflictPackage::Group(group)
                                if self.groups.contains_because_default(group) =>
                                format!("`{group}` (enabled by default)"),
                            ConflictPackage::Group(group) => format!("`{group}`"),
                            ConflictPackage::Extra(..) => unreachable!(),
                        })
                        .collect()
                )
            )
        } else {
            write!(
                f,
                "{} are incompatible with the declared conflicts: {{{set}}}",
                conjunction(
                    self.conflicts
                        .iter()
                        .enumerate()
                        .map(|(i, conflict)| {
                            let conflict = match conflict {
                                ConflictPackage::Extra(extra) => format!("extra `{extra}`"),
                                ConflictPackage::Group(group)
                                    if self.groups.contains_because_default(group) =>
                                {
                                    format!("group `{group}` (enabled by default)")
                                }
                                ConflictPackage::Group(group) => format!("group `{group}`"),
                            };
                            if i == 0 {
                                capitalize(&conflict)
                            } else {
                                conflict
                            }
                        })
                        .collect()
                )
            )
        }
    }
}

impl std::error::Error for ConflictError {}

/// A [`SharedState`] instance to use for universal resolution.
#[derive(Default, Clone)]
pub(crate) struct UniversalState(SharedState);

impl std::ops::Deref for UniversalState {
    type Target = SharedState;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl UniversalState {
    /// Fork the [`UniversalState`] to create a [`PlatformState`].
    pub(crate) fn fork(&self) -> PlatformState {
        PlatformState(self.0.fork())
    }
}

/// A [`SharedState`] instance to use for platform-specific resolution.
#[derive(Default, Clone)]
pub(crate) struct PlatformState(SharedState);

impl std::ops::Deref for PlatformState {
    type Target = SharedState;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl PlatformState {
    /// Fork the [`PlatformState`] to create a [`UniversalState`].
    pub(crate) fn fork(&self) -> UniversalState {
        UniversalState(self.0.fork())
    }

    /// Create a [`SharedState`] from the [`PlatformState`].
    pub(crate) fn into_inner(self) -> SharedState {
        self.0
    }
}

/// Compute the `Requires-Python` bound for the [`Workspace`].
///
/// For a [`Workspace`] with multiple packages, the `Requires-Python` bound is the union of the
/// `Requires-Python` bounds of all the packages.
#[allow(clippy::result_large_err)]
pub(crate) fn find_requires_python(
    workspace: &Workspace,
    groups: &DependencyGroupsWithDefaults,
) -> Result<Option<RequiresPython>, ProjectError> {
    let requires_python = workspace.requires_python(groups)?;
    // If there are no `Requires-Python` specifiers in the workspace, return `None`.
    if requires_python.is_empty() {
        return Ok(None);
    }
    for ((package, group), specifiers) in &requires_python {
        if let [spec] = &specifiers[..] {
            if let Some(spec) = TildeVersionSpecifier::from_specifier_ref(spec) {
                if spec.has_patch() {
                    continue;
                }
                let (lower, upper) = spec.bounding_specifiers();
                let spec_0 = spec.with_patch_version(0);
                let (lower_0, upper_0) = spec_0.bounding_specifiers();
                warn_user_once!(
                    "The `requires-python` specifier (`{spec}`) in `{package}{group}` \
                    uses the tilde specifier (`~=`) without a patch version. This will be \
                    interpreted as `{lower}, {upper}`. Did you mean `{spec_0}` to constrain the \
                    version as `{lower_0}, {upper_0}`? We recommend only using \
                    the tilde specifier with a patch version to avoid ambiguity.",
                    group = if let Some(group) = group {
                        format!(":{group}")
                    } else {
                        String::new()
                    },
                );
            }
        }
    }
    match RequiresPython::intersection(requires_python.iter().map(|(.., specifiers)| specifiers)) {
        Some(requires_python) => Ok(Some(requires_python)),
        None => Err(ProjectError::DisjointRequiresPython(requires_python)),
    }
}

/// Returns an error if the [`Interpreter`] does not satisfy the [`Workspace`] `requires-python`.
///
/// If no [`Workspace`] is provided, the `requires-python` will be validated against the originating
/// source (e.g., a `.python-version` file or a `--python` command-line argument).
#[allow(clippy::result_large_err)]
pub(crate) fn validate_project_requires_python(
    interpreter: &Interpreter,
    workspace: Option<&Workspace>,
    groups: &DependencyGroupsWithDefaults,
    requires_python: &RequiresPython,
    source: &PythonRequestSource,
) -> Result<(), ProjectError> {
    if requires_python.contains(interpreter.python_version()) {
        return Ok(());
    }

    // Find all the individual requires_python constraints that conflict
    let conflicting_requires = workspace
        .and_then(|workspace| workspace.requires_python(groups).ok())
        .into_iter()
        .flatten()
        .filter(|(.., requires)| !requires.contains(interpreter.python_version()))
        .collect::<RequiresPythonSources>();
    let workspace_non_trivial = workspace
        .map(|workspace| workspace.packages().len() > 1)
        .unwrap_or(false);

    match source {
        PythonRequestSource::UserRequest => {
            Err(ProjectError::RequestedPythonProjectIncompatibility(
                interpreter.python_version().clone(),
                requires_python.clone(),
                conflicting_requires,
                workspace_non_trivial,
            ))
        }
        PythonRequestSource::DotPythonVersion(file) => {
            Err(ProjectError::DotPythonVersionProjectIncompatibility(
                file.path().user_display().to_string(),
                interpreter.python_version().clone(),
                requires_python.clone(),
                conflicting_requires,
                workspace_non_trivial,
            ))
        }
        PythonRequestSource::RequiresPython => {
            Err(ProjectError::RequiresPythonProjectIncompatibility(
                interpreter.python_version().clone(),
                requires_python.clone(),
                conflicting_requires,
                workspace_non_trivial,
            ))
        }
    }
}

/// Returns an error if the [`Interpreter`] does not satisfy script or workspace `requires-python`.
#[allow(clippy::result_large_err)]
fn validate_script_requires_python(
    interpreter: &Interpreter,
    requires_python: &RequiresPython,
    source: &PythonRequestSource,
) -> Result<(), ProjectError> {
    if requires_python.contains(interpreter.python_version()) {
        return Ok(());
    }
    match source {
        PythonRequestSource::UserRequest => {
            Err(ProjectError::RequestedPythonScriptIncompatibility(
                interpreter.python_version().clone(),
                requires_python.clone(),
            ))
        }
        PythonRequestSource::DotPythonVersion(file) => {
            Err(ProjectError::DotPythonVersionScriptIncompatibility(
                file.file_name().to_string(),
                interpreter.python_version().clone(),
                requires_python.clone(),
            ))
        }
        PythonRequestSource::RequiresPython => {
            Err(ProjectError::RequiresPythonScriptIncompatibility(
                interpreter.python_version().clone(),
                requires_python.clone(),
            ))
        }
    }
}

/// An interpreter suitable for a PEP 723 script.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum ScriptInterpreter {
    /// An interpreter to use to create a new script environment.
    Interpreter(Interpreter),
    /// An interpreter from an existing script environment.
    Environment(PythonEnvironment),
}

impl ScriptInterpreter {
    /// Return the expected virtual environment path for the [`Pep723Script`].
    ///
    /// If `--active` is set, the active virtual environment will be preferred.
    ///
    /// See: [`Workspace::venv`].
    pub(crate) fn root(script: Pep723ItemRef<'_>, active: Option<bool>, cache: &Cache) -> PathBuf {
        /// Resolve the `VIRTUAL_ENV` variable, if any.
        fn from_virtual_env_variable() -> Option<PathBuf> {
            let value = std::env::var_os(EnvVars::VIRTUAL_ENV)?;

            if value.is_empty() {
                return None;
            }

            let path = PathBuf::from(value);
            if path.is_absolute() {
                return Some(path);
            }

            // Resolve the path relative to current directory.
            Some(CWD.join(path))
        }

        // Determine the stable path to the script environment in the cache.
        let cache_env = {
            let entry = match script {
                // For local scripts, use a hash of the path to the script.
                Pep723ItemRef::Script(script) => {
                    let digest = cache_digest(&script.path);
                    if let Some(file_name) = script
                        .path
                        .file_stem()
                        .and_then(|name| name.to_str())
                        .and_then(cache_name)
                    {
                        format!("{file_name}-{digest}")
                    } else {
                        digest
                    }
                }
                // For remote scripts, use a hash of the URL.
                Pep723ItemRef::Remote(.., url) => cache_digest(url),
                // Otherwise, use a hash of the metadata.
                Pep723ItemRef::Stdin(metadata) => cache_digest(&metadata.raw),
            };

            cache
                .shard(CacheBucket::Environments, entry)
                .into_path_buf()
        };

        // If `--active` is set, prefer the active virtual environment.
        if let Some(from_virtual_env) = from_virtual_env_variable() {
            if !uv_fs::is_same_file_allow_missing(&from_virtual_env, &cache_env).unwrap_or(false) {
                match active {
                    Some(true) => {
                        debug!(
                            "Using active virtual environment `{}` instead of script environment `{}`",
                            from_virtual_env.user_display(),
                            cache_env.user_display()
                        );
                        return from_virtual_env;
                    }
                    Some(false) => {}
                    None => {
                        warn_user_once!(
                            "`VIRTUAL_ENV={}` does not match the script environment path `{}` and will be ignored; use `--active` to target the active environment instead",
                            from_virtual_env.user_display(),
                            cache_env.user_display()
                        );
                    }
                }
            }
        } else {
            if active.unwrap_or_default() {
                debug!(
                    "Use of the active virtual environment was requested, but `VIRTUAL_ENV` is not set"
                );
            }
        }

        // Otherwise, use the cache root.
        cache_env
    }

    /// Discover the interpreter to use for the current [`Pep723Item`].
    pub(crate) async fn discover(
        script: Pep723ItemRef<'_>,
        python_request: Option<PythonRequest>,
        network_settings: &NetworkSettings,
        python_preference: PythonPreference,
        python_downloads: PythonDownloads,
        install_mirrors: &PythonInstallMirrors,
        keep_incompatible: bool,
        no_config: bool,
        active: Option<bool>,
        cache: &Cache,
        printer: Printer,
        preview: Preview,
    ) -> Result<Self, ProjectError> {
        // For now, we assume that scripts are never evaluated in the context of a workspace.
        let workspace = None;

        let ScriptPython {
            source,
            python_request,
            requires_python,
        } = ScriptPython::from_request(python_request, workspace, script, no_config).await?;

        let root = Self::root(script, active, cache);
        match PythonEnvironment::from_root(&root, cache) {
            Ok(venv) => {
                match environment_is_usable(
                    &venv,
                    EnvironmentKind::Script,
                    python_request.as_ref(),
                    python_preference,
                    requires_python
                        .as_ref()
                        .map(|(requires_python, _)| requires_python),
                    cache,
                ) {
                    Ok(()) => return Ok(Self::Environment(venv)),
                    Err(err) if keep_incompatible => {
                        warn_user!(
                            "Using incompatible environment (`{}`) due to `--no-sync` ({err})",
                            root.user_display().cyan(),
                        );
                        return Ok(Self::Environment(venv));
                    }
                    Err(err) => {
                        debug!("{err}");
                    }
                }
            }
            Err(uv_python::Error::MissingEnvironment(_)) => {}
            Err(err) => warn!("Ignoring existing script environment: {err}"),
        }

        let client_builder = BaseClientBuilder::new()
            .retries_from_env()?
            .connectivity(network_settings.connectivity)
            .native_tls(network_settings.native_tls)
            .allow_insecure_host(network_settings.allow_insecure_host.clone());

        let reporter = PythonDownloadReporter::single(printer);

        let interpreter = PythonInstallation::find_or_download(
            python_request.as_ref(),
            EnvironmentPreference::Any,
            python_preference,
            python_downloads,
            &client_builder,
            cache,
            Some(&reporter),
            install_mirrors.python_install_mirror.as_deref(),
            install_mirrors.pypy_install_mirror.as_deref(),
            install_mirrors.python_downloads_json_url.as_deref(),
            preview,
        )
        .await?
        .into_interpreter();

        if let Err(err) = match requires_python {
            Some((requires_python, RequiresPythonSource::Project)) => {
                validate_project_requires_python(
                    &interpreter,
                    workspace,
                    &DependencyGroupsWithDefaults::none(),
                    &requires_python,
                    &source,
                )
            }
            Some((requires_python, RequiresPythonSource::Script)) => {
                validate_script_requires_python(&interpreter, &requires_python, &source)
            }
            None => Ok(()),
        } {
            warn_user!("{err}");
        }

        Ok(Self::Interpreter(interpreter))
    }

    /// Consume the [`PythonInstallation`] and return the [`Interpreter`].
    pub(crate) fn into_interpreter(self) -> Interpreter {
        match self {
            ScriptInterpreter::Interpreter(interpreter) => interpreter,
            ScriptInterpreter::Environment(venv) => venv.into_interpreter(),
        }
    }

    /// Grab a file lock for the script to prevent concurrent writes across processes.
    pub(crate) async fn lock(script: Pep723ItemRef<'_>) -> Result<LockedFile, std::io::Error> {
        match script {
            Pep723ItemRef::Script(script) => {
                LockedFile::acquire(
                    std::env::temp_dir().join(format!("uv-{}.lock", cache_digest(&script.path))),
                    script.path.simplified_display(),
                )
                .await
            }
            Pep723ItemRef::Remote(.., url) => {
                LockedFile::acquire(
                    std::env::temp_dir().join(format!("uv-{}.lock", cache_digest(url))),
                    url.to_string(),
                )
                .await
            }
            Pep723ItemRef::Stdin(metadata) => {
                LockedFile::acquire(
                    std::env::temp_dir().join(format!("uv-{}.lock", cache_digest(&metadata.raw))),
                    "stdin".to_string(),
                )
                .await
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum EnvironmentKind {
    Script,
    Project,
}

impl std::fmt::Display for EnvironmentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Script => write!(f, "script"),
            Self::Project => write!(f, "project"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EnvironmentIncompatibilityError {
    #[error("The {0} environment's Python version does not satisfy the request: `{1}`")]
    PythonRequest(EnvironmentKind, PythonRequest),

    #[error("The {0} environment's Python version does not meet the Python requirement: `{1}`")]
    RequiresPython(EnvironmentKind, RequiresPython),

    #[error(
        "The interpreter in the {0} environment has a different version ({1}) than it was created with ({2})"
    )]
    PyenvVersionConflict(EnvironmentKind, Version, Version),

    #[error("The {0} environment's Python interpreter does not meet the Python preference: `{1}`")]
    PythonPreference(EnvironmentKind, PythonPreference),
}

/// Whether an environment is usable for a project or script, i.e., if it matches the requirements.
fn environment_is_usable(
    environment: &PythonEnvironment,
    kind: EnvironmentKind,
    python_request: Option<&PythonRequest>,
    python_preference: PythonPreference,
    requires_python: Option<&RequiresPython>,
    cache: &Cache,
) -> Result<(), EnvironmentIncompatibilityError> {
    if let Some((cfg_version, int_version)) = environment.get_pyvenv_version_conflict() {
        return Err(EnvironmentIncompatibilityError::PyenvVersionConflict(
            kind,
            int_version,
            cfg_version,
        ));
    }

    if let Some(request) = python_request {
        if request.satisfied(environment.interpreter(), cache) {
            debug!("The {kind} environment's Python version satisfies the request: `{request}`");
        } else {
            return Err(EnvironmentIncompatibilityError::PythonRequest(
                kind,
                request.clone(),
            ));
        }
    }

    if let Some(requires_python) = requires_python {
        if requires_python.contains(environment.interpreter().python_version()) {
            trace!(
                "The {kind} environment's Python version meets the Python requirement: `{requires_python}`"
            );
        } else {
            return Err(EnvironmentIncompatibilityError::RequiresPython(
                kind,
                requires_python.clone(),
            ));
        }
    }

    if satisfies_python_preference(
        PythonSource::DiscoveredEnvironment,
        environment.interpreter(),
        python_preference,
    ) {
        trace!(
            "The virtual environment's Python interpreter meets the Python preference: `{}`",
            python_preference
        );
    } else {
        return Err(EnvironmentIncompatibilityError::PythonPreference(
            kind,
            python_preference,
        ));
    }

    Ok(())
}

/// An interpreter suitable for the project.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum ProjectInterpreter {
    /// An interpreter from outside the project, to create a new project virtual environment.
    Interpreter(Interpreter),
    /// An interpreter from an existing project virtual environment.
    Environment(PythonEnvironment),
}

impl ProjectInterpreter {
    /// Discover the interpreter to use in the current [`Workspace`].
    pub(crate) async fn discover(
        workspace: &Workspace,
        project_dir: &Path,
        groups: &DependencyGroupsWithDefaults,
        python_request: Option<PythonRequest>,
        network_settings: &NetworkSettings,
        python_preference: PythonPreference,
        python_downloads: PythonDownloads,
        install_mirrors: &PythonInstallMirrors,
        keep_incompatible: bool,
        no_config: bool,
        active: Option<bool>,
        cache: &Cache,
        printer: Printer,
        preview: Preview,
    ) -> Result<Self, ProjectError> {
        // Resolve the Python request and requirement for the workspace.
        let WorkspacePython {
            source,
            python_request,
            requires_python,
        } = WorkspacePython::from_request(
            python_request,
            Some(workspace),
            groups,
            project_dir,
            no_config,
        )
        .await?;

        // Read from the virtual environment first.
        let root = workspace.venv(active);
        match PythonEnvironment::from_root(&root, cache) {
            Ok(venv) => {
                match environment_is_usable(
                    &venv,
                    EnvironmentKind::Project,
                    python_request.as_ref(),
                    python_preference,
                    requires_python.as_ref(),
                    cache,
                ) {
                    Ok(()) => return Ok(Self::Environment(venv)),
                    Err(err) if keep_incompatible => {
                        warn_user!(
                            "Using incompatible environment (`{}`) due to `--no-sync` ({err})",
                            root.user_display().cyan(),
                        );
                        return Ok(Self::Environment(venv));
                    }
                    Err(err) => {
                        debug!("{err}");
                    }
                }
            }
            Err(uv_python::Error::MissingEnvironment(_)) => {}
            Err(uv_python::Error::InvalidEnvironment(inner)) => {
                // If there's an invalid environment with existing content, we error instead of
                // deleting it later on
                match inner.kind {
                    InvalidEnvironmentKind::NotDirectory => {
                        return Err(ProjectError::InvalidProjectEnvironmentDir(
                            root,
                            inner.kind.to_string(),
                        ));
                    }
                    InvalidEnvironmentKind::MissingExecutable(_) => {
                        // If it's not an empty directory
                        if fs_err::read_dir(&root).is_ok_and(|mut dir| dir.next().is_some()) {
                            // ... and there's no `pyvenv.cfg`
                            if !root.join("pyvenv.cfg").try_exists().unwrap_or_default() {
                                // ... then it's not a valid Python environment
                                return Err(ProjectError::InvalidProjectEnvironmentDir(
                                    root,
                                    "it is not a valid Python environment (no Python executable was found)"
                                        .to_string(),
                                ));
                            }
                        }
                        // Otherwise, we'll delete it
                    }
                    // If the environment is an empty directory, it's fine to use
                    InvalidEnvironmentKind::Empty => {}
                }
            }
            Err(uv_python::Error::Query(uv_python::InterpreterError::NotFound(_))) => {}
            Err(uv_python::Error::Query(uv_python::InterpreterError::BrokenSymlink(
                broken_symlink,
            ))) => {
                let target_path = fs_err::read_link(&broken_symlink.path)?;
                warn_user!(
                    "Ignoring existing virtual environment linked to non-existent Python interpreter: {} -> {}",
                    broken_symlink.path.user_display().cyan(),
                    target_path.user_display().cyan(),
                );
            }
            Err(err) => return Err(err.into()),
        }

        let client_builder = BaseClientBuilder::default()
            .retries_from_env()?
            .connectivity(network_settings.connectivity)
            .native_tls(network_settings.native_tls)
            .allow_insecure_host(network_settings.allow_insecure_host.clone());

        let reporter = PythonDownloadReporter::single(printer);

        // Locate the Python interpreter to use in the environment.
        let python = PythonInstallation::find_or_download(
            python_request.as_ref(),
            EnvironmentPreference::OnlySystem,
            python_preference,
            python_downloads,
            &client_builder,
            cache,
            Some(&reporter),
            install_mirrors.python_install_mirror.as_deref(),
            install_mirrors.pypy_install_mirror.as_deref(),
            install_mirrors.python_downloads_json_url.as_deref(),
            preview,
        )
        .await?;

        let managed = python.source().is_managed();
        let implementation = python.implementation();
        let interpreter = python.into_interpreter();

        if managed {
            writeln!(
                printer.stderr(),
                "Using {} {}",
                implementation.pretty(),
                interpreter.python_version().cyan()
            )?;
        } else {
            writeln!(
                printer.stderr(),
                "Using {} {} interpreter at: {}",
                implementation.pretty(),
                interpreter.python_version(),
                interpreter.sys_executable().user_display().cyan()
            )?;
        }

        if let Some(requires_python) = requires_python.as_ref() {
            validate_project_requires_python(
                &interpreter,
                Some(workspace),
                groups,
                requires_python,
                &source,
            )?;
        }

        Ok(Self::Interpreter(interpreter))
    }

    /// Convert the [`ProjectInterpreter`] into an [`Interpreter`].
    pub(crate) fn into_interpreter(self) -> Interpreter {
        match self {
            ProjectInterpreter::Interpreter(interpreter) => interpreter,
            ProjectInterpreter::Environment(venv) => venv.into_interpreter(),
        }
    }

    /// Grab a file lock for the environment to prevent concurrent writes across processes.
    pub(crate) async fn lock(workspace: &Workspace) -> Result<LockedFile, std::io::Error> {
        LockedFile::acquire(
            std::env::temp_dir().join(format!(
                "uv-{}.lock",
                cache_digest(workspace.install_path())
            )),
            workspace.install_path().simplified_display(),
        )
        .await
    }
}

/// The source of a `Requires-Python` specifier.
#[derive(Debug, Clone)]
pub(crate) enum RequiresPythonSource {
    /// From the PEP 723 inline script metadata.
    Script,
    /// From a `pyproject.toml` in a workspace.
    Project,
}

#[derive(Debug, Clone)]
pub(crate) enum PythonRequestSource {
    /// The request was provided by the user.
    UserRequest,
    /// The request was inferred from a `.python-version` or `.python-versions` file.
    DotPythonVersion(PythonVersionFile),
    /// The request was inferred from a `pyproject.toml` file.
    RequiresPython,
}

impl std::fmt::Display for PythonRequestSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PythonRequestSource::UserRequest => write!(f, "explicit request"),
            PythonRequestSource::DotPythonVersion(file) => {
                write!(f, "version file at `{}`", file.path().user_display())
            }
            PythonRequestSource::RequiresPython => write!(f, "`requires-python` metadata"),
        }
    }
}

/// The resolved Python request and requirement for a [`Workspace`].
#[derive(Debug, Clone)]
pub(crate) struct WorkspacePython {
    /// The source of the Python request.
    pub(crate) source: PythonRequestSource,
    /// The resolved Python request, computed by considering (1) any explicit request from the user
    /// via `--python`, (2) any implicit request from the user via `.python-version`, and (3) any
    /// `Requires-Python` specifier in the `pyproject.toml`.
    pub(crate) python_request: Option<PythonRequest>,
    /// The resolved Python requirement for the project, computed by taking the intersection of all
    /// `Requires-Python` specifiers in the workspace.
    pub(crate) requires_python: Option<RequiresPython>,
}

impl WorkspacePython {
    /// Determine the [`WorkspacePython`] for the current [`Workspace`].
    pub(crate) async fn from_request(
        python_request: Option<PythonRequest>,
        workspace: Option<&Workspace>,
        groups: &DependencyGroupsWithDefaults,
        project_dir: &Path,
        no_config: bool,
    ) -> Result<Self, ProjectError> {
        let requires_python = workspace
            .map(|workspace| find_requires_python(workspace, groups))
            .transpose()?
            .flatten();

        let workspace_root = workspace.map(Workspace::install_path);

        let (source, python_request) = if let Some(request) = python_request {
            // (1) Explicit request from user
            let source = PythonRequestSource::UserRequest;
            let request = Some(request);
            (source, request)
        } else if let Some(file) = PythonVersionFile::discover(
            project_dir,
            &VersionFileDiscoveryOptions::default()
                .with_stop_discovery_at(workspace_root.map(PathBuf::as_ref))
                .with_no_config(no_config),
        )
        .await?
        {
            // (2) Request from `.python-version`
            let source = PythonRequestSource::DotPythonVersion(file.clone());
            let request = file.into_version();
            (source, request)
        } else {
            // (3) `requires-python` in `pyproject.toml`
            let request = requires_python
                .as_ref()
                .map(RequiresPython::specifiers)
                .map(|specifiers| {
                    PythonRequest::Version(VersionRequest::Range(
                        specifiers.clone(),
                        PythonVariant::Default,
                    ))
                });
            let source = PythonRequestSource::RequiresPython;
            (source, request)
        };

        if let Some(python_request) = python_request.as_ref() {
            debug!(
                "Using Python request `{}` from {source}",
                python_request.to_canonical_string()
            );
        }

        Ok(Self {
            source,
            python_request,
            requires_python,
        })
    }
}

/// The resolved Python request and requirement for a [`Pep723Script`]
#[derive(Debug, Clone)]
pub(crate) struct ScriptPython {
    /// The source of the Python request.
    pub(crate) source: PythonRequestSource,
    /// The resolved Python request, computed by considering (1) any explicit request from the user
    /// via `--python`, (2) any implicit request from the user via `.python-version`, (3) any
    /// `Requires-Python` specifier in the script metadata, and (4) any `Requires-Python` specifier
    /// in the `pyproject.toml`.
    pub(crate) python_request: Option<PythonRequest>,
    /// The resolved Python requirement for the script and its source.
    pub(crate) requires_python: Option<(RequiresPython, RequiresPythonSource)>,
}

impl ScriptPython {
    /// Determine the [`ScriptPython`] for the current [`Workspace`].
    pub(crate) async fn from_request(
        python_request: Option<PythonRequest>,
        workspace: Option<&Workspace>,
        script: Pep723ItemRef<'_>,
        no_config: bool,
    ) -> Result<Self, ProjectError> {
        // First, discover a requirement from the workspace
        let WorkspacePython {
            mut source,
            mut python_request,
            requires_python,
        } = WorkspacePython::from_request(
            python_request,
            workspace,
            // Scripts have no groups to hang requires-python settings off of
            &DependencyGroupsWithDefaults::none(),
            script.path().and_then(Path::parent).unwrap_or(&**CWD),
            no_config,
        )
        .await?;

        // If the script has a `requires-python` specifier, prefer that over one from the workspace.
        let requires_python =
            if let Some(requires_python_specifiers) = script.metadata().requires_python.as_ref() {
                if python_request.is_none() {
                    python_request = Some(PythonRequest::Version(VersionRequest::Range(
                        requires_python_specifiers.clone(),
                        PythonVariant::Default,
                    )));
                    source = PythonRequestSource::RequiresPython;
                }
                Some((
                    RequiresPython::from_specifiers(requires_python_specifiers),
                    RequiresPythonSource::Script,
                ))
            } else {
                requires_python.map(|requirement| (requirement, RequiresPythonSource::Project))
            };

        if let Some(python_request) = python_request.as_ref() {
            debug!("Using Python request {python_request} from {source}");
        }

        Ok(Self {
            source,
            python_request,
            requires_python,
        })
    }
}

/// The Python environment for a project.
#[derive(Debug)]
enum ProjectEnvironment {
    /// An existing [`PythonEnvironment`] was discovered, which satisfies the project's requirements.
    Existing(PythonEnvironment),
    /// An existing [`PythonEnvironment`] was discovered, but did not satisfy the project's
    /// requirements, and so was replaced.
    Replaced(PythonEnvironment),
    /// A new [`PythonEnvironment`] was created.
    Created(PythonEnvironment),
    /// An existing [`PythonEnvironment`] was discovered, but did not satisfy the project's
    /// requirements. A new environment would've been created, but `--dry-run` mode is enabled; as
    /// such, a temporary environment was created instead.
    WouldReplace(
        PathBuf,
        PythonEnvironment,
        #[allow(unused)] tempfile::TempDir,
    ),
    /// A new [`PythonEnvironment`] would've been created, but `--dry-run` mode is enabled; as such,
    /// a temporary environment was created instead.
    WouldCreate(
        PathBuf,
        PythonEnvironment,
        #[allow(unused)] tempfile::TempDir,
    ),
}

impl ProjectEnvironment {
    /// Initialize a virtual environment for the current project.
    pub(crate) async fn get_or_init(
        workspace: &Workspace,
        groups: &DependencyGroupsWithDefaults,
        python: Option<PythonRequest>,
        install_mirrors: &PythonInstallMirrors,
        network_settings: &NetworkSettings,
        python_preference: PythonPreference,
        python_downloads: PythonDownloads,
        no_sync: bool,
        no_config: bool,
        active: Option<bool>,
        cache: &Cache,
        dry_run: DryRun,
        printer: Printer,
        preview: Preview,
    ) -> Result<Self, ProjectError> {
        // Lock the project environment to avoid synchronization issues.
        let _lock = ProjectInterpreter::lock(workspace)
            .await
            .inspect_err(|err| {
                warn!("Failed to acquire project environment lock: {err}");
            })
            .ok();

        let upgradeable = preview.is_enabled(PreviewFeatures::PYTHON_UPGRADE)
            && python
                .as_ref()
                .is_none_or(|request| !request.includes_patch());

        match ProjectInterpreter::discover(
            workspace,
            workspace.install_path().as_ref(),
            groups,
            python,
            network_settings,
            python_preference,
            python_downloads,
            install_mirrors,
            no_sync,
            no_config,
            active,
            cache,
            printer,
            preview,
        )
        .await?
        {
            // If we found an existing, compatible environment, use it.
            ProjectInterpreter::Environment(environment) => Ok(Self::Existing(environment)),

            // Otherwise, create a virtual environment with the discovered interpreter.
            ProjectInterpreter::Interpreter(interpreter) => {
                let root = workspace.venv(active);

                // Avoid removing things that are not virtual environments
                let replace = match (root.try_exists(), root.join("pyvenv.cfg").try_exists()) {
                    // It's a virtual environment we can remove it
                    (_, Ok(true)) => true,
                    // It doesn't exist at all, we should use it without deleting it to avoid TOCTOU bugs
                    (Ok(false), Ok(false)) => false,
                    // If it's not a virtual environment, bail
                    (Ok(true), Ok(false)) => {
                        // Unless it's empty, in which case we just ignore it
                        if root.read_dir().is_ok_and(|mut dir| dir.next().is_none()) {
                            false
                        } else {
                            return Err(ProjectError::InvalidProjectEnvironmentDir(
                                root,
                                "it is not a compatible environment but cannot be recreated because it is not a virtual environment".to_string(),
                            ));
                        }
                    }
                    // Similarly, if we can't _tell_ if it exists we should bail
                    (_, Err(err)) | (Err(err), _) => {
                        return Err(ProjectError::InvalidProjectEnvironmentDir(
                            root,
                            format!(
                                "it is not a compatible environment but cannot be recreated because uv cannot determine if it is a virtual environment: {err}"
                            ),
                        ));
                    }
                };

                // Determine a prompt for the environment, in order of preference:
                //
                // 1) The name of the project
                // 2) The name of the directory at the root of the workspace
                // 3) No prompt
                let prompt = workspace
                    .pyproject_toml()
                    .project
                    .as_ref()
                    .map(|p| p.name.to_string())
                    .or_else(|| {
                        workspace
                            .install_path()
                            .file_name()
                            .map(|f| f.to_string_lossy().to_string())
                    })
                    .map(uv_virtualenv::Prompt::Static)
                    .unwrap_or(uv_virtualenv::Prompt::None);

                // Under `--dry-run`, avoid modifying the environment.
                if dry_run.enabled() {
                    let temp_dir = cache.venv_dir()?;
                    let environment = uv_virtualenv::create_venv(
                        temp_dir.path(),
                        interpreter,
                        prompt,
                        false,
                        uv_virtualenv::OnExisting::Remove,
                        false,
                        false,
                        upgradeable,
                        preview,
                    )?;
                    return Ok(if replace {
                        Self::WouldReplace(root, environment, temp_dir)
                    } else {
                        Self::WouldCreate(root, environment, temp_dir)
                    });
                }

                // Remove the existing virtual environment if it doesn't meet the requirements.
                if replace {
                    match remove_virtualenv(&root) {
                        Ok(()) => {
                            writeln!(
                                printer.stderr(),
                                "Removed virtual environment at: {}",
                                root.user_display().cyan()
                            )?;
                        }
                        Err(uv_virtualenv::Error::Io(err))
                            if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => return Err(err.into()),
                    }
                }

                writeln!(
                    printer.stderr(),
                    "Creating virtual environment at: {}",
                    root.user_display().cyan()
                )?;

                let environment = uv_virtualenv::create_venv(
                    &root,
                    interpreter,
                    prompt,
                    false,
                    uv_virtualenv::OnExisting::Remove,
                    false,
                    false,
                    upgradeable,
                    preview,
                )?;

                if replace {
                    Ok(Self::Replaced(environment))
                } else {
                    Ok(Self::Created(environment))
                }
            }
        }
    }

    /// Convert the [`ProjectEnvironment`] into a [`PythonEnvironment`].
    ///
    /// Returns an error if the environment was created in `--dry-run` mode, as dropping the
    /// associated temporary directory could lead to errors downstream.
    #[allow(clippy::result_large_err)]
    pub(crate) fn into_environment(self) -> Result<PythonEnvironment, ProjectError> {
        match self {
            Self::Existing(environment) => Ok(environment),
            Self::Replaced(environment) => Ok(environment),
            Self::Created(environment) => Ok(environment),
            Self::WouldReplace(..) => Err(ProjectError::DroppedEnvironment),
            Self::WouldCreate(..) => Err(ProjectError::DroppedEnvironment),
        }
    }

    /// Return the path to the actual target, if this was a dry run environment.
    pub(crate) fn dry_run_target(&self) -> Option<&Path> {
        match self {
            Self::WouldReplace(path, _, _) | Self::WouldCreate(path, _, _) => Some(path),
            Self::Created(_) | Self::Existing(_) | Self::Replaced(_) => None,
        }
    }
}

impl std::ops::Deref for ProjectEnvironment {
    type Target = PythonEnvironment;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Existing(environment) => environment,
            Self::Replaced(environment) => environment,
            Self::Created(environment) => environment,
            Self::WouldReplace(_, environment, _) => environment,
            Self::WouldCreate(_, environment, _) => environment,
        }
    }
}

/// The Python environment for a script.
#[derive(Debug)]
enum ScriptEnvironment {
    /// An existing [`PythonEnvironment`] was discovered, which satisfies the script's requirements.
    Existing(PythonEnvironment),
    /// An existing [`PythonEnvironment`] was discovered, but did not satisfy the script's
    /// requirements, and so was replaced.
    Replaced(PythonEnvironment),
    /// A new [`PythonEnvironment`] was created for the script.
    Created(PythonEnvironment),
    /// An existing [`PythonEnvironment`] was discovered, but did not satisfy the script's
    /// requirements. A new environment would've been created, but `--dry-run` mode is enabled; as
    /// such, a temporary environment was created instead.
    WouldReplace(
        PathBuf,
        PythonEnvironment,
        #[allow(unused)] tempfile::TempDir,
    ),
    /// A new [`PythonEnvironment`] would've been created, but `--dry-run` mode is enabled; as such,
    /// a temporary environment was created instead.
    WouldCreate(
        PathBuf,
        PythonEnvironment,
        #[allow(unused)] tempfile::TempDir,
    ),
}

impl ScriptEnvironment {
    /// Initialize a virtual environment for a PEP 723 script.
    pub(crate) async fn get_or_init(
        script: Pep723ItemRef<'_>,
        python_request: Option<PythonRequest>,
        network_settings: &NetworkSettings,
        python_preference: PythonPreference,
        python_downloads: PythonDownloads,
        install_mirrors: &PythonInstallMirrors,
        no_sync: bool,
        no_config: bool,
        active: Option<bool>,
        cache: &Cache,
        dry_run: DryRun,
        printer: Printer,
        preview: Preview,
    ) -> Result<Self, ProjectError> {
        // Lock the script environment to avoid synchronization issues.
        let _lock = ScriptInterpreter::lock(script)
            .await
            .inspect_err(|err| {
                warn!("Failed to acquire script environment lock: {err}");
            })
            .ok();

        let upgradeable = python_request
            .as_ref()
            .is_none_or(|request| !request.includes_patch());

        match ScriptInterpreter::discover(
            script,
            python_request,
            network_settings,
            python_preference,
            python_downloads,
            install_mirrors,
            no_sync,
            no_config,
            active,
            cache,
            printer,
            preview,
        )
        .await?
        {
            // If we found an existing, compatible environment, use it.
            ScriptInterpreter::Environment(environment) => Ok(Self::Existing(environment)),

            // Otherwise, create a virtual environment with the discovered interpreter.
            ScriptInterpreter::Interpreter(interpreter) => {
                let root = ScriptInterpreter::root(script, active, cache);

                // Determine a prompt for the environment, in order of preference:
                //
                // 1) The name of the script
                // 2) No prompt
                let prompt = script
                    .path()
                    .and_then(|path| path.file_name())
                    .map(|f| f.to_string_lossy().to_string())
                    .map(uv_virtualenv::Prompt::Static)
                    .unwrap_or(uv_virtualenv::Prompt::None);

                // Under `--dry-run`, avoid modifying the environment.
                if dry_run.enabled() {
                    let temp_dir = cache.venv_dir()?;
                    let environment = uv_virtualenv::create_venv(
                        temp_dir.path(),
                        interpreter,
                        prompt,
                        false,
                        uv_virtualenv::OnExisting::Remove,
                        false,
                        false,
                        upgradeable,
                        preview,
                    )?;
                    return Ok(if root.exists() {
                        Self::WouldReplace(root, environment, temp_dir)
                    } else {
                        Self::WouldCreate(root, environment, temp_dir)
                    });
                }

                // Remove the existing virtual environment.
                let replaced = match fs_err::remove_dir_all(&root) {
                    Ok(()) => {
                        debug!(
                            "Removed virtual environment at: {}",
                            root.user_display().cyan()
                        );
                        true
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
                    Err(err) => return Err(err.into()),
                };

                debug!(
                    "Creating script environment at: {}",
                    root.user_display().cyan()
                );

                let environment = uv_virtualenv::create_venv(
                    &root,
                    interpreter,
                    prompt,
                    false,
                    uv_virtualenv::OnExisting::Remove,
                    false,
                    false,
                    upgradeable,
                    preview,
                )?;

                Ok(if replaced {
                    Self::Replaced(environment)
                } else {
                    Self::Created(environment)
                })
            }
        }
    }

    /// Convert the [`ScriptEnvironment`] into a [`PythonEnvironment`].
    ///
    /// Returns an error if the environment was created in `--dry-run` mode, as dropping the
    /// associated temporary directory could lead to errors downstream.
    #[allow(clippy::result_large_err)]
    pub(crate) fn into_environment(self) -> Result<PythonEnvironment, ProjectError> {
        match self {
            Self::Existing(environment) => Ok(environment),
            Self::Replaced(environment) => Ok(environment),
            Self::Created(environment) => Ok(environment),
            Self::WouldReplace(..) => Err(ProjectError::DroppedEnvironment),
            Self::WouldCreate(..) => Err(ProjectError::DroppedEnvironment),
        }
    }

    /// Return the path to the actual target, if this was a dry run environment.
    pub(crate) fn dry_run_target(&self) -> Option<&Path> {
        match self {
            Self::WouldReplace(path, _, _) | Self::WouldCreate(path, _, _) => Some(path),
            Self::Created(_) | Self::Existing(_) | Self::Replaced(_) => None,
        }
    }
}

impl std::ops::Deref for ScriptEnvironment {
    type Target = PythonEnvironment;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Existing(environment) => environment,
            Self::Replaced(environment) => environment,
            Self::Created(environment) => environment,
            Self::WouldReplace(_, environment, _) => environment,
            Self::WouldCreate(_, environment, _) => environment,
        }
    }
}

/// Resolve any [`UnresolvedRequirementSpecification`] into a fully-qualified [`Requirement`].
pub(crate) async fn resolve_names(
    requirements: Vec<UnresolvedRequirementSpecification>,
    interpreter: &Interpreter,
    settings: &ResolverInstallerSettings,
    network_settings: &NetworkSettings,
    state: &SharedState,
    concurrency: Concurrency,
    cache: &Cache,
    workspace_cache: &WorkspaceCache,
    printer: Printer,
    preview: Preview,
) -> Result<Vec<Requirement>, uv_requirements::Error> {
    // Partition the requirements into named and unnamed requirements.
    let (mut requirements, unnamed): (Vec<_>, Vec<_>) =
        requirements
            .into_iter()
            .partition_map(|spec| match spec.requirement {
                UnresolvedRequirement::Named(requirement) => itertools::Either::Left(requirement),
                UnresolvedRequirement::Unnamed(requirement) => {
                    itertools::Either::Right(requirement)
                }
            });

    // Short-circuit if there are no unnamed requirements.
    if unnamed.is_empty() {
        return Ok(requirements);
    }

    // Extract the project settings.
    let ResolverInstallerSettings {
        resolver:
            ResolverSettings {
                build_options,
                config_setting,
                config_settings_package,
                dependency_metadata,
                exclude_newer,
                fork_strategy: _,
                index_locations,
                index_strategy,
                keyring_provider,
                link_mode,
                no_build_isolation,
                no_build_isolation_package,
                extra_build_dependencies,
                prerelease: _,
                resolution: _,
                sources,
                upgrade: _,
            },
        compile_bytecode: _,
        reinstall: _,
    } = settings;

    let client_builder = BaseClientBuilder::new()
        .retries_from_env()
        .map_err(uv_requirements::Error::ClientError)?
        .connectivity(network_settings.connectivity)
        .native_tls(network_settings.native_tls)
        .keyring(*keyring_provider)
        .allow_insecure_host(network_settings.allow_insecure_host.clone());

    index_locations.cache_index_credentials();

    // Initialize the registry client.
    let client = RegistryClientBuilder::try_from(client_builder)?
        .cache(cache.clone())
        .index_locations(index_locations)
        .index_strategy(*index_strategy)
        .markers(interpreter.markers())
        .platform(interpreter.platform())
        .build();

    // Determine whether to enable build isolation.
    let environment;
    let build_isolation = if *no_build_isolation {
        environment = PythonEnvironment::from_interpreter(interpreter.clone());
        BuildIsolation::Shared(&environment)
    } else if no_build_isolation_package.is_empty() {
        BuildIsolation::Isolated
    } else {
        environment = PythonEnvironment::from_interpreter(interpreter.clone());
        BuildIsolation::SharedPackage(&environment, no_build_isolation_package)
    };

    // TODO(charlie): These are all default values. We should consider whether we want to make them
    // optional on the downstream APIs.
    let hasher = HashStrategy::default();
    let flat_index = FlatIndex::default();
    let build_constraints = Constraints::default();
    let build_hasher = HashStrategy::default();

    // Create a build dispatch.
    let extra_build_requires =
        uv_distribution::ExtraBuildRequires::from_lowered(extra_build_dependencies.clone());
    let build_dispatch = BuildDispatch::new(
        &client,
        cache,
        build_constraints,
        interpreter,
        index_locations,
        &flat_index,
        dependency_metadata,
        state.clone(),
        *index_strategy,
        config_setting,
        config_settings_package,
        build_isolation,
        &extra_build_requires,
        *link_mode,
        build_options,
        &build_hasher,
        exclude_newer.clone(),
        *sources,
        workspace_cache.clone(),
        concurrency,
        preview,
    );

    // Resolve the unnamed requirements.
    requirements.extend(
        NamedRequirementsResolver::new(
            &hasher,
            state.index(),
            DistributionDatabase::new(&client, &build_dispatch, concurrency.downloads),
        )
        .with_reporter(Arc::new(ResolverReporter::from(printer)))
        .resolve(unnamed.into_iter())
        .await?,
    );

    Ok(requirements)
}

#[derive(Debug, Clone)]
pub(crate) enum PreferenceLocation<'lock> {
    /// The preferences should be extracted from a lockfile.
    Lock {
        lock: &'lock Lock,
        install_path: &'lock Path,
    },
    /// The preferences will be provided directly as [`Preference`] entries.
    Entries(Vec<Preference>),
}

#[derive(Debug, Clone)]
pub(crate) struct EnvironmentSpecification<'lock> {
    /// The requirements to include in the environment.
    requirements: RequirementsSpecification,
    /// The preferences to respect when resolving.
    preferences: Option<PreferenceLocation<'lock>>,
}

impl From<RequirementsSpecification> for EnvironmentSpecification<'_> {
    fn from(requirements: RequirementsSpecification) -> Self {
        Self {
            requirements,
            preferences: None,
        }
    }
}

impl<'lock> EnvironmentSpecification<'lock> {
    /// Set the [`PreferenceLocation`] for the specification.
    #[must_use]
    pub(crate) fn with_preferences(self, preferences: PreferenceLocation<'lock>) -> Self {
        Self {
            preferences: Some(preferences),
            ..self
        }
    }
}

/// Run dependency resolution for an interpreter, returning the [`ResolverOutput`].
pub(crate) async fn resolve_environment(
    spec: EnvironmentSpecification<'_>,
    interpreter: &Interpreter,
    build_constraints: Constraints,
    settings: &ResolverSettings,
    network_settings: &NetworkSettings,
    state: &PlatformState,
    logger: Box<dyn ResolveLogger>,
    concurrency: Concurrency,
    cache: &Cache,
    printer: Printer,
    preview: Preview,
) -> Result<ResolverOutput, ProjectError> {
    warn_on_requirements_txt_setting(&spec.requirements, settings);

    let ResolverSettings {
        index_locations,
        index_strategy,
        keyring_provider,
        resolution,
        prerelease,
        fork_strategy,
        dependency_metadata,
        config_setting,
        config_settings_package,
        no_build_isolation,
        no_build_isolation_package,
        extra_build_dependencies,
        exclude_newer,
        link_mode,
        upgrade: _,
        build_options,
        sources,
    } = settings;

    // Respect all requirements from the provided sources.
    let RequirementsSpecification {
        project,
        requirements,
        constraints,
        overrides,
        source_trees,
        ..
    } = spec.requirements;

    let client_builder = BaseClientBuilder::new()
        .retries_from_env()?
        .connectivity(network_settings.connectivity)
        .native_tls(network_settings.native_tls)
        .keyring(*keyring_provider)
        .allow_insecure_host(network_settings.allow_insecure_host.clone());

    // Determine the tags, markers, and interpreter to use for resolution.
    let tags = interpreter.tags()?;
    let marker_env = interpreter.resolver_marker_environment();
    let python_requirement = PythonRequirement::from_interpreter(interpreter);

    index_locations.cache_index_credentials();

    // Initialize the registry client.
    let client = RegistryClientBuilder::try_from(client_builder)?
        .cache(cache.clone())
        .index_locations(index_locations)
        .index_strategy(*index_strategy)
        .markers(interpreter.markers())
        .platform(interpreter.platform())
        .build();

    // Determine whether to enable build isolation.
    let environment;
    let build_isolation = if *no_build_isolation {
        environment = PythonEnvironment::from_interpreter(interpreter.clone());
        BuildIsolation::Shared(&environment)
    } else if no_build_isolation_package.is_empty() {
        BuildIsolation::Isolated
    } else {
        environment = PythonEnvironment::from_interpreter(interpreter.clone());
        BuildIsolation::SharedPackage(&environment, no_build_isolation_package)
    };

    let options = OptionsBuilder::new()
        .resolution_mode(*resolution)
        .prerelease_mode(*prerelease)
        .fork_strategy(*fork_strategy)
        .exclude_newer(exclude_newer.clone())
        .index_strategy(*index_strategy)
        .build_options(build_options.clone())
        .build();

    // TODO(charlie): These are all default values. We should consider whether we want to make them
    // optional on the downstream APIs.
    let extras = ExtrasSpecification::default();
    let groups = BTreeMap::new();
    let hasher = HashStrategy::default();
    let build_hasher = HashStrategy::default();

    // When resolving from an interpreter, we assume an empty environment, so reinstalls and
    // upgrades aren't relevant.
    let reinstall = Reinstall::default();
    let upgrade = Upgrade::default();

    // If an existing lockfile exists, build up a set of preferences.
    let preferences = match spec.preferences {
        Some(PreferenceLocation::Lock { lock, install_path }) => {
            let LockedRequirements { preferences, git } =
                read_lock_requirements(lock, install_path, &upgrade)?;

            // Populate the Git resolver.
            for ResolvedRepositoryReference { reference, sha } in git {
                debug!("Inserting Git reference into resolver: `{reference:?}` at `{sha}`");
                state.git().insert(reference, sha);
            }

            preferences
        }
        Some(PreferenceLocation::Entries(entries)) => entries,
        None => vec![],
    };

    // Resolve the flat indexes from `--find-links`.
    let flat_index = {
        let client = FlatIndexClient::new(client.cached_client(), client.connectivity(), cache);
        let entries = client
            .fetch_all(index_locations.flat_indexes().map(Index::url))
            .await?;
        FlatIndex::from_entries(entries, Some(tags), &hasher, build_options)
    };

    let workspace_cache = WorkspaceCache::default();

    // Create a build dispatch.
    let extra_build_requires =
        uv_distribution::ExtraBuildRequires::from_lowered(extra_build_dependencies.clone());
    let resolve_dispatch = BuildDispatch::new(
        &client,
        cache,
        build_constraints,
        interpreter,
        index_locations,
        &flat_index,
        dependency_metadata,
        state.clone().into_inner(),
        *index_strategy,
        config_setting,
        config_settings_package,
        build_isolation,
        &extra_build_requires,
        *link_mode,
        build_options,
        &build_hasher,
        exclude_newer.clone(),
        *sources,
        workspace_cache,
        concurrency,
        preview,
    );

    // Resolve the requirements.
    Ok(pip::operations::resolve(
        requirements,
        constraints,
        overrides,
        source_trees,
        project,
        BTreeSet::default(),
        &extras,
        &groups,
        preferences,
        EmptyInstalledPackages,
        &hasher,
        &reinstall,
        &upgrade,
        Some(tags),
        ResolverEnvironment::specific(marker_env),
        python_requirement,
        interpreter.markers(),
        Conflicts::empty(),
        &client,
        &flat_index,
        state.index(),
        &resolve_dispatch,
        concurrency,
        options,
        logger,
        printer,
    )
    .await?)
}

/// Sync a [`PythonEnvironment`] with a set of resolved requirements.
pub(crate) async fn sync_environment(
    venv: PythonEnvironment,
    resolution: &Resolution,
    modifications: Modifications,
    build_constraints: Constraints,
    settings: InstallerSettingsRef<'_>,
    network_settings: &NetworkSettings,
    state: &PlatformState,
    logger: Box<dyn InstallLogger>,
    installer_metadata: bool,
    concurrency: Concurrency,
    cache: &Cache,
    printer: Printer,
    preview: Preview,
) -> Result<PythonEnvironment, ProjectError> {
    let InstallerSettingsRef {
        index_locations,
        index_strategy,
        keyring_provider,
        dependency_metadata,
        config_setting,
        config_settings_package,
        no_build_isolation,
        no_build_isolation_package,
        extra_build_dependencies,
        exclude_newer,
        link_mode,
        compile_bytecode,
        reinstall,
        build_options,
        sources,
    } = settings;

    let client_builder = BaseClientBuilder::new()
        .retries_from_env()?
        .connectivity(network_settings.connectivity)
        .native_tls(network_settings.native_tls)
        .keyring(keyring_provider)
        .allow_insecure_host(network_settings.allow_insecure_host.clone());

    let site_packages = SitePackages::from_environment(&venv)?;

    // Determine the markers tags to use for resolution.
    let interpreter = venv.interpreter();
    let tags = venv.interpreter().tags()?;

    index_locations.cache_index_credentials();

    // Initialize the registry client.
    let client = RegistryClientBuilder::try_from(client_builder)?
        .cache(cache.clone())
        .index_locations(index_locations)
        .index_strategy(index_strategy)
        .markers(interpreter.markers())
        .platform(interpreter.platform())
        .build();

    // Determine whether to enable build isolation.
    let build_isolation = if no_build_isolation {
        BuildIsolation::Shared(&venv)
    } else if no_build_isolation_package.is_empty() {
        BuildIsolation::Isolated
    } else {
        BuildIsolation::SharedPackage(&venv, no_build_isolation_package)
    };

    // TODO(charlie): These are all default values. We should consider whether we want to make them
    // optional on the downstream APIs.
    let build_hasher = HashStrategy::default();
    let dry_run = DryRun::default();
    let hasher = HashStrategy::default();
    let workspace_cache = WorkspaceCache::default();

    // Resolve the flat indexes from `--find-links`.
    let flat_index = {
        let client = FlatIndexClient::new(client.cached_client(), client.connectivity(), cache);
        let entries = client
            .fetch_all(index_locations.flat_indexes().map(Index::url))
            .await?;
        FlatIndex::from_entries(entries, Some(tags), &hasher, build_options)
    };

    // Create a build dispatch.
    let extra_build_requires =
        uv_distribution::ExtraBuildRequires::from_lowered(extra_build_dependencies.clone());
    let build_dispatch = BuildDispatch::new(
        &client,
        cache,
        build_constraints,
        interpreter,
        index_locations,
        &flat_index,
        dependency_metadata,
        state.clone().into_inner(),
        index_strategy,
        config_setting,
        config_settings_package,
        build_isolation,
        &extra_build_requires,
        link_mode,
        build_options,
        &build_hasher,
        exclude_newer,
        sources,
        workspace_cache,
        concurrency,
        preview,
    );

    // Sync the environment.
    pip::operations::install(
        resolution,
        site_packages,
        modifications,
        reinstall,
        build_options,
        link_mode,
        compile_bytecode,
        index_locations,
        config_setting,
        config_settings_package,
        &hasher,
        tags,
        &client,
        state.in_flight(),
        concurrency,
        &build_dispatch,
        cache,
        &venv,
        logger,
        installer_metadata,
        dry_run,
        printer,
    )
    .await?;

    // Notify the user of any resolution diagnostics.
    pip::operations::diagnose_resolution(resolution.diagnostics(), printer)?;

    Ok(venv)
}

/// The result of updating a [`PythonEnvironment`] to satisfy a set of [`RequirementsSource`]s.
#[derive(Debug)]
pub(crate) struct EnvironmentUpdate {
    /// The updated [`PythonEnvironment`].
    pub(crate) environment: PythonEnvironment,
    /// The [`Changelog`] of changes made to the environment.
    pub(crate) changelog: Changelog,
}

impl EnvironmentUpdate {
    /// Convert the [`EnvironmentUpdate`] into a [`PythonEnvironment`].
    pub(crate) fn into_environment(self) -> PythonEnvironment {
        self.environment
    }
}

/// Update a [`PythonEnvironment`] to satisfy a set of [`RequirementsSource`]s.
pub(crate) async fn update_environment(
    venv: PythonEnvironment,
    spec: RequirementsSpecification,
    modifications: Modifications,
    build_constraints: Constraints,
    extra_build_requires: uv_distribution::ExtraBuildRequires,
    settings: &ResolverInstallerSettings,
    network_settings: &NetworkSettings,
    state: &SharedState,
    resolve: Box<dyn ResolveLogger>,
    install: Box<dyn InstallLogger>,
    installer_metadata: bool,
    concurrency: Concurrency,
    cache: &Cache,
    workspace_cache: WorkspaceCache,
    dry_run: DryRun,
    printer: Printer,
    preview: Preview,
) -> Result<EnvironmentUpdate, ProjectError> {
    warn_on_requirements_txt_setting(&spec, &settings.resolver);

    let ResolverInstallerSettings {
        resolver:
            ResolverSettings {
                build_options,
                config_setting,
                config_settings_package,
                dependency_metadata,
                exclude_newer,
                fork_strategy,
                index_locations,
                index_strategy,
                keyring_provider,
                link_mode,
                no_build_isolation,
                no_build_isolation_package,
                extra_build_dependencies: _,
                prerelease,
                resolution,
                sources,
                upgrade,
            },
        compile_bytecode,
        reinstall,
    } = settings;

    let client_builder = BaseClientBuilder::new()
        .retries_from_env()?
        .connectivity(network_settings.connectivity)
        .native_tls(network_settings.native_tls)
        .keyring(*keyring_provider)
        .allow_insecure_host(network_settings.allow_insecure_host.clone());

    // Respect all requirements from the provided sources.
    let RequirementsSpecification {
        project,
        requirements,
        constraints,
        overrides,
        source_trees,
        ..
    } = spec;

    // Determine markers to use for resolution.
    let interpreter = venv.interpreter();
    let marker_env = venv.interpreter().resolver_marker_environment();

    // Check if the current environment satisfies the requirements
    let site_packages = SitePackages::from_environment(&venv)?;
    if reinstall.is_none()
        && upgrade.is_none()
        && source_trees.is_empty()
        && matches!(modifications, Modifications::Sufficient)
    {
        match site_packages.satisfies_spec(&requirements, &constraints, &overrides, &marker_env)? {
            // If the requirements are already satisfied, we're done.
            SatisfiesResult::Fresh {
                recursive_requirements,
            } => {
                if recursive_requirements.is_empty() {
                    debug!("No requirements to install");
                } else {
                    debug!(
                        "All requirements satisfied: {}",
                        recursive_requirements
                            .iter()
                            .map(ToString::to_string)
                            .sorted()
                            .join(" | ")
                    );
                }
                return Ok(EnvironmentUpdate {
                    environment: venv,
                    changelog: Changelog::default(),
                });
            }
            SatisfiesResult::Unsatisfied(requirement) => {
                debug!("At least one requirement is not satisfied: {requirement}");
            }
        }
    }

    index_locations.cache_index_credentials();

    // Initialize the registry client.
    let client = RegistryClientBuilder::try_from(client_builder)?
        .cache(cache.clone())
        .index_locations(index_locations)
        .index_strategy(*index_strategy)
        .markers(interpreter.markers())
        .platform(interpreter.platform())
        .build();

    // Determine whether to enable build isolation.
    let build_isolation = if *no_build_isolation {
        BuildIsolation::Shared(&venv)
    } else if no_build_isolation_package.is_empty() {
        BuildIsolation::Isolated
    } else {
        BuildIsolation::SharedPackage(&venv, no_build_isolation_package)
    };

    let options = OptionsBuilder::new()
        .resolution_mode(*resolution)
        .prerelease_mode(*prerelease)
        .fork_strategy(*fork_strategy)
        .exclude_newer(exclude_newer.clone())
        .index_strategy(*index_strategy)
        .build_options(build_options.clone())
        .build();

    // TODO(charlie): These are all default values. We should consider whether we want to make them
    // optional on the downstream APIs.
    let build_hasher = HashStrategy::default();
    let extras = ExtrasSpecification::default();
    let groups = BTreeMap::new();
    let hasher = HashStrategy::default();
    let preferences = Vec::default();

    // Determine the tags to use for resolution.
    let tags = venv.interpreter().tags()?;
    let python_requirement = PythonRequirement::from_interpreter(interpreter);

    // Resolve the flat indexes from `--find-links`.
    let flat_index = {
        let client = FlatIndexClient::new(client.cached_client(), client.connectivity(), cache);
        let entries = client
            .fetch_all(index_locations.flat_indexes().map(Index::url))
            .await?;
        FlatIndex::from_entries(entries, Some(tags), &hasher, build_options)
    };

    // Create a build dispatch.
    let build_dispatch = BuildDispatch::new(
        &client,
        cache,
        build_constraints,
        interpreter,
        index_locations,
        &flat_index,
        dependency_metadata,
        state.clone(),
        *index_strategy,
        config_setting,
        config_settings_package,
        build_isolation,
        &extra_build_requires,
        *link_mode,
        build_options,
        &build_hasher,
        exclude_newer.clone(),
        *sources,
        workspace_cache,
        concurrency,
        preview,
    );

    // Resolve the requirements.
    let resolution = match pip::operations::resolve(
        requirements,
        constraints,
        overrides,
        source_trees,
        project,
        BTreeSet::default(),
        &extras,
        &groups,
        preferences,
        site_packages.clone(),
        &hasher,
        reinstall,
        upgrade,
        Some(tags),
        ResolverEnvironment::specific(marker_env.clone()),
        python_requirement,
        venv.interpreter().markers(),
        Conflicts::empty(),
        &client,
        &flat_index,
        state.index(),
        &build_dispatch,
        concurrency,
        options,
        resolve,
        printer,
    )
    .await
    {
        Ok(resolution) => Resolution::from(resolution),
        Err(err) => return Err(err.into()),
    };

    // Sync the environment.
    let changelog = pip::operations::install(
        &resolution,
        site_packages,
        modifications,
        reinstall,
        build_options,
        *link_mode,
        *compile_bytecode,
        index_locations,
        config_setting,
        config_settings_package,
        &hasher,
        tags,
        &client,
        state.in_flight(),
        concurrency,
        &build_dispatch,
        cache,
        &venv,
        install,
        installer_metadata,
        dry_run,
        printer,
    )
    .await?;

    // Notify the user of any resolution diagnostics.
    pip::operations::diagnose_resolution(resolution.diagnostics(), printer)?;

    Ok(EnvironmentUpdate {
        environment: venv,
        changelog,
    })
}

/// Determine the [`RequiresPython`] requirement for a new PEP 723 script.
pub(crate) async fn init_script_python_requirement(
    python: Option<&str>,
    install_mirrors: &PythonInstallMirrors,
    directory: &Path,
    no_pin_python: bool,
    python_preference: PythonPreference,
    python_downloads: PythonDownloads,
    no_config: bool,
    client_builder: &BaseClientBuilder<'_>,
    cache: &Cache,
    reporter: &PythonDownloadReporter,
    preview: Preview,
) -> anyhow::Result<RequiresPython> {
    let python_request = if let Some(request) = python {
        // (1) Explicit request from user
        PythonRequest::parse(request)
    } else if let (false, Some(request)) = (
        no_pin_python,
        PythonVersionFile::discover(
            directory,
            &VersionFileDiscoveryOptions::default().with_no_config(no_config),
        )
        .await?
        .and_then(PythonVersionFile::into_version),
    ) {
        // (2) Request from `.python-version`
        request
    } else {
        // (3) Assume any Python version
        PythonRequest::Any
    };

    let interpreter = PythonInstallation::find_or_download(
        Some(&python_request),
        EnvironmentPreference::Any,
        python_preference,
        python_downloads,
        client_builder,
        cache,
        Some(reporter),
        install_mirrors.python_install_mirror.as_deref(),
        install_mirrors.pypy_install_mirror.as_deref(),
        install_mirrors.python_downloads_json_url.as_deref(),
        preview,
    )
    .await?
    .into_interpreter();

    Ok(RequiresPython::greater_than_equal_version(
        &interpreter.python_minor_version(),
    ))
}

/// Returns the default dependency groups from the [`PyProjectToml`].
#[allow(clippy::result_large_err)]
pub(crate) fn default_dependency_groups(
    pyproject_toml: &PyProjectToml,
) -> Result<DefaultGroups, ProjectError> {
    if let Some(defaults) = pyproject_toml
        .tool
        .as_ref()
        .and_then(|tool| tool.uv.as_ref().and_then(|uv| uv.default_groups.as_ref()))
    {
        if let DefaultGroups::List(defaults) = defaults {
            for group in defaults {
                if !pyproject_toml
                    .dependency_groups
                    .as_ref()
                    .is_some_and(|groups| groups.contains_key(group))
                {
                    return Err(ProjectError::MissingDefaultGroup(group.clone()));
                }
            }
        }
        Ok(defaults.clone())
    } else {
        Ok(DefaultGroups::List(vec![DEV_DEPENDENCIES.clone()]))
    }
}

/// Validate that we aren't trying to install extras or groups that
/// are declared as conflicting.
#[allow(clippy::result_large_err)]
pub(crate) fn detect_conflicts(
    lock: &Lock,
    extras: &ExtrasSpecification,
    groups: &DependencyGroupsWithDefaults,
) -> Result<(), ProjectError> {
    // Note that we need to collect all extras and groups that match in
    // a particular set, since extras can be declared as conflicting with
    // groups. So if extra `x` and group `g` are declared as conflicting,
    // then enabling both of those should result in an error.
    let conflicts = lock.conflicts();
    for set in conflicts.iter() {
        let mut conflicts: Vec<ConflictPackage> = vec![];
        for item in set.iter() {
            if item
                .extra()
                .map(|extra| extras.contains(extra))
                .unwrap_or(false)
            {
                conflicts.push(item.conflict().clone());
            }
            if item
                .group()
                .map(|group| groups.contains(group))
                .unwrap_or(false)
            {
                conflicts.push(item.conflict().clone());
            }
        }
        if conflicts.len() >= 2 {
            return Err(ProjectError::Conflict(ConflictError {
                set: set.clone(),
                conflicts,
                groups: groups.clone(),
            }));
        }
    }
    Ok(())
}

/// Determine the [`RequirementsSpecification`] for a script.
#[allow(clippy::result_large_err)]
pub(crate) fn script_specification(
    script: Pep723ItemRef<'_>,
    settings: &ResolverSettings,
) -> Result<Option<RequirementsSpecification>, ProjectError> {
    let Some(dependencies) = script.metadata().dependencies.as_ref() else {
        return Ok(None);
    };

    let script_dir = script.directory()?;
    let script_indexes = script.indexes(settings.sources);
    let script_sources = script.sources(settings.sources);

    let requirements = dependencies
        .iter()
        .cloned()
        .flat_map(|requirement| {
            LoweredRequirement::from_non_workspace_requirement(
                requirement,
                script_dir.as_ref(),
                script_sources,
                script_indexes,
                &settings.index_locations,
            )
            .map_ok(LoweredRequirement::into_inner)
        })
        .collect::<Result<_, _>>()?;
    let constraints = script
        .metadata()
        .tool
        .as_ref()
        .and_then(|tool| tool.uv.as_ref())
        .and_then(|uv| uv.constraint_dependencies.as_ref())
        .into_iter()
        .flatten()
        .cloned()
        .flat_map(|requirement| {
            LoweredRequirement::from_non_workspace_requirement(
                requirement,
                script_dir.as_ref(),
                script_sources,
                script_indexes,
                &settings.index_locations,
            )
            .map_ok(LoweredRequirement::into_inner)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let overrides = script
        .metadata()
        .tool
        .as_ref()
        .and_then(|tool| tool.uv.as_ref())
        .and_then(|uv| uv.override_dependencies.as_ref())
        .into_iter()
        .flatten()
        .cloned()
        .flat_map(|requirement| {
            LoweredRequirement::from_non_workspace_requirement(
                requirement,
                script_dir.as_ref(),
                script_sources,
                script_indexes,
                &settings.index_locations,
            )
            .map_ok(LoweredRequirement::into_inner)
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Some(RequirementsSpecification::from_overrides(
        requirements,
        constraints,
        overrides,
    )))
}

/// Determine the extra build requires for a script.
#[allow(clippy::result_large_err)]
pub(crate) fn script_extra_build_requires(
    script: Pep723ItemRef<'_>,
    settings: &ResolverSettings,
) -> Result<uv_distribution::ExtraBuildRequires, ProjectError> {
    let script_dir = script.directory()?;
    let script_indexes = script.indexes(settings.sources);
    let script_sources = script.sources(settings.sources);

    // Collect any `tool.uv.extra-build-dependencies` from the script.
    let empty = BTreeMap::default();
    let script_extra_build_dependencies = script
        .metadata()
        .tool
        .as_ref()
        .and_then(|tool| tool.uv.as_ref())
        .and_then(|uv| uv.extra_build_dependencies.as_ref())
        .unwrap_or(&empty);

    // Lower the extra build dependencies
    let mut extra_build_dependencies = ExtraBuildDependencies::default();
    for (name, requirements) in script_extra_build_dependencies {
        let lowered_requirements: Vec<_> = requirements
            .iter()
            .cloned()
            .flat_map(|requirement| {
                LoweredRequirement::from_non_workspace_requirement(
                    requirement,
                    script_dir.as_ref(),
                    script_sources,
                    script_indexes,
                    &settings.index_locations,
                )
                .map_ok(|req| req.into_inner().into())
            })
            .collect::<Result<Vec<_>, _>>()?;
        extra_build_dependencies.insert(name.clone(), lowered_requirements);
    }

    Ok(uv_distribution::ExtraBuildRequires::from_lowered(
        extra_build_dependencies,
    ))
}

/// Warn if the user provides (e.g.) an `--index-url` in a requirements file.
fn warn_on_requirements_txt_setting(spec: &RequirementsSpecification, settings: &ResolverSettings) {
    let RequirementsSpecification {
        index_url,
        extra_index_urls,
        no_index,
        find_links,
        no_binary,
        no_build,
        ..
    } = spec;

    if settings.index_locations.no_index() {
        // Nothing to do, we're ignoring the URLs anyway.
    } else if *no_index {
        warn_user_once!(
            "Ignoring `--no-index` from requirements file. Instead, use the `--no-index` command-line argument, or set `no-index` in a `uv.toml` or `pyproject.toml` file."
        );
    } else {
        if let Some(index_url) = index_url {
            if settings.index_locations.default_index().map(Index::url) != Some(index_url) {
                warn_user_once!(
                    "Ignoring `--index-url` from requirements file: `{index_url}`. Instead, use the `--index-url` command-line argument, or set `index-url` in a `uv.toml` or `pyproject.toml` file."
                );
            }
        }
        for extra_index_url in extra_index_urls {
            if !settings
                .index_locations
                .implicit_indexes()
                .any(|index| index.url() == extra_index_url)
            {
                warn_user_once!(
                    "Ignoring `--extra-index-url` from requirements file: `{extra_index_url}`. Instead, use the `--extra-index-url` command-line argument, or set `extra-index-url` in a `uv.toml` or `pyproject.toml` file.`"
                );
            }
        }
        for find_link in find_links {
            if !settings
                .index_locations
                .flat_indexes()
                .any(|index| index.url() == find_link)
            {
                warn_user_once!(
                    "Ignoring `--find-links` from requirements file: `{find_link}`. Instead, use the `--find-links` command-line argument, or set `find-links` in a `uv.toml` or `pyproject.toml` file.`"
                );
            }
        }
    }

    if !no_binary.is_none() && settings.build_options.no_binary() != no_binary {
        warn_user_once!(
            "Ignoring `--no-binary` setting from requirements file. Instead, use the `--no-binary` command-line argument, or set `no-binary` in a `uv.toml` or `pyproject.toml` file."
        );
    }

    if !no_build.is_none() && settings.build_options.no_build() != no_build {
        warn_user_once!(
            "Ignoring `--no-binary` setting from requirements file. Instead, use the `--no-build` command-line argument, or set `no-build` in a `uv.toml` or `pyproject.toml` file."
        );
    }
}

/// Normalize a filename for use in a cache entry.
///
/// Replaces non-alphanumeric characters with dashes, and lowercases the filename.
fn cache_name(name: &str) -> Option<Cow<'_, str>> {
    if name.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
        return if name.is_empty() {
            None
        } else {
            Some(Cow::Borrowed(name))
        };
    }
    let mut normalized = String::with_capacity(name.len());
    let mut dash = false;
    for char in name.bytes() {
        match char {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' => {
                dash = false;
                normalized.push(char.to_ascii_lowercase() as char);
            }
            _ => {
                if !dash {
                    normalized.push('-');
                    dash = true;
                }
            }
        }
    }
    if normalized.ends_with('-') {
        normalized.pop();
    }
    if normalized.is_empty() {
        None
    } else {
        Some(Cow::Owned(normalized))
    }
}

fn format_requires_python_sources(conflicts: &RequiresPythonSources) -> String {
    conflicts
        .iter()
        .map(|((package, group), specifiers)| {
            if let Some(group) = group {
                format!("- {package}:{group}: {specifiers}")
            } else {
                format!("- {package}: {specifiers}")
            }
        })
        .join("\n")
}

fn format_optional_requires_python_sources(
    conflicts: &RequiresPythonSources,
    workspace_non_trivial: bool,
) -> String {
    // If there's lots of conflicts, print a list
    if conflicts.len() > 1 {
        return format!(
            ".\nThe following `requires-python` declarations do not permit this version:\n{}",
            format_requires_python_sources(conflicts)
        );
    }
    // If there's one conflict, give a clean message
    if conflicts.len() == 1 {
        let ((package, group), _) = conflicts.iter().next().unwrap();
        if let Some(group) = group {
            if workspace_non_trivial {
                return format!(
                    " (from workspace member `{package}`'s `tool.uv.dependency-groups.{group}.requires-python`)."
                );
            }
            return format!(" (from `tool.uv.dependency-groups.{group}.requires-python`).");
        }
        if workspace_non_trivial {
            return format!(" (from workspace member `{package}`'s `project.requires-python`).");
        }
        return " (from `project.requires-python`)".to_owned();
    }
    // Otherwise don't elaborate
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_name() {
        assert_eq!(cache_name("foo"), Some("foo".into()));
        assert_eq!(cache_name("foo-bar"), Some("foo-bar".into()));
        assert_eq!(cache_name("foo_bar"), Some("foo-bar".into()));
        assert_eq!(cache_name("foo-bar_baz"), Some("foo-bar-baz".into()));
        assert_eq!(cache_name("foo-bar_baz_"), Some("foo-bar-baz".into()));
        assert_eq!(cache_name("foo-_bar_baz"), Some("foo-bar-baz".into()));
        assert_eq!(cache_name("_+-_"), None);
    }
}
