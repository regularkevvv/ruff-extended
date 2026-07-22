use crate::Db;
use crate::glob::{ExcludeFilter, IncludeExcludeFilter, IncludeFilter, PortableGlobKind};
use crate::metadata::python_version::SupportedPythonVersion;
use crate::metadata::settings::{
    OverrideSettings, PluginEntrySettings, PluginEnvironmentFingerprint, PluginRuntimeSettings,
    PluginSettings, SrcSettings,
};

use super::settings::{Override, Settings, TerminalSettings};
use crate::metadata::value::{RelativeGlobPattern, RelativePathBuf};
use anyhow::Context;
use ordermap::OrderMap;
use ruff_cache::{CacheKey, CacheKeyHasher};
use ruff_db::RustDoc;
use ruff_db::diagnostic::{
    Annotation, Diagnostic, DiagnosticFormat, DiagnosticId, DisplayDiagnosticConfig, Severity,
    Span, SubDiagnostic, SubDiagnosticSeverity,
};
use ruff_db::files::system_path_to_file;
use ruff_db::system::{System, SystemPath, SystemPathBuf};
use ruff_db::vendored::VendoredFileSystem;
use ruff_macros::{Combine, OptionsMetadata, RustDoc};
use ruff_options_metadata::{OptionSet, OptionsMetadata, Visit};
use ruff_python_ast::PythonVersion;
use ruff_ranged_value::{RangedValue, ValueSource, ValueSourceGuard};
use rustc_hash::FxHasher;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::{self, Debug, Display};
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::Deref;
use std::sync::Arc;
use strum::IntoEnumIterator;
use thiserror::Error;
use ty_combine::Combine;
use ty_module_resolver::{
    ModuleGlobSet, ModuleGlobSetBuilder, SearchPathSettings, SearchPathSettingsError, SearchPaths,
};
use ty_plugin_host::{HostError, PluginEnvironment};
use ty_plugin_protocol::{
    AttributeClaimKind, AttributeScope, ClassClaimKind, MethodClaimKind, PluginManifest,
    RuntimeSpec,
};
use ty_python_core::platform::PythonPlatform;
use ty_python_core::program::{
    MisconfigurationStrategy, ProgramSettings, SemanticPlugin, SemanticPluginEnvironment,
    SemanticPluginMemberClaim, SemanticPluginMethodClaim, SemanticPluginRuntime,
};
use ty_python_semantic::lint::{Level, LintSource, RuleSelection};
use ty_python_semantic::{
    AnalysisSettings, PythonEnvironment, PythonVersionFileSource, PythonVersionSource,
    PythonVersionWithSource, SitePackagesPaths, SysPrefixPathOrigin,
    inferred_python_version_source_annotation,
};
use ty_static::EnvVars;

#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Eq,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Options {
    /// Configures the type checking environment.
    #[option_group]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentOptions>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub src: Option<SrcOptions>,

    /// Configures the enabled rules and their severity.
    ///
    /// The keys are either rule names or `all` to set a default severity for all rules.
    /// See [the rules documentation](https://ty.dev/rules) for a list of all available rules.
    ///
    /// Valid severities are:
    ///
    /// * `ignore`: Disable the rule.
    /// * `warn`: Enable the rule and create a warning diagnostic.
    /// * `error`: Enable the rule and create an error diagnostic.
    ///
    /// By default, ty exits with code 1 if it emits any warning or error diagnostics.
    /// Set `terminal.error-on-warning` to `false` to exit with code 0 if all diagnostics have `warning` severity.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"{...}"#,
        value_type = r#"dict[RuleName | "all", "ignore" | "warn" | "error"]"#,
        example = r#"
            [tool.ty.rules]
            possibly-unresolved-reference = "warn"
            division-by-zero = "ignore"
        "#
    )]
    pub rules: Option<Rules>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub terminal: Option<TerminalOptions>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub analysis: Option<AnalysisOptions>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub plugins: Option<PluginsOptions>,

    /// Override configurations for specific file patterns.
    ///
    /// Each override specifies include/exclude patterns and rule configurations
    /// that apply to matching files. Multiple overrides can match the same file,
    /// with later overrides taking precedence.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub overrides: Option<OverridesOptions>,
}

impl Options {
    pub fn from_toml_str(content: &str, source: ValueSource) -> Result<Self, TyTomlError> {
        let _guard = ValueSourceGuard::new(source, true);
        let mut options: Self = toml::from_str(content)?;
        options.prioritize_all_selectors();
        Ok(options)
    }

    /// Ensures that the `all` selector is applied before per-rule selectors
    /// in all rule tables (top-level and overrides).
    ///
    /// This must be called after deserializing from TOML and before any
    /// [`Combine::combine`] calls, because TOML tables are unordered and the
    /// `toml` crate sorts keys lexicographically.
    pub(crate) fn prioritize_all_selectors(&mut self) {
        // Stable sort that moves all `all` selectors before non-`all` selectors
        // while preserving relative order among non-`all` entries.
        let sort = |rules: &mut Rules| {
            rules.inner.sort_by(
                |key_a, _, key_b, _| match (**key_a == "all", **key_b == "all") {
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    _ => Ordering::Equal,
                },
            );
        };

        if let Some(rules) = &mut self.rules {
            sort(rules);
        }
        if let Some(overrides) = &mut self.overrides {
            for override_option in &mut overrides.0 {
                if let Some(rules) = &mut override_option.rules {
                    sort(rules);
                }
            }
        }
    }

    pub fn deserialize_with<'de, D>(source: ValueSource, deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let _guard = ValueSourceGuard::new(source, false);
        Self::deserialize(deserializer)
    }

    pub(crate) fn to_program_settings<Strategy: MisconfigurationStrategy>(
        &self,
        project_root: &SystemPath,
        project_name: &str,
        system: &dyn System,
        vendored: &VendoredFileSystem,
        strategy: &Strategy,
    ) -> Result<(ProgramSettings, Vec<ProgramSettingsDiagnostic>), Strategy::Error<anyhow::Error>>
    {
        let mut diagnostics = Vec::new();
        let environment = self.environment.or_default();

        let configured_python_version = environment
            .python_version
            .as_ref()
            .map(python_version_from_config);
        let python_platform = environment
            .python_platform
            .as_deref()
            .cloned()
            .unwrap_or_else(|| {
                let default = PythonPlatform::default();
                tracing::info!("Defaulting to python-platform `{default}`");
                default
            });

        let python_environment = if let Some(python_path) = environment.python.as_ref() {
            let origin = match python_path.source() {
                ValueSource::Cli => SysPrefixPathOrigin::PythonCliFlag,
                ValueSource::File(path) => {
                    SysPrefixPathOrigin::ConfigFileSetting(path.clone(), python_path.range())
                }
                ValueSource::Editor => SysPrefixPathOrigin::Editor,
            };

            PythonEnvironment::new(python_path.absolute(project_root, system), origin, system)
                .map_err(anyhow::Error::from)
                .map(Some)
        } else {
            PythonEnvironment::discover(project_root, system)
                .context("Failed to discover local Python environment")
        };

        // If in safe-mode, fallback to None if this fails instead of erroring.
        let python_environment = strategy
            .fallback_opt(python_environment, |_| {
                tracing::debug!("Default settings failed to discover local Python environment");
            })?
            .flatten();

        let self_environment = self_environment_search_paths(
            python_environment
                .as_ref()
                .map(ty_python_semantic::PythonEnvironment::origin)
                .cloned(),
            system,
        );

        let site_packages_paths = if let Some(python_environment) = python_environment.as_ref() {
            let site_packages_paths = python_environment
                .site_packages_paths(system)
                .context("Failed to discover the site-packages directory");
            let site_packages_paths = strategy.fallback(site_packages_paths, |_| {
                tracing::debug!("Default settings failed to discover site-packages directory");
                SitePackagesPaths::default()
            })?;
            match self_environment {
                // When ty is installed in a virtual environment (e.g., `uvx --with ...`),
                // the self-environment takes priority over the discovered environment.
                Some((self_site_packages, true)) => {
                    self_site_packages.concatenate(site_packages_paths)
                }
                // When ty is installed in a system Python, do not include the system
                // Python's site-packages if there's a discovered project environment.
                Some((_, false)) | None => site_packages_paths,
            }
        } else {
            tracing::debug!("No virtual environment found");
            self_environment.map(|(paths, _)| paths).unwrap_or_default()
        };

        let real_stdlib_path = python_environment.as_ref().and_then(|python_environment| {
            // For now this is considered non-fatal, we don't Need this for anything.
            python_environment.real_stdlib_path(system).map_err(|err| {
                tracing::info!("No real stdlib found, stdlib goto-definition may have degraded quality: {err}");
            }).ok()
        });

        let python_version = configured_python_version
            .map(PythonVersionResolution::Configured)
            .or_else(|| {
                let inferred_python_version = python_environment
                    .as_ref()
                    .and_then(|python_environment| {
                        python_environment.python_version_from_metadata()
                    })
                    .cloned()
                    .or_else(|| site_packages_paths.python_version_from_layout());

                inferred_python_version.map(PythonVersionResolution::Inferred)
            })
            .and_then(|resolution| resolution.into_program_version(&mut diagnostics))
            .unwrap_or_default();

        let plugin_site_packages = site_packages_paths.clone().into_vec();

        // Safe mode is handled inside this function, so we just assume this can't fail
        let search_paths = strategy.to_anyhow(self.to_search_paths(
            project_root,
            project_name,
            site_packages_paths,
            real_stdlib_path,
            system,
            vendored,
            strategy,
        ))?;

        tracing::info!(
            "Python version: Python {python_version}, platform: {python_platform}",
            python_version = python_version.version
        );

        let plugins = self.plugins.or_default();
        let semantic_plugins = plugins.semantic_environment_for_program_settings(
            project_root,
            system,
            &plugin_site_packages,
        );

        Ok((
            ProgramSettings {
                python_version,
                python_platform,
                search_paths,
                semantic_plugins,
            },
            diagnostics,
        ))
    }

    #[expect(clippy::too_many_arguments)]
    fn to_search_paths<Strategy: MisconfigurationStrategy>(
        &self,
        project_root: &SystemPath,
        project_name: &str,
        site_packages_paths: SitePackagesPaths,
        real_stdlib_path: Option<SystemPathBuf>,
        system: &dyn System,
        vendored: &VendoredFileSystem,
        strategy: &Strategy,
    ) -> Result<SearchPaths, Strategy::Error<SearchPathSettingsError>> {
        let environment = self.environment.or_default();
        let src = self.src.or_default();

        #[allow(deprecated)]
        let src_roots = if let Some(roots) = environment
            .root
            .as_deref()
            .or_else(|| Some(std::slice::from_ref(src.root.as_ref()?)))
        {
            roots
                .iter()
                .map(|root| root.absolute(project_root, system))
                .collect()
        } else {
            let mut roots = vec![];
            let is_package = |dir: &SystemPath| {
                system.is_file(&dir.join("__init__.py"))
                    || system.is_file(&dir.join("__init__.pyi"))
            };

            // Check for `./src` directory (src-layout)
            let src = project_root.join("src");
            if system.is_directory(&src) && !is_package(&src) {
                tracing::debug!(
                    "Including `./src` in `environment.root` because a `./src` directory exists and is not a package"
                );
                roots.push(src);
            }

            // Check for `./<project-name>/<project-name>` directory (src-layout with project-named folder)
            // For example, the "src" folder for `psycopg` is called `psycopg` and the python files are in `psycopg/psycopg/_adapters_map.py`
            let project_name_dir = project_root.join(project_name);
            if system.is_directory(&project_name_dir.join(project_name))
                && !is_package(&project_name_dir)
                && !roots.contains(&project_name_dir)
            {
                tracing::debug!(
                    "Including `./{project_name}` in `environment.root` because a `./{project_name}/{project_name}` directory exists and `./{project_name}` is not a package"
                );
                roots.push(project_name_dir);
            }

            // Check for `./python` directory (maturin-based rust/python projects)
            // https://github.com/PyO3/maturin/blob/979fe1db42bb9e58bc150fa6fc45360b377288bf/README.md?plain=1#L88-L99
            let python = project_root.join("python");
            if system.is_directory(&python) && !is_package(&python) && !roots.contains(&python) {
                tracing::debug!(
                    "Including `./python` in `environment.root` because a `./python` directory exists and is not a package"
                );
                roots.push(python);
            }

            // The project root is always included, and should always come last
            // (after any subdirectories such as `./src`, `./<project-name>`, and/or `./python`).
            roots.push(project_root.to_path_buf());

            roots
        };

        // collect the existing site packages
        let mut extra_paths: Vec<SystemPathBuf> = environment
            .extra_paths
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|path| path.absolute(project_root, system))
            .collect();

        // read all the paths off the PYTHONPATH environment variable, check
        // they exist as a directory, and add them to the vec of extra_paths
        // as they should be checked before site-packages just like python
        // interpreter does
        if let Ok(python_path) = system.env_var(EnvVars::PYTHONPATH) {
            for path in std::env::split_paths(python_path.as_str()) {
                let path = match SystemPathBuf::from_path_buf(path) {
                    Ok(path) => path,
                    Err(path) => {
                        tracing::debug!(
                            "Skipping `{path}` listed in `PYTHONPATH` because the path is not valid UTF-8",
                            path = path.display()
                        );
                        continue;
                    }
                };

                let abspath = SystemPath::absolute(path, system.current_directory());

                if !system.is_directory(&abspath) {
                    tracing::debug!(
                        "Skipping `{abspath}` listed in `PYTHONPATH` because the path doesn't exist or isn't a directory"
                    );
                    continue;
                }

                tracing::debug!(
                    "Adding `{abspath}` from the `PYTHONPATH` environment variable to `extra_paths`"
                );

                extra_paths.push(abspath);
            }
        }

        let plugin_site_packages = site_packages_paths.clone().into_vec();
        let plugin_stub_overlay_paths = self
            .plugins
            .or_default()
            .active_stub_overlay_paths_for_program_settings(
                project_root,
                system,
                &plugin_site_packages,
            );

        let settings = SearchPathSettings {
            extra_paths,
            src_roots,
            plugin_stub_overlay_paths,
            custom_typeshed: environment
                .typeshed
                .as_ref()
                .map(|path| path.absolute(project_root, system)),
            site_packages_paths: site_packages_paths.into_vec(),
            real_stdlib_path,
        };

        settings.to_search_paths(system, vendored, strategy)
    }

    pub(crate) fn to_settings<Strategy: MisconfigurationStrategy>(
        &self,
        db: &dyn Db,
        project_root: &SystemPath,
        strategy: &Strategy,
    ) -> Result<(Settings, Vec<OptionDiagnostic>), Strategy::Error<ToSettingsError>> {
        let mut diagnostics = Vec::new();
        let rules = self.to_rule_selection(db, &mut diagnostics);

        let terminal_options = self.terminal.or_default();
        let terminal = TerminalSettings {
            output_format: terminal_options
                .output_format
                .as_deref()
                .copied()
                .unwrap_or_default(),
            error_on_warning: terminal_options.error_on_warning.unwrap_or(true),
        };

        let src_options = self.src.or_default();

        #[allow(deprecated)]
        if let Some(src_root) = src_options.root.as_ref() {
            let mut diagnostic = OptionDiagnostic::new(
                DiagnosticId::DeprecatedSetting,
                "The `src.root` setting is deprecated. Use `environment.root` instead.".to_string(),
                Severity::Warning,
            );

            if let Some(file) = src_root
                .source()
                .file()
                .and_then(|path| system_path_to_file(db, path).ok())
            {
                diagnostic = diagnostic.with_annotation(Some(Annotation::primary(
                    Span::from(file).with_optional_range(src_root.range()),
                )));
            }

            if self.environment.or_default().root.is_some() {
                diagnostic = diagnostic.sub(SubDiagnostic::new(
                    SubDiagnosticSeverity::Info,
                    "The `src.root` setting was ignored in favor of the `environment.root` setting",
                ));
            }

            diagnostics.push(diagnostic);
        }

        let src = src_options
            .to_settings(db, project_root, &mut diagnostics)
            .map_err(|err| ToSettingsError {
                diagnostic: err,
                output_format: terminal.output_format,
                color: colored::control::SHOULD_COLORIZE.should_colorize(),
            });
        let src = strategy.fallback(src, |_| SrcSettings::default())?;

        let mut analysis_diagnostics = Vec::new();
        let analysis = self
            .analysis
            .or_default()
            .to_settings(db, &mut analysis_diagnostics);

        let analysis_result: Result<_, ToSettingsError> =
            if let Some(diagnostic) = analysis_diagnostics.into_iter().next() {
                Err(ToSettingsError {
                    diagnostic: Box::new(diagnostic),
                    output_format: terminal.output_format,
                    color: colored::control::SHOULD_COLORIZE.should_colorize(),
                })
            } else {
                Ok(analysis)
            };
        let analysis = strategy.fallback(analysis_result, |_| AnalysisSettings::default())?;

        let plugin_site_packages = self.plugin_site_packages(project_root, db.system());
        let plugins = self.plugins.or_default().to_settings(
            db,
            project_root,
            db.system(),
            &plugin_site_packages,
            &mut diagnostics,
        );

        let overrides = self
            .to_overrides_settings(db, project_root, &mut diagnostics)
            .map_err(|err| ToSettingsError {
                diagnostic: err,
                output_format: terminal.output_format,
                color: colored::control::SHOULD_COLORIZE.should_colorize(),
            });
        let overrides = strategy.fallback(overrides, |_| Vec::new())?;

        let settings = Settings {
            rules: Arc::new(rules),
            terminal,
            src,
            analysis,
            plugins,
            overrides,
        };

        Ok((settings, diagnostics))
    }

    fn plugin_site_packages(
        &self,
        project_root: &SystemPath,
        system: &dyn System,
    ) -> Vec<SystemPathBuf> {
        let environment = self.environment.or_default();
        let python_environment = if let Some(python_path) = environment.python.as_ref() {
            let origin = match python_path.source() {
                ValueSource::Cli => SysPrefixPathOrigin::PythonCliFlag,
                ValueSource::File(path) => {
                    SysPrefixPathOrigin::ConfigFileSetting(path.clone(), python_path.range())
                }
                ValueSource::Editor => SysPrefixPathOrigin::Editor,
            };
            PythonEnvironment::new(python_path.absolute(project_root, system), origin, system).ok()
        } else {
            PythonEnvironment::discover(project_root, system)
                .ok()
                .flatten()
        };

        python_environment
            .and_then(|environment| environment.site_packages_paths(system).ok())
            .map(SitePackagesPaths::into_vec)
            .unwrap_or_default()
    }

    #[must_use]
    fn to_rule_selection(
        &self,
        db: &dyn Db,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> RuleSelection {
        self.rules.or_default().to_rule_selection(db, diagnostics)
    }

    fn to_overrides_settings(
        &self,
        db: &dyn Db,
        project_root: &SystemPath,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> Result<Vec<Override>, Box<OptionDiagnostic>> {
        let override_options = &**self.overrides.or_default();

        let mut overrides = Vec::with_capacity(override_options.len());

        for override_option in override_options {
            let override_instance = override_option.to_override(
                db,
                project_root,
                self.rules.as_ref(),
                self.analysis.as_ref(),
                diagnostics,
            )?;

            if let Some(value) = override_instance {
                overrides.push(value);
            }
        }

        Ok(overrides)
    }
}

fn python_version_from_config(
    ranged_version: &RangedValue<SupportedPythonVersion>,
) -> PythonVersionWithSource {
    PythonVersionWithSource {
        version: PythonVersion::from(**ranged_version),
        source: match ranged_version.source() {
            ValueSource::Cli => PythonVersionSource::Cli,
            ValueSource::File(path) => PythonVersionSource::ConfigFile(
                PythonVersionFileSource::new(path.clone(), ranged_version.range()),
            ),
            ValueSource::Editor => PythonVersionSource::Editor,
        },
    }
}

/// A Python version before unsupported inferred versions are filtered.
#[derive(Eq, PartialEq, Debug, Clone)]
enum PythonVersionResolution {
    /// The Python version was configured directly by the user.
    Configured(PythonVersionWithSource),
    /// The Python version was inferred from the environment.
    Inferred(PythonVersionWithSource),
}

impl PythonVersionResolution {
    fn into_program_version(
        self,
        diagnostics: &mut Vec<ProgramSettingsDiagnostic>,
    ) -> Option<PythonVersionWithSource> {
        match self {
            Self::Configured(python_version) => Some(python_version),
            Self::Inferred(python_version) => {
                if SupportedPythonVersion::try_from(python_version.version).is_ok() {
                    Some(python_version)
                } else {
                    diagnostics.push(ProgramSettingsDiagnostic::UnsupportedInferredPythonVersion(
                        python_version,
                    ));
                    None
                }
            }
        }
    }
}

/// A diagnostic produced while resolving [`ProgramSettings`].
///
/// These diagnostics are kept separate from [`OptionDiagnostic`] while program settings are
/// resolved so that this step does not need access to the database.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ProgramSettingsDiagnostic {
    /// The Python version inferred from the environment is newer than ty supports.
    UnsupportedInferredPythonVersion(PythonVersionWithSource),
}

impl ProgramSettingsDiagnostic {
    /// Convert this program-settings diagnostic into a diagnostic that can be stored on a project.
    pub(crate) fn into_diagnostic(self, db: &dyn Db) -> OptionDiagnostic {
        match self {
            Self::UnsupportedInferredPythonVersion(python_version) => {
                unsupported_inferred_python_version_diagnostic(db, &python_version)
            }
        }
    }
}

/// Construct an [`OptionDiagnostic`] to indicate that the inferred Python version is unsupported.
fn unsupported_inferred_python_version_diagnostic(
    db: &dyn Db,
    python_version: &PythonVersionWithSource,
) -> OptionDiagnostic {
    let expected = SupportedPythonVersion::iter()
        .map(|version| format!("`{version}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let fallback = PythonVersion::latest_ty();

    let mut diagnostic = OptionDiagnostic::new(
        DiagnosticId::UnsupportedPythonVersion,
        format!(
            "Ignoring unsupported inferred Python version `{}`; ty will use Python {fallback} instead.",
            python_version.version
        ),
        Severity::Warning,
    )
    .sub(SubDiagnostic::new(
        SubDiagnosticSeverity::Info,
        format!("Expected one of {expected}."),
    ))
    .sub(SubDiagnostic::new(
        SubDiagnosticSeverity::Info,
        "Set `environment.python-version` explicitly to override the inferred version.",
    ));

    diagnostic = match &python_version.source {
        source @ PythonVersionSource::ConfigFile(_) => diagnostic
            .with_annotation(inferred_python_version_source_annotation(db, source))
            .sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "The version was inferred from a configuration file.",
            )),
        source @ PythonVersionSource::PyvenvCfgFile(_) => diagnostic
            .with_annotation(inferred_python_version_source_annotation(db, source))
            .sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "The version was inferred from your virtual environment metadata.",
            )),
        PythonVersionSource::InstallationDirectoryLayout {
            site_packages_parent_dir,
            source,
        } => diagnostic
            .with_annotation(inferred_python_version_source_annotation(
                db,
                &PythonVersionSource::InstallationDirectoryLayout {
                    site_packages_parent_dir: site_packages_parent_dir.clone(),
                    source: source.clone(),
                },
            ))
            .sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                format!(
                    "The version was inferred from the `lib/{site_packages_parent_dir}/site-packages` directory layout.",
                ),
            )),
        PythonVersionSource::Cli => diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "The version was inferred from the command line.",
        )),
        PythonVersionSource::Editor => diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "The version was inferred from your editor.",
        )),
        PythonVersionSource::Default => diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "ty fell back to its default Python version.",
        )),
    };

    diagnostic
}

/// Return the site-packages from the environment ty is installed in, as derived from ty's
/// executable.
///
/// If there's an existing environment with an origin that does not allow including site-packages
/// from ty's environment, discovery of ty's environment is skipped and [`None`] is returned.
///
/// Since ty may be executed from an arbitrary non-Python location, errors during discovery of ty's
/// environment are not raised, instead [`None`] is returned.
///
/// Returns a tuple of (`site_packages`, `is_virtual_env`). When the self-environment is a virtual
/// environment (e.g., `uvx --with ...`), it takes priority over other environments.
/// When it's a system Python and there's a project environment (like `.venv`), the system
/// Python's site-packages are excluded entirely.
fn self_environment_search_paths(
    existing_origin: Option<SysPrefixPathOrigin>,
    system: &dyn System,
) -> Option<(SitePackagesPaths, bool)> {
    if existing_origin.is_some_and(|origin| !origin.allows_concatenation_with_self_environment()) {
        return None;
    }

    let Ok(exe_path) = std::env::current_exe() else {
        return None;
    };
    let ty_path = SystemPath::from_std_path(exe_path.as_path())?;

    let environment = PythonEnvironment::new(ty_path, SysPrefixPathOrigin::SelfEnvironment, system)
        .inspect_err(|err| tracing::debug!("Failed to discover ty's environment: {err}"))
        .ok()?;

    let is_virtual_env = environment.is_virtual();

    let search_paths = environment
        .site_packages_paths(system)
        .inspect_err(|err| {
            tracing::debug!("Failed to discover site-packages in ty's environment: {err}");
        })
        .ok()?;

    tracing::debug!("Using site-packages from ty's environment");
    Some((search_paths, is_virtual_env))
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct EnvironmentOptions {
    /// The root paths of the project, used for finding first-party modules.
    ///
    /// Accepts a list of directory paths searched in priority order (first has highest priority).
    ///
    /// If left unspecified, ty will try to detect common project layouts and initialize `root` accordingly.
    /// The project root (`.`) is always included. Additionally, the following directories are included
    /// if they exist and are not packages (i.e. they do not contain `__init__.py` or `__init__.pyi` files):
    ///
    /// * `./src`
    /// * `./<project-name>` (if a `./<project-name>/<project-name>` directory exists)
    /// * `./python`
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "list[str]",
        example = r#"
            # Multiple directories (priority order)
            root = ["./src", "./lib", "./vendor"]
        "#
    )]
    pub root: Option<Vec<RelativePathBuf>>,

    /// Specifies the version of Python that will be used to analyze the source code.
    /// The version should be specified as a string in the format `M.m` where `M` is the major version
    /// and `m` is the minor (e.g. `"3.7"` or `"3.12"`).
    /// If a version is provided, ty will generate errors if the source code makes use of language features
    /// that are not supported in that version.
    ///
    /// ty officially supports type checking code that targets Python 3.10 and later. Python 3.7
    /// through 3.9 can still be selected, but ty may produce false positives or false negatives for
    /// standard-library APIs because its bundled stubs do not fully describe those versions.
    ///
    /// If a version is not specified, ty will try the following techniques in order of preference
    /// to determine a value:
    /// 1. Check for the `project.requires-python` setting in a `pyproject.toml` file
    ///    and use the minimum version from the specified range
    /// 2. Check for an activated or configured Python environment
    ///    and attempt to infer the Python version of that environment
    /// 3. Fall back to the default value (see below)
    ///
    /// For some language features, ty can also understand conditionals based on comparisons
    /// with `sys.version_info`. These are commonly found in typeshed, for example,
    /// to reflect the differing contents of the standard library across Python versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#""3.14""#,
        value_type = r#""3.7" | "3.8" | "3.9" | "3.10" | "3.11" | "3.12" | "3.13" | "3.14" | "3.15""#,
        example = r#"
            python-version = "3.12"
        "#
    )]
    pub python_version: Option<RangedValue<SupportedPythonVersion>>,

    /// Specifies the target platform that will be used to analyze the source code.
    /// If specified, ty will understand conditions based on comparisons with `sys.platform`, such
    /// as are commonly found in typeshed to reflect the differing contents of the standard library across platforms.
    /// If `all` is specified, ty will assume that the source code can run on any platform.
    ///
    /// If no platform is specified, ty will use the current platform:
    /// - `win32` for Windows
    /// - `darwin` for macOS
    /// - `android` for Android
    /// - `ios` for iOS
    /// - `linux` for everything else
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"<current-platform>"#,
        value_type = r#""win32" | "darwin" | "android" | "ios" | "linux" | "all" | str"#,
        example = r#"
        # Tailor type stubs and conditionalized type definitions to windows.
        python-platform = "win32"
        "#
    )]
    pub python_platform: Option<RangedValue<PythonPlatform>>,

    /// User-provided paths that should take first priority in module resolution.
    ///
    /// This is an advanced option that should usually only be used for first-party or third-party
    /// modules that are not installed into your Python environment in a conventional way.
    /// Use the `python` option to specify the location of your Python environment.
    ///
    /// This option is similar to mypy's `MYPYPATH` environment variable and pyright's `stubPath`
    /// configuration setting.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"[]"#,
        value_type = "list[str]",
        example = r#"
            extra-paths = ["./shared/my-search-path"]
        "#
    )]
    pub extra_paths: Option<Vec<RelativePathBuf>>,

    /// Optional path to a "typeshed" directory on disk for us to use for standard-library types.
    /// If this is not provided, we will fallback to our vendored typeshed stubs for the stdlib,
    /// bundled as a zip file in the binary
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "str",
        example = r#"
            typeshed = "/path/to/custom/typeshed"
        "#
    )]
    pub typeshed: Option<RelativePathBuf>,

    /// Path to your project's Python environment or interpreter.
    ///
    /// ty uses the `site-packages` directory of your project's Python environment
    /// to resolve third-party (and, in some cases, first-party) imports in your code.
    ///
    /// This can be a path to:
    ///
    /// - A Python interpreter, e.g. `.venv/bin/python3`
    /// - A virtual environment directory, e.g. `.venv`
    /// - A system Python [`sys.prefix`] directory, e.g. `/usr`
    ///
    /// If you're using a project management tool such as uv, you should not generally need to
    /// specify this option, as commands such as `uv run` will set the `VIRTUAL_ENV` environment
    /// variable to point to your project's virtual environment. ty can also infer the location of
    /// your environment from an activated Conda environment, and will look for a `.venv` directory
    /// in the project root if none of the above apply. Failing that, ty will look for a `python3`
    /// or `python` binary available in `PATH`.
    ///
    /// [`sys.prefix`]: https://docs.python.org/3/library/sys.html#sys.prefix
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "str",
        example = r#"
            python = "./custom-venv-location/.venv"
        "#
    )]
    pub python: Option<RelativePathBuf>,
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct SrcOptions {
    /// The root of the project, used for finding first-party modules.
    ///
    /// If left unspecified, ty will try to detect common project layouts and initialize `src.root` accordingly.
    /// The project root (`.`) is always included. Additionally, the following directories are included
    /// if they exist and are not packages (i.e. they do not contain `__init__.py` or `__init__.pyi` files):
    ///
    /// * `./src`
    /// * `./<project-name>` (if a `./<project-name>/<project-name>` directory exists)
    /// * `./python`
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "str",
        example = r#"
            root = "./app"
        "#
    )]
    #[deprecated(note = "Use `environment.root` instead.")]
    pub root: Option<RelativePathBuf>,

    /// Whether to automatically exclude files that are ignored by `.ignore`,
    /// `.gitignore`, `.git/info/exclude`, and global `gitignore` files.
    /// Enabled by default.
    #[option(
        default = r#"true"#,
        value_type = r#"bool"#,
        example = r#"
            respect-ignore-files = false
        "#
    )]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub respect_ignore_files: Option<bool>,

    /// A list of files and directories to check. The `include` option
    /// follows a similar syntax to `.gitignore` but reversed:
    /// Including a file or directory will make it so that it (and its contents)
    /// are type checked.
    ///
    /// - `./src/` matches only a directory
    /// - `./src` matches both files and directories
    /// - `src` matches a file or directory named `src`
    /// - `*` matches any (possibly empty) sequence of characters (except `/`).
    /// - `**` matches zero or more path components.
    ///   This sequence **must** form a single path component, so both `**a` and `b**` are invalid and will result in an error.
    ///   A sequence of more than two consecutive `*` characters is also invalid.
    /// - `?` matches any single character except `/`
    /// - `[abc]` matches any character inside the brackets. Character sequences can also specify ranges of characters, as ordered by Unicode,
    ///   so e.g. `[0-9]` specifies any character between `0` and `9` inclusive. An unclosed bracket is invalid.
    ///
    /// All paths are anchored relative to the project root (`src` only
    /// matches `<project_root>/src` and not `<project_root>/test/src`).
    ///
    /// `exclude` takes precedence over `include`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = r#"list[str]"#,
        example = r#"
            include = [
                "src",
                "tests",
            ]
        "#
    )]
    pub include: Option<RangedValue<Vec<RelativeGlobPattern>>>,

    /// A list of file and directory patterns to exclude from type checking.
    ///
    /// Patterns follow a syntax similar to `.gitignore`:
    ///
    /// - `./src/` matches only a directory
    /// - `./src` matches both files and directories
    /// - `src` matches files or directories named `src`
    /// - `*` matches any (possibly empty) sequence of characters (except `/`).
    /// - `**` matches zero or more path components.
    ///   This sequence **must** form a single path component, so both `**a` and `b**` are invalid and will result in an error.
    ///   A sequence of more than two consecutive `*` characters is also invalid.
    /// - `?` matches any single character except `/`
    /// - `[abc]` matches any character inside the brackets. Character sequences can also specify ranges of characters, as ordered by Unicode,
    ///   so e.g. `[0-9]` specifies any character between `0` and `9` inclusive. An unclosed bracket is invalid.
    /// - `!pattern` negates a pattern (undoes the exclusion of files that would otherwise be excluded)
    ///
    /// All paths are anchored relative to the project root (`src` only
    /// matches `<project_root>/src` and not `<project_root>/test/src`).
    /// To exclude any directory or file named `src`, use `**/src` instead.
    ///
    /// By default, ty excludes commonly ignored directories:
    ///
    /// - `**/.bzr/`
    /// - `**/.direnv/`
    /// - `**/.eggs/`
    /// - `**/.git/`
    /// - `**/.git-rewrite/`
    /// - `**/.hg/`
    /// - `**/.mypy_cache/`
    /// - `**/.nox/`
    /// - `**/.pants.d/`
    /// - `**/.pytype/`
    /// - `**/.ruff_cache/`
    /// - `**/.svn/`
    /// - `**/.tox/`
    /// - `**/.venv/`
    /// - `**/__pypackages__/`
    /// - `**/_build/`
    /// - `**/buck-out/`
    /// - `**/dist/`
    /// - `**/node_modules/`
    /// - `**/venv/`
    ///
    /// You can override any default exclude by using a negated pattern. For example,
    /// to re-include `dist` use `exclude = ["!dist"]`
    #[option(
        default = r#"null"#,
        value_type = r#"list[str]"#,
        example = r#"
            exclude = [
                "generated",
                "*.proto",
                "tests/fixtures/**",
                "!tests/fixtures/important.py"  # Include this one file
            ]
        "#
    )]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<RangedValue<Vec<RelativeGlobPattern>>>,
}

impl SrcOptions {
    fn to_settings(
        &self,
        db: &dyn Db,
        project_root: &SystemPath,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> Result<SrcSettings, Box<OptionDiagnostic>> {
        let include = build_include_filter(
            db,
            project_root,
            self.include.as_ref(),
            GlobFilterContext::SrcRoot,
            diagnostics,
        )?;
        let exclude = build_exclude_filter(
            db,
            project_root,
            self.exclude.as_ref(),
            DEFAULT_SRC_EXCLUDES,
            GlobFilterContext::SrcRoot,
        )?;
        let files = IncludeExcludeFilter::new(include, exclude);

        Ok(SrcSettings {
            respect_ignore_files: self.respect_ignore_files.unwrap_or(true),
            files,
        })
    }
}

#[derive(
    Debug, Default, Clone, Eq, PartialEq, Combine, Serialize, Deserialize, Hash, get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", transparent)]
pub struct Rules {
    /// The rules with their severity. Entries coming later in the map take precedence over
    /// earlier entries (e.g. a `all` selector earlier in the hash map will be overridden
    /// by a specific rule selector coming after it but if `all` is the last selector, then it
    /// overrides even specific rule codes).
    inner: OrderMap<RangedValue<String>, RangedValue<Level>, BuildHasherDefault<FxHasher>>,
}

impl FromIterator<(RangedValue<String>, RangedValue<Level>)> for Rules {
    fn from_iter<T: IntoIterator<Item = (RangedValue<String>, RangedValue<Level>)>>(
        iter: T,
    ) -> Self {
        Self {
            inner: iter.into_iter().collect(),
        }
    }
}

impl Rules {
    /// Convert the rules to a `RuleSelection` with diagnostics.
    pub fn to_rule_selection(
        &self,
        db: &dyn Db,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> RuleSelection {
        let registry = db.lint_registry();

        // Initialize the selection with the defaults
        let mut selection = RuleSelection::from_registry(registry);

        for (rule_name, level) in &self.inner {
            let source = rule_name.source();
            let lint_source = match source {
                ValueSource::File(_) => LintSource::File,
                ValueSource::Cli => LintSource::Cli,
                ValueSource::Editor => LintSource::Editor,
            };

            let mut set_lint_level = |lint| {
                if let Ok(severity) = Severity::try_from(**level) {
                    selection.enable(lint, severity, lint_source);
                } else {
                    selection.disable(lint);
                }
            };

            // Handle "all" as a special case - apply the level to all rules
            if rule_name.as_str() == "all" {
                for lint in registry.lints() {
                    set_lint_level(*lint);
                }
                continue;
            }

            match registry.get(rule_name) {
                Ok(lint) => {
                    set_lint_level(lint);
                }
                Err(error) => {
                    // `system_path_to_file` can return `Err` if the file was deleted since the configuration
                    // was read. This should be rare and it should be okay to default to not showing a configuration
                    // file in that case.
                    let file = source
                        .file()
                        .and_then(|path| system_path_to_file(db, path).ok());

                    // TODO: Add a note if the value was configured on the CLI
                    let diagnostic = OptionDiagnostic::new(
                        DiagnosticId::UnknownRule,
                        error.to_string(),
                        Severity::Warning,
                    );

                    let annotation = file.map(Span::from).map(|span| {
                        Annotation::primary(span.with_optional_range(rule_name.range()))
                    });
                    diagnostics.push(diagnostic.with_annotation(annotation));
                }
            }
        }

        selection
    }

    pub(super) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// Default exclude patterns for src options.
pub(crate) const DEFAULT_SRC_EXCLUDES: &[&str] = &[
    "**/.bzr/",
    "**/.direnv/",
    "**/.eggs/",
    "**/.git/",
    "**/.git-rewrite/",
    "**/.hg/",
    "**/.mypy_cache/",
    "**/.nox/",
    "**/.pants.d/",
    "**/.pytype/",
    "**/.ruff_cache/",
    "**/.svn/",
    "**/.tox/",
    "**/.venv/",
    "**/__pypackages__/",
    "**/_build/",
    "**/buck-out/",
    "**/dist/",
    "**/node_modules/",
    "**/venv/",
];

/// Helper function to build an include filter from patterns with proper error handling.
fn build_include_filter(
    db: &dyn Db,
    project_root: &SystemPath,
    include_patterns: Option<&RangedValue<Vec<RelativeGlobPattern>>>,
    context: GlobFilterContext,
    diagnostics: &mut Vec<OptionDiagnostic>,
) -> Result<IncludeFilter, Box<OptionDiagnostic>> {
    use crate::glob::{IncludeFilterBuilder, PortableGlobPattern};

    let system = db.system();
    let mut includes = IncludeFilterBuilder::new();

    if let Some(include_patterns) = include_patterns {
        if include_patterns.is_empty() {
            // An override with an empty include `[]` won't match any files.
            let mut diagnostic = OptionDiagnostic::new(
                DiagnosticId::EmptyInclude,
                "Empty include matches no files".to_string(),
                Severity::Warning,
            )
            .sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "Remove the `include` option to match all files or add a pattern to match specific files",
            ));

            // Add source annotation if we have source information
            if let Some(source_file) = include_patterns.source().file() {
                if let Ok(file) = system_path_to_file(db, source_file) {
                    let annotation = Annotation::primary(
                        Span::from(file).with_optional_range(include_patterns.range()),
                    )
                    .message("This `include` list is empty");
                    diagnostic = diagnostic.with_annotation(Some(annotation));
                }
            }

            diagnostics.push(diagnostic);
        }

        for pattern in include_patterns {
            pattern
                .absolute(project_root, system, PortableGlobKind::Include)
                .and_then(|include| Ok(includes.add(&include)?))
                .map_err(|err| {
                    let diagnostic = OptionDiagnostic::new(
                        DiagnosticId::InvalidGlob,
                        format!("Invalid include pattern `{pattern}`: {err}"),
                        Severity::Error,
                    );

                    diagnostic.with_source_sub(
                        db,
                        pattern.value(),
                        "pattern",
                        context.include_name(),
                        err,
                    )
                })?;
        }
    } else {
        includes
            .add(
                &PortableGlobPattern::parse("**", PortableGlobKind::Include)
                    .unwrap()
                    .into_absolute(""),
            )
            .unwrap();
    }

    includes.build().map_err(|_| {
        let diagnostic = OptionDiagnostic::new(
            DiagnosticId::InvalidGlob,
            format!("The `{}` patterns resulted in a regex that is too large", context.include_name()),
            Severity::Error,
        );
        Box::new(diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "Please open an issue on the ty repository and share the patterns that caused the error.",
        )))
    })
}

/// Helper function to build an exclude filter from patterns with proper error handling.
fn build_exclude_filter(
    db: &dyn Db,
    project_root: &SystemPath,
    exclude_patterns: Option<&RangedValue<Vec<RelativeGlobPattern>>>,
    default_patterns: &[&str],
    context: GlobFilterContext,
) -> Result<ExcludeFilter, Box<OptionDiagnostic>> {
    use crate::glob::{ExcludeFilterBuilder, PortableGlobPattern};

    let system = db.system();
    let mut excludes = ExcludeFilterBuilder::new();

    for pattern in default_patterns {
        PortableGlobPattern::parse(pattern, PortableGlobKind::Exclude)
            .and_then(|exclude| Ok(excludes.add(&exclude.into_absolute(""))?))
            .unwrap_or_else(|err| {
                panic!("Expected default exclude to be valid glob but adding it failed with: {err}")
            });
    }

    // Add user-specified excludes
    if let Some(exclude_patterns) = exclude_patterns {
        for exclude in exclude_patterns {
            exclude
                .absolute(project_root, system, PortableGlobKind::Exclude)
                .and_then(|pattern| Ok(excludes.add(&pattern)?))
                .map_err(|err| {
                    let diagnostic = OptionDiagnostic::new(
                        DiagnosticId::InvalidGlob,
                        format!("Invalid exclude pattern `{exclude}`: {err}"),
                        Severity::Error,
                    );

                    diagnostic.with_source_sub(
                        db,
                        exclude.value(),
                        "pattern",
                        context.exclude_name(),
                        err,
                    )
                })?;
        }
    }

    excludes.build().map_err(|_| {
        let diagnostic = OptionDiagnostic::new(
            DiagnosticId::InvalidGlob,
            format!("The `{}` patterns resulted in a regex that is too large", context.exclude_name()),
            Severity::Error,
        );
        Box::new(diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "Please open an issue on the ty repository and share the patterns that caused the error.",
        )))
    })
}

/// Context for filter operations, used in error messages
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobFilterContext {
    /// Source root configuration context
    SrcRoot,
    /// Override configuration context
    Overrides,
}

impl GlobFilterContext {
    fn include_name(self) -> &'static str {
        match self {
            Self::SrcRoot => "src.include",
            Self::Overrides => "overrides.include",
        }
    }

    fn exclude_name(self) -> &'static str {
        match self {
            Self::SrcRoot => "src.exclude",
            Self::Overrides => "overrides.exclude",
        }
    }
}

/// The diagnostic output format.
#[derive(
    Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize, get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum OutputFormat {
    /// The default full mode will print "pretty" diagnostics.
    ///
    /// That is, color will be used when printing to a `tty`.
    /// Moreover, diagnostic messages may include additional
    /// context and annotations on the input to help understand
    /// the message.
    #[default]
    Full,
    /// Print diagnostics in a concise mode.
    ///
    /// This will guarantee that each diagnostic is printed on
    /// a single line. Only the most important or primary aspects
    /// of the diagnostic are included. Contextual information is
    /// dropped.
    ///
    /// This may use color when printing to a `tty`.
    Concise,
    /// Print diagnostics in the JSON format expected by GitLab [Code Quality] reports.
    ///
    /// [Code Quality]: https://docs.gitlab.com/ci/testing/code_quality/#code-quality-report-format
    Gitlab,
    /// Print diagnostics in the format used by [GitHub Actions] workflow error annotations.
    ///
    /// [GitHub Actions]: https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands#setting-an-error-message
    Github,
    /// Print diagnostics as a JUnit-style XML report.
    #[cfg(feature = "junit")]
    Junit,
}

impl OutputFormat {
    /// Returns `true` if this format is intended for users to read directly, in contrast to
    /// machine-readable or structured formats.
    ///
    /// This can be used to check whether information beyond the diagnostics, such as a header or
    /// `Found N diagnostics` footer, should be included.
    pub const fn is_human_readable(&self) -> bool {
        matches!(self, OutputFormat::Full | OutputFormat::Concise)
    }
}

impl From<OutputFormat> for DiagnosticFormat {
    fn from(value: OutputFormat) -> Self {
        match value {
            OutputFormat::Full => Self::Full,
            OutputFormat::Concise => Self::Concise,
            OutputFormat::Gitlab => Self::Gitlab,
            OutputFormat::Github => Self::Github,
            #[cfg(feature = "junit")]
            OutputFormat::Junit => Self::Junit,
        }
    }
}

impl Combine for OutputFormat {
    #[inline(always)]
    fn combine_with(&mut self, _other: Self) {}

    #[inline]
    fn combine(self, _other: Self) -> Self {
        self
    }
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct TerminalOptions {
    /// The format to use for printing diagnostic messages.
    ///
    /// Defaults to `full`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"full"#,
        value_type = "full | concise | github | gitlab | junit",
        example = r#"
            output-format = "concise"
        "#
    )]
    pub output_format: Option<RangedValue<OutputFormat>>,
    /// Use exit code 1, even if all diagnostics only had `warning` severity.
    ///
    /// Defaults to `true`.
    #[option(
        default = r#"true"#,
        value_type = "bool",
        example = r#"
        # Exit with code 0 if all diagnostics had `warning` severity.
        error-on-warning = false
        "#
    )]
    pub error_on_warning: Option<bool>,
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Hash,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct AnalysisOptions {
    /// Whether equality-based checks should preserve broad builtin types rather than narrow them to
    /// literal types.
    ///
    /// By default, ty narrows `value` from `str` to `Literal["a"]` in the positive branch of
    /// `value == "a"`. When this option is enabled, `value` remains `str`. This also applies to
    /// membership tests and literal match patterns, which use equality comparisons.
    ///
    /// ```python
    /// from typing import Literal
    ///
    /// def parse(value: str) -> Literal["a"] | None:
    ///     if value == "a":
    ///         return value  # Accepted by default; `value` remains `str` in strict mode.
    ///     return None
    /// ```
    ///
    /// Broad builtin types include subclasses, but literal types distinguish values by both their
    /// runtime type and value. This makes the narrowing unsound even for subclasses that inherit
    /// builtin equality. For example:
    ///
    /// ```python
    /// class StringSubclass(str): ...
    ///
    /// result = parse(StringSubclass("a"))
    /// # Statically `Literal["a"] | None`, but `result` has runtime type `StringSubclass`.
    /// ```
    ///
    /// The standard library's `StrEnum` and `IntEnum` types are also subclasses of `str` and `int`,
    /// respectively. This means enum members can encounter the same unsoundness:
    ///
    /// ```python
    /// from enum import StrEnum
    ///
    /// class Choice(StrEnum):
    ///     A = "a"
    ///
    /// result = parse(Choice.A)
    /// # Statically `Literal["a"] | None`, but `result` has runtime type `Choice`.
    /// ```
    ///
    /// A subclass can also override `__eq__` to compare equal to a literal with a different value:
    ///
    /// ```python
    /// class MisleadingStr(str):
    ///     def __eq__(self, other: object) -> bool:
    ///         return True
    ///
    /// result = parse(MisleadingStr("b"))
    /// # Statically `Literal["a"] | None`, but `result` contains `"b"` at runtime.
    /// ```
    ///
    /// Enable this option to preserve the broader builtin type instead.
    ///
    /// Defaults to `false`.
    #[option(
        default = r#"false"#,
        value_type = "bool",
        example = r#"
        # Preserve broad builtin types instead of narrowing them to literals
        strict-literal-narrowing = true
        "#
    )]
    pub strict_literal_narrowing: Option<bool>,

    /// Whether ty should respect `type: ignore` comments.
    ///
    /// When set to `false`, `type: ignore` comments are treated like any other normal
    /// comment and can't be used to suppress ty errors (you have to use `ty: ignore` instead).
    ///
    /// Setting this option can be useful when using ty alongside other type checkers or when
    /// you prefer using `ty: ignore` over `type: ignore`.
    ///
    /// Defaults to `true`.
    #[option(
        default = r#"true"#,
        value_type = "bool",
        example = r#"
        # Disable support for `type: ignore` comments
        respect-type-ignore-comments = false
        "#
    )]
    pub respect_type_ignore_comments: Option<bool>,

    /// A list of module glob patterns for which `unresolved-import` diagnostics should be suppressed.
    ///
    /// Details on supported glob patterns:
    /// - `*` matches zero or more characters except `.`. For example, `foo.*` matches `foo.bar` but
    ///   not `foo.bar.baz`; `foo*` matches `foo` and `foobar` but not `foo.bar` or `barfoo`; and `*foo`
    ///   matches `foo` and `barfoo` but not `foo.bar` or `foobar`.
    /// - `**` matches any number of module components (e.g., `foo.**` matches `foo`, `foo.bar`, etc.)
    /// - Prefix a pattern with `!` to exclude matching modules
    ///
    /// When multiple patterns match, later entries take precedence.
    ///
    /// Glob patterns can be used in combinations with each other. For example, to suppress errors for
    /// any module where the first component contains the substring `test`, use `*test*.**`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"[]"#,
        value_type = "list[str]",
        example = r#"
            # Suppress errors for all `test` modules except `test.foo`
            allowed-unresolved-imports = ["test.**", "!test.foo"]
        "#
    )]
    pub allowed_unresolved_imports: Option<Vec<RangedValue<String>>>,

    /// A list of module glob patterns whose imports should be replaced with `typing.Any`.
    ///
    /// Unlike `allowed-unresolved-imports`, this setting replaces the module's type information
    /// with `typing.Any` even if the module can be resolved. Import diagnostics are
    /// unconditionally suppressed for matching modules.
    ///
    /// - Prefix a pattern with `!` to exclude matching modules
    ///
    /// When multiple patterns match, later entries take precedence.
    ///
    /// Glob patterns can be used in combinations with each other. For example, to suppress errors for
    /// any module where the first component contains the substring `test`, use `*test*.**`.
    ///
    /// When multiple patterns match, later entries take precedence.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"[]"#,
        value_type = "list[str]",
        example = r#"
            # Replace all pandas and numpy imports with Any
            replace-imports-with-any = ["pandas.**", "numpy.**"]
        "#
    )]
    pub replace_imports_with_any: Option<Vec<RangedValue<String>>>,
}

impl AnalysisOptions {
    pub(super) fn to_settings(
        &self,
        db: &dyn Db,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> AnalysisSettings {
        let Self {
            strict_literal_narrowing,
            respect_type_ignore_comments,
            allowed_unresolved_imports,
            replace_imports_with_any,
        } = self;

        let AnalysisSettings {
            strict_literal_narrowing: strict_literal_narrowing_default,
            respect_type_ignore_comments: respect_type_ignore_default,
            allowed_unresolved_imports: allowed_unresolved_imports_default,
            replace_imports_with_any: replace_imports_with_any_default,
        } = AnalysisSettings::default();

        let allowed_unresolved_imports =
            if let Some(allowed_unresolved_imports) = allowed_unresolved_imports {
                build_module_glob_set(db, allowed_unresolved_imports, "allowed_unresolved_imports")
                    .unwrap_or_else(|error| {
                        diagnostics.push(*error);
                        ModuleGlobSet::empty()
                    })
            } else {
                allowed_unresolved_imports_default
            };

        let replace_imports_with_any =
            if let Some(replace_imports_with_any) = replace_imports_with_any {
                build_module_glob_set(db, replace_imports_with_any, "replace_imports_with_any")
                    .unwrap_or_else(|error| {
                        diagnostics.push(*error);
                        ModuleGlobSet::empty()
                    })
            } else {
                replace_imports_with_any_default
            };

        AnalysisSettings {
            strict_literal_narrowing: strict_literal_narrowing
                .unwrap_or(strict_literal_narrowing_default),
            respect_type_ignore_comments: respect_type_ignore_comments
                .unwrap_or(respect_type_ignore_default),
            allowed_unresolved_imports,
            replace_imports_with_any,
        }
    }
}

fn build_module_glob_set(
    db: &dyn Db,
    patterns: &[RangedValue<String>],
    option_name: &str,
) -> Result<ModuleGlobSet, Box<OptionDiagnostic>> {
    let mut builder = ModuleGlobSetBuilder::new();

    for glob in patterns {
        if let Err(error) = builder.add(glob) {
            let diagnostic = OptionDiagnostic::new(
                DiagnosticId::InvalidGlob,
                format!("Invalid glob pattern `{error}`"),
                Severity::Error,
            );

            return Err(diagnostic
                .with_source_sub(db, glob, "glob", option_name, error)
                .into());
        }
    }

    builder.build().map_err(|_| {
        let diagnostic = OptionDiagnostic::new(
            DiagnosticId::InvalidGlob,
            "The `{option_name}` patterns resulted in a regex that is too large".to_string(),
            Severity::Error,
        );

        Box::new(diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "Please open an issue on the ty repository and share the patterns that caused the error.",
        )))
    })
}

/// Configures external semantic plugins.
#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PluginsOptions {
    /// Whether semantic plugins are enabled for this project.
    ///
    /// Plugins are disabled by default. Enabling this option only allows plugins
    /// that are explicitly listed in `plugins.plugin`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = "false",
        value_type = "bool",
        example = r#"
            [tool.ty.plugins]
            enabled = true
        "#
    )]
    pub enabled: Option<bool>,

    /// Whether to load trusted plugin packages installed into the project's Python environment.
    ///
    /// Installed plugin packages expose a `ty-plugin.json` manifest next to their artifact. This
    /// is disabled by default. Set this to `true` to activate installed-package plugins for a
    /// project.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = "false",
        value_type = "bool",
        example = r#"
            [tool.ty.plugins]
            auto-discover = true
        "#
    )]
    pub auto_discover: Option<bool>,

    /// Plugin-specific configuration keyed by installed plugin id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<HashMap<String, PluginConfig>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub plugin: Option<PluginEntriesOptions>,
}

impl PluginsOptions {
    fn program_settings_manifests(
        &self,
        project_root: &SystemPath,
        system: &dyn System,
        site_packages: &[SystemPathBuf],
    ) -> Vec<ProgramSettingsPluginManifest> {
        let mut loaded_manifests = Vec::new();

        if self.enabled.unwrap_or(false) {
            for entry in self.plugin.as_deref().unwrap_or_default() {
                let settings = entry.to_settings(project_root, system);
                push_program_settings_plugin_manifest(system, &settings, &mut loaded_manifests);
            }
        }

        for settings in self.auto_discovered_plugins(site_packages, system) {
            push_program_settings_plugin_manifest(system, &settings, &mut loaded_manifests);
        }

        loaded_manifests
    }

    fn active_stub_overlay_paths_for_program_settings(
        &self,
        project_root: &SystemPath,
        system: &dyn System,
        site_packages: &[SystemPathBuf],
    ) -> Vec<SystemPathBuf> {
        let loaded_manifests = self.program_settings_manifests(project_root, system, site_packages);

        let manifests = loaded_manifests
            .iter()
            .map(|loaded| loaded.manifest.clone())
            .collect::<Vec<_>>();
        let Ok(environment) = PluginEnvironment::from_manifests(manifests) else {
            return Vec::new();
        };

        let mut active_stub_overlay_paths = Vec::new();
        for plugin in environment.plugins() {
            if !plugin.manifest().capabilities.stub_overlays {
                continue;
            }

            if let Some(loaded) = loaded_manifests
                .iter()
                .find(|loaded| loaded.manifest.id == plugin.id())
                && let Some(stub_overlay_path) = loaded.stub_overlay_path.as_ref()
            {
                push_unique_path(&mut active_stub_overlay_paths, stub_overlay_path);
            }
        }

        active_stub_overlay_paths
    }

    fn semantic_environment_for_program_settings(
        &self,
        project_root: &SystemPath,
        system: &dyn System,
        site_packages: &[SystemPathBuf],
    ) -> SemanticPluginEnvironment {
        let loaded_manifests = self.program_settings_manifests(project_root, system, site_packages);
        if loaded_manifests.is_empty() {
            return SemanticPluginEnvironment::default();
        }

        let manifests = loaded_manifests
            .iter()
            .map(|loaded| loaded.manifest.clone())
            .collect::<Vec<_>>();
        let Ok(environment) = PluginEnvironment::from_manifests(manifests) else {
            return SemanticPluginEnvironment::default();
        };

        let mut hasher = CacheKeyHasher::new();
        true.cache_key(&mut hasher);
        let mut semantic_plugins = Vec::new();

        for plugin in environment.plugins() {
            let Some(loaded) = loaded_manifests
                .iter()
                .find(|loaded| loaded.manifest.id == plugin.id())
            else {
                continue;
            };

            if !loaded.runtime.participates_in_semantic_hooks() {
                continue;
            }

            loaded.configured_id.cache_key(&mut hasher);
            loaded.manifest_path.cache_key(&mut hasher);
            loaded.manifest_content_hash.cache_key(&mut hasher);
            loaded.artifact_path.cache_key(&mut hasher);
            loaded.artifact_content_hash.cache_key(&mut hasher);
            loaded.config_hash.cache_key(&mut hasher);
            loaded.strict_settings.cache_key(&mut hasher);

            let manifest = plugin.manifest();
            let class_transform_claims = if manifest.capabilities.class_transform {
                manifest
                    .claims
                    .classes
                    .iter()
                    .map(|claim| match &claim.kind {
                        ClassClaimKind::Exact { qualified_name }
                        | ClassClaimKind::SubclassOf {
                            base_qualified_name: qualified_name,
                        } => qualified_name.clone(),
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };

            let class_member_claims = if manifest.capabilities.class_member {
                manifest
                    .claims
                    .attributes
                    .iter()
                    .filter_map(|claim| {
                        let (owner_qualified_name, attribute_name, scope) =
                            claim.exact_attribute()?;
                        (scope == AttributeScope::Class).then(|| {
                            SemanticPluginMemberClaim::new(
                                owner_qualified_name.to_string(),
                                attribute_name.to_string(),
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };

            let instance_member_claims = if manifest.capabilities.instance_member {
                manifest
                    .claims
                    .attributes
                    .iter()
                    .filter_map(|claim| {
                        let (owner_qualified_name, attribute_name, scope) =
                            claim.exact_attribute()?;
                        (scope == AttributeScope::Instance).then(|| {
                            SemanticPluginMemberClaim::new(
                                owner_qualified_name.to_string(),
                                attribute_name.to_string(),
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let instance_member_on_subclass_claims = if manifest.capabilities.instance_member {
                manifest
                    .claims
                    .attributes
                    .iter()
                    .filter_map(|claim| match &claim.kind {
                        AttributeClaimKind::OnSubclassOf {
                            owner_base_qualified_name,
                            scope: AttributeScope::Instance,
                        } => Some(owner_base_qualified_name.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let (mutation_class_claims, mutation_subclass_claims) =
                if manifest.capabilities.mutation_validation {
                    manifest.claims.mutations.iter().fold(
                        (Vec::new(), Vec::new()),
                        |(mut exact, mut subclasses), claim| {
                            match &claim.kind {
                                ClassClaimKind::Exact { qualified_name } => {
                                    exact.push(qualified_name.clone());
                                }
                                ClassClaimKind::SubclassOf {
                                    base_qualified_name,
                                } => subclasses.push(base_qualified_name.clone()),
                            }
                            (exact, subclasses)
                        },
                    )
                } else {
                    (Vec::new(), Vec::new())
                };

            let project_index_participates = manifest.capabilities.project_index
                || manifest.capabilities.cross_symbol_contributions
                || manifest.capabilities.settings_data
                || manifest.capabilities.virtual_types;
            let settings_module_claims = if manifest.capabilities.settings_data {
                let config = serde_json::from_str::<serde_json::Value>(&loaded.config_json)
                    .unwrap_or_default();
                manifest
                    .claims
                    .settings
                    .iter()
                    .filter_map(|claim| {
                        claim.config_key.as_ref().map_or_else(
                            || (!claim.module.is_empty()).then(|| claim.module.clone()),
                            |key| {
                                config
                                    .get(key)
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string)
                            },
                        )
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };

            // A constructor call is claimed by declaring the class's qualified name as a
            // `functions` claim; regular functions and exact `Class.method` method claims are
            // flattened into the same qualified-name list the semantic layer matches call sites
            // against. Subclass-pattern method claims stay structured so the semantic layer can
            // match them against bound receiver hierarchy at the call site.
            let call_signature_claims = if manifest.capabilities.call_signature {
                call_claim_names(manifest)
            } else {
                Vec::new()
            };
            let call_signature_method_on_subclass_claims = if manifest.capabilities.call_signature {
                call_method_on_subclass_claims(manifest)
            } else {
                Vec::new()
            };

            let call_return_claims = if manifest.capabilities.call_return {
                call_claim_names(manifest)
            } else {
                Vec::new()
            };
            let call_return_method_on_subclass_claims = if manifest.capabilities.call_return {
                call_method_on_subclass_claims(manifest)
            } else {
                Vec::new()
            };

            if class_transform_claims.is_empty()
                && class_member_claims.is_empty()
                && instance_member_claims.is_empty()
                && instance_member_on_subclass_claims.is_empty()
                && mutation_class_claims.is_empty()
                && mutation_subclass_claims.is_empty()
                && call_signature_claims.is_empty()
                && call_return_claims.is_empty()
                && call_signature_method_on_subclass_claims.is_empty()
                && call_return_method_on_subclass_claims.is_empty()
                && !project_index_participates
            {
                continue;
            }

            semantic_plugins.push(
                SemanticPlugin::new(
                    plugin.id().to_string(),
                    match loaded.runtime {
                        PluginRuntimeSettings::Mock => SemanticPluginRuntime::Mock,
                        PluginRuntimeSettings::Wasm => SemanticPluginRuntime::Wasm,
                        PluginRuntimeSettings::Subprocess => continue,
                    },
                    class_transform_claims,
                    class_member_claims,
                    instance_member_claims,
                    call_signature_claims,
                    call_return_claims,
                )
                .with_call_method_on_subclass_claims(
                    call_signature_method_on_subclass_claims,
                    call_return_method_on_subclass_claims,
                )
                .with_instance_member_on_subclass_claims(instance_member_on_subclass_claims)
                .with_mutation_claims(mutation_class_claims, mutation_subclass_claims)
                .with_settings_module_claims(settings_module_claims)
                .with_config_json(loaded.config_json.clone())
                .with_strict_settings(loaded.strict_settings)
                .with_project_index_enabled(project_index_participates),
            );
        }

        SemanticPluginEnvironment::new(hasher.finish(), semantic_plugins)
    }

    pub(super) fn to_settings(
        &self,
        db: &dyn Db,
        project_root: &SystemPath,
        system: &dyn System,
        site_packages: &[SystemPathBuf],
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> PluginSettings {
        let enabled = self.enabled.unwrap_or(false);
        let mut plugins = Vec::new();
        let mut reload_paths = Vec::new();
        let mut loaded_manifests = Vec::new();

        for entry in self.plugin.as_deref().unwrap_or_default() {
            let settings = entry.to_settings(project_root, system);
            push_unique_path(&mut reload_paths, settings.path());
            if let Some(manifest_path) = settings.manifest_path() {
                push_unique_path(&mut reload_paths, manifest_path);
            }
            if let Some(stub_overlay_path) = settings.stub_overlay_path() {
                push_unique_path(&mut reload_paths, stub_overlay_path);
            }

            if !enabled {
                diagnostics.push(
                    plugin_diagnostic_at_value(
                        db,
                        &entry.id,
                        format!(
                            "Plugin `{}` is configured but plugins are disabled",
                            settings.id()
                        ),
                        Severity::Warning,
                    )
                    .sub(SubDiagnostic::new(
                        SubDiagnosticSeverity::Info,
                        "Set `plugins.enabled = true` to enable configured plugins.",
                    )),
                );
                plugins.push(settings);
                continue;
            }

            if !system.is_file(settings.path()) {
                diagnostics.push(plugin_diagnostic_at_relative_path(
                    db,
                    &entry.path,
                    format!(
                        "Plugin `{}` points to an artifact path that does not exist or is not a file",
                        settings.id()
                    ),
                    Severity::Error,
                    format!("`{}` does not exist or is not a file", settings.path()),
                ));
                plugins.push(settings);
                continue;
            }

            if settings.runtime().requires_trust() && !settings.trusted() {
                diagnostics.push(
                    plugin_diagnostic_at_value(
                        db,
                        &entry.id,
                        format!(
                            "Plugin `{}` is not trusted to execute local code",
                            settings.id()
                        ),
                        Severity::Error,
                    )
                    .sub(SubDiagnostic::new(
                        SubDiagnosticSeverity::Info,
                        "Set `trusted = true` for this plugin only if you trust the artifact.",
                    )),
                );
                plugins.push(settings);
                continue;
            }

            if !settings.runtime().is_supported() {
                let message = format!(
                    "Plugin `{}` uses unsupported runtime `{}`",
                    settings.id(),
                    settings.runtime().as_str()
                );
                let diagnostic = if let Some(runtime) = entry.runtime.as_ref() {
                    plugin_diagnostic_at_value(db, runtime, message, Severity::Error)
                } else {
                    plugin_diagnostic_at_value(db, &entry.id, message, Severity::Error)
                };
                diagnostics.push(diagnostic);
                plugins.push(settings);
                continue;
            }

            if let Some(stub_overlay_path) = settings.stub_overlay_path()
                && !system.is_directory(stub_overlay_path)
            {
                if let Some(stub_overlay_option) = entry.stub_overlay_path.as_ref() {
                    diagnostics.push(plugin_diagnostic_at_relative_path(
                        db,
                        stub_overlay_option,
                        format!(
                            "Plugin `{}` points to a stub overlay path that does not exist or is not a directory",
                            settings.id()
                        ),
                        Severity::Error,
                        format!("`{stub_overlay_path}` does not exist or is not a directory"),
                    ));
                }
                plugins.push(settings);
                continue;
            }

            if let Some(loaded_manifest) =
                load_plugin_manifest(db, system, entry, &settings, diagnostics)
            {
                if settings.stub_overlay_path().is_some()
                    && !loaded_manifest.manifest.capabilities.stub_overlays
                {
                    if let Some(stub_overlay_option) = entry.stub_overlay_path.as_ref() {
                        diagnostics.push(plugin_diagnostic_at_relative_path(
                            db,
                            stub_overlay_option,
                            format!(
                                "Plugin `{}` configures a stub overlay path but the manifest does not declare the stub-overlays capability",
                                settings.id()
                            ),
                            Severity::Error,
                            "manifest must set `capabilities.stub-overlays = true`",
                        ));
                    }
                    plugins.push(settings);
                    continue;
                }

                loaded_manifests.push(loaded_manifest);
            }

            plugins.push(settings);
        }

        let mut auto_discovered_plugin_loaded = false;
        for settings in self.auto_discovered_plugins(site_packages, system) {
            push_plugin_reload_paths(&settings, &mut reload_paths);

            if !settings.runtime().is_supported() {
                tracing::warn!(
                    "Skipping installed plugin `{}` because runtime `{}` is unavailable in this ty build",
                    settings.id(),
                    settings.runtime().as_str()
                );
                continue;
            }

            let Some(loaded) = load_plugin_manifest_for_program_settings(system, &settings) else {
                tracing::warn!("Skipping invalid installed plugin `{}`", settings.id());
                continue;
            };

            if loaded.stub_overlay_path.is_some() && !loaded.manifest.capabilities.stub_overlays {
                tracing::warn!(
                    "Skipping installed plugin `{}` because its stub overlay is not declared by its manifest",
                    settings.id()
                );
                continue;
            }

            auto_discovered_plugin_loaded = true;
            loaded_manifests.push(loaded.into());
            plugins.push(settings);
        }

        let (environment_fingerprint, active_stub_overlay_paths) =
            build_plugin_environment(db, &loaded_manifests, diagnostics);

        PluginSettings::new(
            enabled || auto_discovered_plugin_loaded,
            plugins,
            environment_fingerprint,
            reload_paths,
            active_stub_overlay_paths,
        )
    }

    fn auto_discovered_plugins(
        &self,
        site_packages: &[SystemPathBuf],
        system: &dyn System,
    ) -> Vec<PluginEntrySettings> {
        if self.auto_discover.unwrap_or(false) {
            let configured_ids = self
                .plugin
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|entry| entry.id.to_string())
                .collect::<Vec<_>>();
            discover_installed_plugins(site_packages, system)
                .into_iter()
                .filter(|plugin| !configured_ids.iter().any(|id| id == plugin.id()))
                .map(|plugin| {
                    if let Some(config) = self
                        .config
                        .as_ref()
                        .and_then(|config| config.get(plugin.id()))
                    {
                        plugin.with_config(config.clone())
                    } else {
                        plugin
                    }
                })
                .collect()
        } else {
            Vec::new()
        }
    }
}

/// Plugin entries configured under `plugins.plugin`.
#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Combine,
    Serialize,
    Deserialize,
    RustDoc,
    get_size2::GetSize,
)]
#[serde(transparent)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PluginEntriesOptions(Vec<RangedValue<PluginEntryOptions>>);

impl OptionsMetadata for PluginEntriesOptions {
    fn documentation() -> Option<&'static str> {
        Some(<Self as RustDoc>::rust_doc())
    }

    fn record(visit: &mut dyn Visit) {
        OptionSet::of::<PluginEntryOptions>().record(visit);
    }
}

impl Deref for PluginEntriesOptions {
    type Target = [RangedValue<PluginEntryOptions>];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(
    Debug,
    Clone,
    Eq,
    PartialEq,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PluginEntryOptions {
    /// Stable plugin identifier.
    #[option(
        default = r#""""#,
        value_type = "str",
        example = r#"
            [[tool.ty.plugins.plugin]]
            id = "pydantic"
        "#
    )]
    pub id: RangedValue<String>,

    /// Path to the plugin artifact.
    #[option(
        default = r#""""#,
        value_type = "str",
        example = r#"
            [[tool.ty.plugins.plugin]]
            path = ".ty/plugins/pydantic.wasm"
        "#
    )]
    pub path: RelativePathBuf,

    /// Runtime used to execute the plugin artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#""wasm""#,
        value_type = "wasm | subprocess | mock",
        example = r#"
            [[tool.ty.plugins.plugin]]
            runtime = "wasm"
        "#
    )]
    pub runtime: Option<RangedValue<PluginRuntimeOption>>,

    /// Optional path to a separate manifest file.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "str",
        example = r#"
            [[tool.ty.plugins.plugin]]
            manifest-path = ".ty/plugins/pydantic.plugin.json"
        "#
    )]
    pub manifest_path: Option<RelativePathBuf>,

    /// Plugin-specific configuration passed through the stable protocol.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"{}"#,
        value_type = "dict[str, Any]",
        example = r#"
            [[tool.ty.plugins.plugin]]
            config = { init-typed = true }
        "#
    )]
    pub config: Option<PluginConfig>,

    /// Optional path to a plugin-provided stub overlay root.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "str",
        example = r#"
            [[tool.ty.plugins.plugin]]
            stub-overlay-path = ".ty/plugins/pydantic-stubs"
        "#
    )]
    pub stub_overlay_path: Option<RelativePathBuf>,

    /// Whether this plugin artifact is trusted to execute locally.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = "false",
        value_type = "bool",
        example = r#"
            [[tool.ty.plugins.plugin]]
            trusted = true
        "#
    )]
    pub trusted: Option<bool>,
}

impl PluginEntryOptions {
    fn to_settings(&self, project_root: &SystemPath, system: &dyn System) -> PluginEntrySettings {
        PluginEntrySettings::new(
            self.id.to_string(),
            self.runtime.as_deref().copied().unwrap_or_default().into(),
            self.path.absolute(project_root, system),
            self.manifest_path
                .as_ref()
                .map(|path| path.absolute(project_root, system)),
            self.config.clone().unwrap_or_default(),
            self.stub_overlay_path
                .as_ref()
                .map(|path| path.absolute(project_root, system)),
            self.trusted.unwrap_or(false),
        )
    }
}

#[derive(
    Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize, get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum PluginRuntimeOption {
    #[default]
    Wasm,
    Subprocess,
    Mock,
}

impl Combine for PluginRuntimeOption {
    #[inline(always)]
    fn combine_with(&mut self, _other: Self) {}

    #[inline]
    fn combine(self, _other: Self) -> Self {
        self
    }
}

impl From<PluginRuntimeOption> for PluginRuntimeSettings {
    fn from(value: PluginRuntimeOption) -> Self {
        match value {
            PluginRuntimeOption::Wasm => Self::Wasm,
            PluginRuntimeOption::Subprocess => Self::Subprocess,
            PluginRuntimeOption::Mock => Self::Mock,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, get_size2::GetSize)]
#[serde(transparent)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PluginConfig(#[get_size(ignore)] serde_json::Value);

impl PluginConfig {
    pub const fn as_value(&self) -> &serde_json::Value {
        &self.0
    }
}

impl Combine for PluginConfig {
    #[inline(always)]
    fn combine_with(&mut self, _other: Self) {}

    #[inline]
    fn combine(self, _other: Self) -> Self {
        self
    }
}

#[derive(Debug)]
struct ProgramSettingsPluginManifest {
    configured_id: String,
    runtime: PluginRuntimeSettings,
    manifest: PluginManifest,
    manifest_path: SystemPathBuf,
    manifest_content_hash: u64,
    artifact_path: SystemPathBuf,
    artifact_content_hash: u64,
    config_hash: u64,
    config_json: String,
    strict_settings: bool,
    stub_overlay_path: Option<SystemPathBuf>,
}

/// Flatten a manifest's claimed callees into qualified names the semantic layer can match call
/// sites against: bare function/constructor names plus `Class.method` for method claims.
fn call_claim_names(manifest: &PluginManifest) -> Vec<String> {
    manifest
        .claims
        .functions
        .iter()
        .map(|claim| claim.qualified_name.clone())
        .chain(manifest.claims.methods.iter().filter_map(|method| {
            let MethodClaimKind::Exact {
                class_qualified_name,
                method_name,
            } = &method.kind
            else {
                return None;
            };
            Some(format!("{class_qualified_name}.{method_name}"))
        }))
        .collect()
}

fn call_method_on_subclass_claims(manifest: &PluginManifest) -> Vec<SemanticPluginMethodClaim> {
    manifest
        .claims
        .methods
        .iter()
        .filter_map(|method| {
            let MethodClaimKind::OnSubclassOf {
                base_qualified_name,
                method_name,
            } = &method.kind
            else {
                return None;
            };
            Some(SemanticPluginMethodClaim::on_subclass_of(
                base_qualified_name.clone(),
                method_name.clone(),
            ))
        })
        .collect()
}

#[derive(Debug)]
struct LoadedPluginManifest {
    configured_id: String,
    manifest: PluginManifest,
    manifest_path: SystemPathBuf,
    manifest_content_hash: u64,
    artifact_path: SystemPathBuf,
    artifact_content_hash: u64,
    config_hash: u64,
    stub_overlay_path: Option<SystemPathBuf>,
}

impl From<ProgramSettingsPluginManifest> for LoadedPluginManifest {
    fn from(value: ProgramSettingsPluginManifest) -> Self {
        Self {
            configured_id: value.configured_id,
            manifest: value.manifest,
            manifest_path: value.manifest_path,
            manifest_content_hash: value.manifest_content_hash,
            artifact_path: value.artifact_path,
            artifact_content_hash: value.artifact_content_hash,
            config_hash: value.config_hash,
            stub_overlay_path: value.stub_overlay_path,
        }
    }
}

const INSTALLED_PLUGIN_MANIFEST: &str = "ty-plugin.json";

fn discover_installed_plugins(
    site_packages: &[SystemPathBuf],
    system: &dyn System,
) -> Vec<PluginEntrySettings> {
    let mut plugins = Vec::new();

    for site_packages_dir in site_packages {
        let Ok(entries) = system.read_directory(site_packages_dir) else {
            continue;
        };

        for entry in entries.flatten() {
            if !entry.file_type().is_directory() {
                continue;
            }

            let package_dir = entry.into_path();
            let manifest_path = package_dir.join(INSTALLED_PLUGIN_MANIFEST);
            let Ok(content) = system.read_to_string(&manifest_path) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<PluginManifest>(&content) else {
                tracing::warn!("Skipping invalid installed plugin manifest at `{manifest_path}`");
                continue;
            };
            if manifest.id.is_empty() {
                tracing::warn!(
                    "Skipping installed plugin manifest at `{manifest_path}` without an id"
                );
                continue;
            }
            let stub_overlay_path =
                (!manifest.stub_overlays.is_empty()).then(|| package_dir.join("stubs"));
            if let Some(stub_overlay_path) = stub_overlay_path.as_ref()
                && !system.is_directory(stub_overlay_path)
            {
                tracing::warn!(
                    "Skipping installed plugin `{}` because stub overlay `{stub_overlay_path}` is missing",
                    manifest.id
                );
                continue;
            }

            let (runtime, artifact_path) = match manifest.runtime {
                RuntimeSpec::Mock => (PluginRuntimeSettings::Mock, manifest_path.clone()),
                RuntimeSpec::Wasm(wasm) => {
                    let artifact_path = package_dir.join(wasm.artifact);
                    if !system.is_file(&artifact_path) {
                        tracing::warn!(
                            "Skipping installed plugin `{}` because artifact `{artifact_path}` is missing",
                            manifest.id
                        );
                        continue;
                    }
                    (PluginRuntimeSettings::Wasm, artifact_path)
                }
                RuntimeSpec::Subprocess(_) => continue,
            };

            if plugins
                .iter()
                .any(|plugin: &PluginEntrySettings| plugin.id() == manifest.id)
            {
                continue;
            }

            plugins.push(PluginEntrySettings::new(
                manifest.id,
                runtime,
                artifact_path,
                Some(manifest_path),
                PluginConfig::default(),
                stub_overlay_path,
                true,
            ));
        }
    }

    plugins
}

fn push_plugin_reload_paths(settings: &PluginEntrySettings, reload_paths: &mut Vec<SystemPathBuf>) {
    push_unique_path(reload_paths, settings.path());
    if let Some(manifest_path) = settings.manifest_path() {
        push_unique_path(reload_paths, manifest_path);
    }
    if let Some(stub_overlay_path) = settings.stub_overlay_path() {
        push_unique_path(reload_paths, stub_overlay_path);
    }
}

fn push_program_settings_plugin_manifest(
    system: &dyn System,
    settings: &PluginEntrySettings,
    loaded_manifests: &mut Vec<ProgramSettingsPluginManifest>,
) {
    if !system.is_file(settings.path())
        || settings.runtime().requires_trust() && !settings.trusted()
        || !settings.runtime().is_supported()
    {
        return;
    }

    if let Some(stub_overlay_path) = settings.stub_overlay_path()
        && !system.is_directory(stub_overlay_path)
    {
        return;
    }

    let Some(loaded_manifest) = load_plugin_manifest_for_program_settings(system, settings) else {
        return;
    };

    if loaded_manifest.stub_overlay_path.is_some()
        && !loaded_manifest.manifest.capabilities.stub_overlays
    {
        return;
    }

    loaded_manifests.push(loaded_manifest);
}

fn load_plugin_manifest_for_program_settings(
    system: &dyn System,
    settings: &PluginEntrySettings,
) -> Option<ProgramSettingsPluginManifest> {
    let manifest_path = settings
        .manifest_path()
        .cloned()
        .unwrap_or_else(|| settings.path().clone());

    if !system.is_file(&manifest_path) {
        return None;
    }

    let manifest_content = system.read_to_string(&manifest_path).ok()?;
    let manifest = serde_json::from_str::<PluginManifest>(&manifest_content).ok()?;

    if manifest.id != settings.id() {
        return None;
    }

    let artifact_content_hash = if manifest_path.as_path() == settings.path().as_path() {
        content_hash(manifest_content.as_bytes())
    } else {
        content_hash(&system.read_to_bytes(settings.path()).ok()?)
    };

    Some(ProgramSettingsPluginManifest {
        configured_id: settings.id().to_string(),
        runtime: settings.runtime(),
        manifest,
        manifest_path,
        manifest_content_hash: content_hash(manifest_content.as_bytes()),
        artifact_path: settings.path().clone(),
        artifact_content_hash,
        config_hash: json_hash(settings.config().as_value()),
        config_json: serde_json::to_string(settings.config().as_value())
            .unwrap_or_else(|_| "{}".to_string()),
        strict_settings: plugin_strict_settings(settings.config().as_value()),
        stub_overlay_path: settings.stub_overlay_path().cloned(),
    })
}

fn load_plugin_manifest(
    db: &dyn Db,
    system: &dyn System,
    entry: &RangedValue<PluginEntryOptions>,
    settings: &PluginEntrySettings,
    diagnostics: &mut Vec<OptionDiagnostic>,
) -> Option<LoadedPluginManifest> {
    let manifest_path = settings
        .manifest_path()
        .cloned()
        .unwrap_or_else(|| settings.path().clone());
    let manifest_source = entry.manifest_path.as_ref().unwrap_or(&entry.path);

    if !system.is_file(&manifest_path) {
        diagnostics.push(plugin_diagnostic_at_relative_path(
            db,
            manifest_source,
            format!(
                "Plugin `{}` points to a manifest path that does not exist or is not a file",
                settings.id()
            ),
            Severity::Error,
            format!("`{manifest_path}` does not exist or is not a file"),
        ));
        return None;
    }

    let manifest_content = match system.read_to_string(&manifest_path) {
        Ok(content) => content,
        Err(error) => {
            diagnostics.push(plugin_diagnostic_at_relative_path(
                db,
                manifest_source,
                format!("Failed to read manifest for plugin `{}`", settings.id()),
                Severity::Error,
                error,
            ));
            return None;
        }
    };

    let manifest = match serde_json::from_str::<PluginManifest>(&manifest_content) {
        Ok(manifest) => manifest,
        Err(error) => {
            diagnostics.push(plugin_diagnostic_at_relative_path(
                db,
                manifest_source,
                format!("Failed to parse manifest for plugin `{}`", settings.id()),
                Severity::Error,
                error,
            ));
            return None;
        }
    };

    if manifest.id != settings.id() {
        diagnostics.push(plugin_diagnostic_at_value(
            db,
            &entry.id,
            format!(
                "Configured plugin id `{}` does not match manifest id `{}`",
                settings.id(),
                manifest.id
            ),
            Severity::Error,
        ));
        return None;
    }

    let artifact_content_hash = if manifest_path.as_path() == settings.path().as_path() {
        content_hash(manifest_content.as_bytes())
    } else {
        match system.read_to_bytes(settings.path()) {
            Ok(content) => content_hash(&content),
            Err(error) => {
                diagnostics.push(plugin_diagnostic_at_relative_path(
                    db,
                    &entry.path,
                    format!("Failed to read artifact for plugin `{}`", settings.id()),
                    Severity::Error,
                    error,
                ));
                return None;
            }
        }
    };

    Some(LoadedPluginManifest {
        configured_id: settings.id().to_string(),
        manifest,
        manifest_path,
        manifest_content_hash: content_hash(manifest_content.as_bytes()),
        artifact_path: settings.path().clone(),
        artifact_content_hash,
        config_hash: json_hash(settings.config().as_value()),
        stub_overlay_path: settings.stub_overlay_path().cloned(),
    })
}

fn build_plugin_environment(
    db: &dyn Db,
    loaded_manifests: &[LoadedPluginManifest],
    diagnostics: &mut Vec<OptionDiagnostic>,
) -> (PluginEnvironmentFingerprint, Vec<SystemPathBuf>) {
    if loaded_manifests.is_empty() {
        return (PluginEnvironmentFingerprint::default(), Vec::new());
    }

    let manifests = loaded_manifests
        .iter()
        .map(|loaded| loaded.manifest.clone())
        .collect::<Vec<_>>();

    let environment = match PluginEnvironment::from_manifests(manifests) {
        Ok(environment) => environment,
        Err(error) => {
            diagnostics.push(plugin_host_error_diagnostic(db, loaded_manifests, error));
            return (PluginEnvironmentFingerprint::default(), Vec::new());
        }
    };

    let mut hasher = CacheKeyHasher::new();
    true.cache_key(&mut hasher);
    let mut active_stub_overlay_paths = Vec::new();

    for plugin in environment.plugins() {
        if let Some(loaded) = loaded_manifests
            .iter()
            .find(|loaded| loaded.manifest.id == plugin.id())
        {
            loaded.configured_id.cache_key(&mut hasher);
            loaded.manifest_path.cache_key(&mut hasher);
            loaded.manifest_content_hash.cache_key(&mut hasher);
            loaded.artifact_path.cache_key(&mut hasher);
            loaded.artifact_content_hash.cache_key(&mut hasher);
            loaded.config_hash.cache_key(&mut hasher);
            if plugin.manifest().capabilities.stub_overlays
                && let Some(stub_overlay_path) = loaded.stub_overlay_path.as_ref()
            {
                push_unique_path(&mut active_stub_overlay_paths, stub_overlay_path);
            }
        }
    }

    (
        PluginEnvironmentFingerprint::new(hasher.finish()),
        active_stub_overlay_paths,
    )
}

fn plugin_host_error_diagnostic(
    db: &dyn Db,
    loaded_manifests: &[LoadedPluginManifest],
    error: HostError,
) -> OptionDiagnostic {
    match error {
        HostError::DuplicatePluginId(plugin_id) => plugin_diagnostic_at_loaded_manifest(
            db,
            loaded_manifests,
            &plugin_id,
            format!("Plugin id `{plugin_id}` is declared by multiple plugin manifests"),
            Severity::Error,
        ),
        HostError::UnsupportedProtocolVersion {
            plugin_id,
            major,
            minor,
            supported_major,
            supported_minor,
        } => plugin_diagnostic_at_loaded_manifest(
            db,
            loaded_manifests,
            &plugin_id,
            format!(
                "Plugin `{plugin_id}` uses unsupported protocol version {major}.{minor}; ty supports {supported_major}.{supported_minor}"
            ),
            Severity::Error,
        ),
        HostError::StubOverlayCapabilityMissing { plugin_id } => {
            plugin_diagnostic_at_loaded_manifest(
                db,
                loaded_manifests,
                &plugin_id,
                format!(
                    "Plugin `{plugin_id}` declares stub overlays without the stub-overlays capability"
                ),
                Severity::Error,
            )
        }
        HostError::ClassTransformCapabilityMissing { plugin_id } => {
            plugin_diagnostic_at_loaded_manifest(
                db,
                loaded_manifests,
                &plugin_id,
                format!(
                    "Plugin `{plugin_id}` declares class-transform claims without the class-transform capability"
                ),
                Severity::Error,
            )
        }
        HostError::ClassMemberCapabilityMissing { plugin_id } => {
            plugin_diagnostic_at_loaded_manifest(
                db,
                loaded_manifests,
                &plugin_id,
                format!(
                    "Plugin `{plugin_id}` declares class-member claims without the class-member capability"
                ),
                Severity::Error,
            )
        }
        HostError::InstanceMemberCapabilityMissing { plugin_id } => {
            plugin_diagnostic_at_loaded_manifest(
                db,
                loaded_manifests,
                &plugin_id,
                format!(
                    "Plugin `{plugin_id}` declares instance-member claims without the instance-member capability"
                ),
                Severity::Error,
            )
        }
        HostError::CallCapabilityMissing { plugin_id } => plugin_diagnostic_at_loaded_manifest(
            db,
            loaded_manifests,
            &plugin_id,
            format!("Plugin `{plugin_id}` declares call claims without a call hook capability"),
            Severity::Error,
        ),
        HostError::SettingsDataCapabilityMissing { plugin_id } => {
            plugin_diagnostic_at_loaded_manifest(
                db,
                loaded_manifests,
                &plugin_id,
                format!(
                    "Plugin `{plugin_id}` declares settings summaries without the settings-data capability"
                ),
                Severity::Error,
            )
        }
        HostError::CrossSymbolContributionsCapabilityMissing { plugin_id } => {
            plugin_diagnostic_at_loaded_manifest(
                db,
                loaded_manifests,
                &plugin_id,
                format!(
                    "Plugin `{plugin_id}` declares contribution-target claims without the cross-symbol-contributions capability"
                ),
                Severity::Error,
            )
        }
        HostError::ProjectIndexCapabilityMissing { plugin_id } => {
            plugin_diagnostic_at_loaded_manifest(
                db,
                loaded_manifests,
                &plugin_id,
                format!(
                    "Plugin `{plugin_id}` declares cross-symbol contributions without the project-index capability"
                ),
                Severity::Error,
            )
        }
        HostError::MutationValidationCapabilityMissing { plugin_id } => {
            plugin_diagnostic_at_loaded_manifest(
                db,
                loaded_manifests,
                &plugin_id,
                format!(
                    "Plugin `{plugin_id}` declares mutation claims without the mutation-validation capability"
                ),
                Severity::Error,
            )
        }
        HostError::UnknownPlugin(plugin_id) => plugin_diagnostic_at_loaded_manifest(
            db,
            loaded_manifests,
            &plugin_id,
            format!("Unknown plugin id `{plugin_id}`"),
            Severity::Error,
        ),
        HostError::Runtime { plugin_id, source } => plugin_diagnostic_at_loaded_manifest(
            db,
            loaded_manifests,
            &plugin_id,
            format!("Plugin `{plugin_id}` runtime failed: {source}"),
            Severity::Error,
        ),
    }
}

fn plugin_diagnostic_at_loaded_manifest(
    db: &dyn Db,
    loaded_manifests: &[LoadedPluginManifest],
    plugin_id: &str,
    message: String,
    severity: Severity,
) -> OptionDiagnostic {
    if let Some(loaded) = loaded_manifests
        .iter()
        .find(|loaded| loaded.manifest.id == plugin_id)
    {
        plugin_diagnostic_at_system_path(
            db,
            &loaded.manifest_path,
            message,
            severity,
            "plugin manifest",
        )
    } else {
        OptionDiagnostic::new(DiagnosticId::PluginConfiguration, message, severity)
    }
}

fn plugin_diagnostic_at_value<T>(
    db: &dyn Db,
    value: &RangedValue<T>,
    message: String,
    severity: Severity,
) -> OptionDiagnostic {
    let diagnostic = OptionDiagnostic::new(DiagnosticId::PluginConfiguration, message, severity);
    match value.source() {
        ValueSource::File(file_path) => diagnostic.with_annotation(config_annotation(
            db,
            file_path,
            value.range(),
            "plugin configuration",
        )),
        ValueSource::Cli => diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "The plugin option was specified on the CLI.",
        )),
        ValueSource::Editor => diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "The plugin option was specified in the editor settings.",
        )),
    }
}

fn plugin_diagnostic_at_relative_path(
    db: &dyn Db,
    value: &RelativePathBuf,
    message: String,
    severity: Severity,
    detail: impl Display,
) -> OptionDiagnostic {
    let diagnostic = OptionDiagnostic::new(DiagnosticId::PluginConfiguration, message, severity);
    match value.source() {
        ValueSource::File(file_path) => {
            diagnostic.with_annotation(config_annotation(db, file_path, value.range(), detail))
        }
        ValueSource::Cli => diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            format!("The plugin path was specified on the CLI: {detail}"),
        )),
        ValueSource::Editor => diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            format!("The plugin path was specified in the editor settings: {detail}"),
        )),
    }
}

fn plugin_diagnostic_at_system_path(
    db: &dyn Db,
    path: &SystemPath,
    message: String,
    severity: Severity,
    label: impl Display,
) -> OptionDiagnostic {
    OptionDiagnostic::new(DiagnosticId::PluginConfiguration, message, severity).with_annotation(
        system_path_to_file(db, path)
            .ok()
            .map(|file| Annotation::primary(Span::from(file)).message(label.to_string())),
    )
}

fn config_annotation(
    db: &dyn Db,
    file_path: &SystemPath,
    range: Option<ruff_text_size::TextRange>,
    message: impl Display,
) -> Option<Annotation> {
    system_path_to_file(db, file_path).ok().map(|file| {
        Annotation::primary(Span::from(file).with_optional_range(range))
            .message(message.to_string())
    })
}

fn push_unique_path(paths: &mut Vec<SystemPathBuf>, path: &SystemPath) {
    if paths.iter().all(|existing| existing.as_path() != path) {
        paths.push(path.to_path_buf());
    }
}

fn content_hash(content: &[u8]) -> u64 {
    let mut hasher = CacheKeyHasher::new();
    content.cache_key(&mut hasher);
    hasher.finish()
}

fn json_hash(value: &serde_json::Value) -> u64 {
    let mut hasher = CacheKeyHasher::new();
    match serde_json::to_string(value) {
        Ok(json) => json.cache_key(&mut hasher),
        Err(error) => error.to_string().cache_key(&mut hasher),
    }
    hasher.finish()
}

fn plugin_strict_settings(config: &serde_json::Value) -> bool {
    config
        .get("strict-settings")
        .or_else(|| config.get("strict_settings"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Configuration override that applies to specific files based on glob patterns.
///
/// An override allows you to apply different rule configurations to specific
/// files or directories. Multiple overrides can match the same file, with
/// later overrides take precedence. Override rules take precedence over global
/// rules for matching files.
///
/// For example, to relax enforcement of rules in test files:
///
/// ```toml
/// [[tool.ty.overrides]]
/// include = ["tests/**", "**/test_*.py"]
///
/// [tool.ty.overrides.rules]
/// possibly-unresolved-reference = "warn"
/// ```
///
/// Or, to ignore a rule in generated files but retain enforcement in an important file:
///
/// ```toml
/// [[tool.ty.overrides]]
/// include = ["generated/**"]
/// exclude = ["generated/important.py"]
///
/// [tool.ty.overrides.rules]
/// possibly-unresolved-reference = "ignore"
/// ```
#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Eq,
    Combine,
    Serialize,
    Deserialize,
    RustDoc,
    get_size2::GetSize,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct OverridesOptions(Vec<RangedValue<OverrideOptions>>);

impl OptionsMetadata for OverridesOptions {
    fn documentation() -> Option<&'static str> {
        Some(<Self as RustDoc>::rust_doc())
    }

    fn record(visit: &mut dyn Visit) {
        OptionSet::of::<OverrideOptions>().record(visit);
    }
}

impl Deref for OverridesOptions {
    type Target = [RangedValue<OverrideOptions>];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OverrideOptions {
    /// A list of file and directory patterns to include for this override.
    ///
    /// The `include` option follows a similar syntax to `.gitignore` but reversed:
    /// Including a file or directory will make it so that it (and its contents)
    /// are affected by this override.
    ///
    /// If not specified, defaults to `["**"]` (matches all files).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = r#"list[str]"#,
        example = r#"
            [[tool.ty.overrides]]
            include = [
                "src",
                "tests",
            ]
        "#
    )]
    pub include: Option<RangedValue<Vec<RelativeGlobPattern>>>,

    /// A list of file and directory patterns to exclude from this override.
    ///
    /// Patterns follow a syntax similar to `.gitignore`.
    /// Exclude patterns take precedence over include patterns within the same override.
    ///
    /// If not specified, defaults to `[]` (excludes no files).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = r#"list[str]"#,
        example = r#"
            [[tool.ty.overrides]]
            exclude = [
                "generated",
                "*.proto",
                "tests/fixtures/**",
                "!tests/fixtures/important.py"  # Include this one file
            ]
        "#
    )]
    pub exclude: Option<RangedValue<Vec<RelativeGlobPattern>>>,

    /// Rule overrides for files matching the include/exclude patterns.
    ///
    /// These rules will be merged with the global rules, with override rules
    /// taking precedence for matching files. You can set rules to different
    /// severity levels or disable them entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"{...}"#,
        value_type = r#"dict[RuleName | "all", "ignore" | "warn" | "error"]"#,
        example = r#"
            [[tool.ty.overrides]]
            include = ["src"]

            [tool.ty.overrides.rules]
            possibly-unresolved-reference = "ignore"
        "#
    )]
    pub rules: Option<Rules>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub analysis: Option<AnalysisOptions>,
}

trait ToOverride {
    fn to_override(
        &self,
        db: &dyn Db,
        project_root: &SystemPath,
        global_rules: Option<&Rules>,
        global_analysis: Option<&AnalysisOptions>,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> Result<Option<Override>, Box<OptionDiagnostic>>;
}

impl ToOverride for RangedValue<OverrideOptions> {
    fn to_override(
        &self,
        db: &dyn Db,
        project_root: &SystemPath,
        global_rules: Option<&Rules>,
        global_analysis: Option<&AnalysisOptions>,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> Result<Option<Override>, Box<OptionDiagnostic>> {
        let rules = self.rules.or_default();
        let analysis = self.analysis.or_default();

        // First, warn about incorrect or useless overrides.
        if rules.is_empty() && *analysis == AnalysisOptions::default() {
            let mut diagnostic = OptionDiagnostic::new(
                DiagnosticId::UselessOverridesSection,
                "Useless `overrides` section".to_string(),
                Severity::Warning,
            );

            diagnostic = if self.rules.is_none() && self.analysis.is_none() {
                diagnostic = diagnostic.sub(SubDiagnostic::new(
                    SubDiagnosticSeverity::Info,
                    "It has no `rules` or `analysis` table",
                ));
                diagnostic.sub(SubDiagnostic::new(
                    SubDiagnosticSeverity::Info,
                    "Add a `[overrides.rules]` or `[overrides.analysis]` table...",
                ))
            } else {
                if self.rules.is_some() && rules.is_empty() {
                    diagnostic = diagnostic.sub(SubDiagnostic::new(
                        SubDiagnosticSeverity::Info,
                        "The `rules` table is empty",
                    ));
                    diagnostic = diagnostic.sub(SubDiagnostic::new(
                        SubDiagnosticSeverity::Info,
                        "Add a rule to `[overrides.rules]` to override specific rules...",
                    ));
                }

                if self.analysis.is_some() && *analysis == AnalysisOptions::default() {
                    diagnostic = diagnostic.sub(SubDiagnostic::new(
                        SubDiagnosticSeverity::Info,
                        "The `analysis` table is empty",
                    ));
                }

                diagnostic
            };

            diagnostic = diagnostic.sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "or remove the `[[overrides]]` section if there's nothing to override",
            ));

            // Add source annotation if we have source information
            if let Some(source_file) = self.source().file() {
                if let Ok(file) = system_path_to_file(db, source_file) {
                    let annotation =
                        Annotation::primary(Span::from(file).with_optional_range(self.range()))
                            .message("This overrides section overrides no settings");
                    diagnostic = diagnostic.with_annotation(Some(annotation));
                }
            }

            diagnostics.push(diagnostic);
            // Return `None`, because this override doesn't override anything
            return Ok(None);
        }

        let include_missing = self.include.is_none();
        let exclude_empty = self
            .exclude
            .as_ref()
            .is_none_or(|exclude| exclude.is_empty());

        if include_missing && exclude_empty {
            // Neither include nor exclude specified - applies to all files
            let mut diagnostic = OptionDiagnostic::new(
                DiagnosticId::UnnecessaryOverridesSection,
                "Unnecessary `overrides` section".to_string(),
                Severity::Warning,
            );

            diagnostic = if self.exclude.is_none() {
                diagnostic.sub(SubDiagnostic::new(
                    SubDiagnosticSeverity::Info,
                    "It has no `include` or `exclude` option restricting the files",
                ))
            } else {
                diagnostic.sub(SubDiagnostic::new(
                    SubDiagnosticSeverity::Info,
                    "It has no `include` option and `exclude` is empty",
                ))
            };

            diagnostic = diagnostic.sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "Restrict the files by adding a pattern to `include` or `exclude`...",
            ));

            diagnostic = diagnostic.sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "or remove the `[[overrides]]` section and merge the configuration into the root `[rules]` table if the configuration should apply to all files",
            ));

            // Add source annotation if we have source information
            if let Some(source_file) = self.source().file() {
                if let Ok(file) = system_path_to_file(db, source_file) {
                    let annotation =
                        Annotation::primary(Span::from(file).with_optional_range(self.range()))
                            .message("This overrides section applies to all files");
                    diagnostic = diagnostic.with_annotation(Some(annotation));
                }
            }

            diagnostics.push(diagnostic);
        }

        // The override is at least (partially) valid.
        // Construct the matcher and resolve the settings.
        let include = build_include_filter(
            db,
            project_root,
            self.include.as_ref(),
            GlobFilterContext::Overrides,
            diagnostics,
        )?;

        let exclude = build_exclude_filter(
            db,
            project_root,
            self.exclude.as_ref(),
            &[],
            GlobFilterContext::Overrides,
        )?;

        let files = IncludeExcludeFilter::new(include, exclude);

        // Merge global rules with override rules, with override rules taking precedence
        let mut merged_rules = rules.into_owned();

        if let Some(global_rules) = global_rules {
            merged_rules = merged_rules.combine(global_rules.clone());
        }

        // Convert merged rules to rule selection
        let rule_selection = merged_rules.to_rule_selection(db, diagnostics);

        let mut merged_analysis = analysis.into_owned();

        if let Some(global_analysis) = global_analysis {
            merged_analysis = merged_analysis.combine(global_analysis.clone());
        }

        let analysis = merged_analysis.to_settings(db, diagnostics);

        let override_instance = Override {
            files,
            options: Arc::new(InnerOverrideOptions {
                rules: self.rules.clone(),
                analysis: self.analysis.clone(),
            }),
            settings: Arc::new(OverrideSettings {
                rules: rule_selection,
                analysis,
            }),
        };

        Ok(Some(override_instance))
    }
}

/// The options for an override but without the include/exclude patterns.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Combine, get_size2::GetSize)]
pub(super) struct InnerOverrideOptions {
    /// Raw rule options as specified in the configuration.
    /// Used when multiple overrides match a file and need to be merged.
    pub(super) rules: Option<Rules>,

    pub(super) analysis: Option<AnalysisOptions>,
}

/// Error returned when the settings can't be resolved because of a hard error.
#[derive(Debug)]
pub struct ToSettingsError {
    diagnostic: Box<OptionDiagnostic>,
    output_format: OutputFormat,
    color: bool,
}

impl ToSettingsError {
    pub fn pretty<'a>(&'a self, db: &'a dyn Db) -> impl fmt::Display + use<'a> {
        let db: &dyn ruff_db::Db = db;

        fmt::from_fn(move |f| {
            let display_config = DisplayDiagnosticConfig::new("ty")
                .format(self.output_format.into())
                .color(self.color);

            write!(
                f,
                "{}",
                self.diagnostic
                    .to_diagnostic()
                    .display(&db, &display_config)
            )
        })
    }

    pub fn into_diagnostic(self) -> OptionDiagnostic {
        *self.diagnostic
    }
}

impl Display for ToSettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.diagnostic.message)
    }
}

impl std::error::Error for ToSettingsError {}

#[cfg(feature = "schemars")]
mod schema {
    impl schemars::JsonSchema for super::Rules {
        fn schema_name() -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("Rules")
        }

        fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
            use serde_json::{Map, Value};

            let registry = ty_python_semantic::default_lint_registry();
            let level_schema = generator.subschema_for::<super::Level>();

            let mut properties: Map<String, Value> = registry
                .lints()
                .iter()
                .map(|lint| {
                    let mut schema = schemars::Schema::default();
                    let object = schema.ensure_object();
                    object.insert(
                        "title".to_string(),
                        Value::String(lint.summary().to_string()),
                    );
                    object.insert(
                        "description".to_string(),
                        Value::String(lint.documentation()),
                    );
                    if lint.status.is_deprecated() {
                        object.insert("deprecated".to_string(), Value::Bool(true));
                    }
                    object.insert(
                        "default".to_string(),
                        Value::String(lint.default_level.to_string()),
                    );
                    object.insert(
                        "oneOf".to_string(),
                        Value::Array(vec![level_schema.clone().into()]),
                    );

                    (lint.name().to_string(), schema.into())
                })
                .collect();

            let mut all_schema = schemars::Schema::default();
            let all = all_schema.ensure_object();
            all.insert(
                "title".to_string(),
                Value::String("set the default severity level for all rules".to_string()),
            );
            all.insert(
                "description".to_string(),
                Value::String(
                    "Configure a default severity level for all rules. Individual rule settings override this default."
                        .to_string(),
                ),
            );
            all.insert(
                "oneOf".to_string(),
                Value::Array(vec![level_schema.clone().into()]),
            );

            properties.insert("all".to_string(), all_schema.into());

            let mut schema = schemars::json_schema!({ "type": "object" });
            let object = schema.ensure_object();
            object.insert("properties".to_string(), Value::Object(properties));
            // Allow unknown rules: ty will warn about them. It gives a better experience when using an older
            // ty version because the schema will not deny rules that have been removed in newer versions.
            object.insert("additionalProperties".to_string(), level_schema.into());

            schema
        }
    }
}

#[derive(Error, Debug)]
pub enum TyTomlError {
    #[error(transparent)]
    TomlSyntax(#[from] toml::de::Error),
}

#[derive(Debug, PartialEq, Eq, Clone, get_size2::GetSize)]
pub struct OptionDiagnostic {
    id: DiagnosticId,
    message: String,
    concise_message: Option<String>,
    severity: Severity,
    annotation: Option<Annotation>,
    sub: Vec<SubDiagnostic>,
}

impl OptionDiagnostic {
    pub fn new(id: DiagnosticId, message: String, severity: Severity) -> Self {
        Self {
            id,
            message,
            concise_message: None,
            severity,
            annotation: None,
            sub: Vec::new(),
        }
    }

    #[must_use]
    fn with_message(self, message: impl Display) -> Self {
        OptionDiagnostic {
            message: message.to_string(),
            ..self
        }
    }

    #[must_use]
    fn with_concise_message(self, message: impl Display) -> Self {
        OptionDiagnostic {
            concise_message: Some(message.to_string()),
            ..self
        }
    }

    #[must_use]
    fn with_annotation(self, annotation: Option<Annotation>) -> Self {
        OptionDiagnostic { annotation, ..self }
    }

    fn with_source_sub<T>(
        mut self,
        db: &dyn Db,
        value: &RangedValue<T>,
        value_label: &str,
        option_name: &str,
        err: impl Display,
    ) -> Self {
        match value.source() {
            ValueSource::File(file_path) => {
                if let Ok(file) = system_path_to_file(db, &**file_path) {
                    let concise_message = std::mem::take(&mut self.message);
                    self.with_concise_message(concise_message)
                        .with_message(format_args!("Invalid {value_label}"))
                        .with_annotation(Some(
                            Annotation::primary(
                                Span::from(file).with_optional_range(value.range()),
                            )
                            .message(err.to_string()),
                        ))
                } else {
                    self.sub(SubDiagnostic::new(
                        SubDiagnosticSeverity::Info,
                        format!(
                            "The {value_label} is defined in the `{option_name}` option \
                            in your configuration file"
                        ),
                    ))
                }
            }
            ValueSource::Cli => self.sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "The {value_label} was specified on the CLI",
            )),
            ValueSource::Editor => self.sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "The {value_label} was specified in the editor settings.",
            )),
        }
    }

    #[must_use]
    fn sub(mut self, sub: SubDiagnostic) -> Self {
        self.sub.push(sub);
        self
    }

    pub(crate) fn to_diagnostic(&self) -> Diagnostic {
        let mut diag = Diagnostic::new(self.id, self.severity, &self.message);

        if let Some(concise_message) = &self.concise_message {
            diag.set_concise_message(concise_message);
        }

        if let Some(annotation) = self.annotation.clone() {
            diag.annotate(annotation);
        }

        for sub in &self.sub {
            diag.sub(sub.clone());
        }

        diag
    }
}

trait OrDefault {
    type Target: ToOwned;

    fn or_default(&self) -> Cow<'_, Self::Target>;
}

impl<T> OrDefault for Option<T>
where
    T: Default + ToOwned<Owned = T>,
{
    type Target = T;

    fn or_default(&self) -> Cow<'_, Self::Target> {
        match self {
            Some(value) => Cow::Borrowed(value),
            None => Cow::Owned(T::default()),
        }
    }
}

#[cfg(test)]
mod plugin_tests {
    use std::sync::Arc;

    use ruff_db::diagnostic::{Diagnostic, DiagnosticId, Severity};
    use ruff_db::system::{SystemPathBuf, TestSystem};
    use ruff_ranged_value::ValueSource;
    use serde_json::json;
    use ty_python_core::program::{
        FallibleStrategy, Program, SemanticPluginMethodClaim, SemanticPluginRuntime,
    };

    use crate::{Db as _, ProjectDatabase, ProjectMetadata};

    use super::{Options, PluginRuntimeSettings};
    use crate::metadata::settings::WASM_RUNTIME_SUPPORTED;

    const PROJECT_ROOT: &str = "/project";
    const CONFIG_PATH: &str = "/project/ty.toml";
    const ARTIFACT_PATH: &str = "/project/.ty/plugins/pydantic.mock";
    const MANIFEST_PATH: &str = "/project/.ty/plugins/pydantic.plugin.json";
    // A virtualenv's `site-packages` sits at `<prefix>/Lib/site-packages` on Windows and
    // `<prefix>/lib/pythonX.Y/site-packages` everywhere else. These fixtures have to match the
    // layout `ty_site_packages` actually probes for, or discovery finds nothing on Windows.
    #[cfg(windows)]
    const SITE_PACKAGES: &str = "/project/.venv/Lib/site-packages";
    #[cfg(not(windows))]
    const SITE_PACKAGES: &str = "/project/.venv/lib/python3.13/site-packages";

    #[cfg(windows)]
    const AUTO_PLUGIN_MANIFEST_PATH: &str =
        "/project/.venv/Lib/site-packages/django_ty/ty-plugin.json";
    #[cfg(not(windows))]
    const AUTO_PLUGIN_MANIFEST_PATH: &str =
        "/project/.venv/lib/python3.13/site-packages/django_ty/ty-plugin.json";
    #[cfg(feature = "plugins-wasm")]
    const WASM_ARTIFACT_PATH: &str = "/project/.ty/plugins/toy-field.wasm";
    #[cfg(feature = "plugins-wasm")]
    const WASM_MANIFEST_PATH: &str = "/project/.ty/plugins/toy-field.plugin.json";
    #[cfg(feature = "plugins-wasm")]
    const MINIMAL_WASM_PLUGIN: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "ty_plugin_alloc") (param i32) (result i32) i32.const 1024)
          (func (export "ty_plugin_handle") (param i32 i32) (result i64) i64.const 0))
    "#;

    #[test]
    fn parses_resolves_and_fingerprints_plugin_options() {
        let db = project_database(
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "pydantic"
            path = ".ty/plugins/pydantic.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/pydantic.plugin.json"
            stub-overlay-path = ".ty/plugins/pydantic-stubs"
            trusted = true
            config = { init-typed = true }
            "#,
            [
                (ARTIFACT_PATH, "plugin artifact"),
                (MANIFEST_PATH, &manifest_json("pydantic", 0)),
                ("/project/.ty/plugins/pydantic-stubs/__init__.py", ""),
            ],
        );

        assert_plugin_diagnostics(&db, []);

        let plugins = db.project().settings(&db).plugins();
        assert!(plugins.enabled());
        assert_ne!(plugins.environment_fingerprint().get(), 0);
        assert_eq!(
            plugins.reload_paths(),
            &[
                SystemPathBuf::from(ARTIFACT_PATH),
                SystemPathBuf::from(MANIFEST_PATH),
                SystemPathBuf::from("/project/.ty/plugins/pydantic-stubs"),
            ]
        );
        assert_eq!(
            plugins.active_stub_overlay_paths(),
            &[SystemPathBuf::from("/project/.ty/plugins/pydantic-stubs")]
        );

        let [plugin] = plugins.plugins() else {
            panic!("expected one plugin entry");
        };

        assert_eq!(plugin.id(), "pydantic");
        assert_eq!(plugin.runtime(), PluginRuntimeSettings::Mock);
        assert_eq!(plugin.path(), &SystemPathBuf::from(ARTIFACT_PATH));
        assert_eq!(
            plugin.manifest_path(),
            Some(&SystemPathBuf::from(MANIFEST_PATH))
        );
        assert_eq!(
            plugin.stub_overlay_path(),
            Some(&SystemPathBuf::from("/project/.ty/plugins/pydantic-stubs"))
        );
        assert!(plugin.trusted());
        assert_eq!(plugin.config().as_value()["init-typed"], json!(true));
    }

    #[test]
    fn discovers_installed_plugin_packages_when_enabled() {
        let db = project_database(
            r#"
            [environment]
            python = ".venv"

            [plugins]
            auto-discover = true
            "#,
            [
                (
                    "/project/.venv/pyvenv.cfg",
                    "home = /python\nversion = 3.13.0",
                ),
                ("/python/bin/python3", ""),
                (AUTO_PLUGIN_MANIFEST_PATH, &installed_plugin_manifest_json()),
            ],
        );

        assert_plugin_diagnostics(&db, []);

        let plugins = db.project().settings(&db).plugins();
        assert!(plugins.enabled());
        let [plugin] = plugins.plugins() else {
            panic!("expected one discovered plugin");
        };
        assert_eq!(plugin.id(), "django-ty");
        assert_eq!(plugin.runtime(), PluginRuntimeSettings::Mock);
        assert_eq!(
            plugin.path(),
            &SystemPathBuf::from(AUTO_PLUGIN_MANIFEST_PATH)
        );
        assert_eq!(
            plugin.manifest_path(),
            Some(&SystemPathBuf::from(AUTO_PLUGIN_MANIFEST_PATH))
        );
        assert!(plugin.trusted());

        let semantic_plugins = Program::get(&db).semantic_plugins(&db);
        let [plugin] = semantic_plugins.plugins() else {
            panic!("expected one discovered semantic plugin");
        };
        assert_eq!(plugin.id(), "django-ty");
        assert_eq!(plugin.runtime(), SemanticPluginRuntime::Mock);
        assert_eq!(plugin.class_transform_claims(), ["django.db.models.Model"]);
    }

    #[test]
    fn installed_plugin_discovery_is_disabled_by_default() {
        let db = project_database(
            r#"
            [environment]
            python = ".venv"
            "#,
            [
                (
                    "/project/.venv/pyvenv.cfg",
                    "home = /python\nversion = 3.13.0",
                ),
                ("/python/bin/python3", ""),
                (AUTO_PLUGIN_MANIFEST_PATH, &installed_plugin_manifest_json()),
            ],
        );

        assert_plugin_diagnostics(&db, []);
        assert!(!db.project().settings(&db).plugins().enabled());
        assert!(Program::get(&db).semantic_plugins(&db).plugins().is_empty());
    }

    #[test]
    fn installed_plugin_discovery_activates_packaged_stub_overlays() {
        let db = project_database(
            r#"
            [environment]
            python = ".venv"

            [plugins]
            auto-discover = true
            "#,
            [
                (
                    "/project/.venv/pyvenv.cfg",
                    "home = /python\nversion = 3.13.0",
                ),
                ("/python/bin/python3", ""),
                (
                    AUTO_PLUGIN_MANIFEST_PATH,
                    &installed_plugin_manifest_with_stub_json(),
                ),
                (
                    &format!("{SITE_PACKAGES}/django_ty/stubs/django/__init__.pyi"),
                    "",
                ),
            ],
        );

        assert_plugin_diagnostics(&db, []);
        assert_eq!(
            db.project()
                .settings(&db)
                .plugins()
                .active_stub_overlay_paths(),
            &[SystemPathBuf::from(format!(
                "{SITE_PACKAGES}/django_ty/stubs"
            ))]
        );
    }

    #[test]
    fn explicit_plugin_configuration_overrides_an_installed_plugin() {
        let db = project_database(
            r#"
            [environment]
            python = ".venv"

            [plugins]
            enabled = true
            auto-discover = true

            [[plugins.plugin]]
            id = "django-ty"
            path = ".ty/plugins/django-ty.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/django-ty.plugin.json"
            "#,
            [
                (
                    "/project/.venv/pyvenv.cfg",
                    "home = /python\nversion = 3.13.0",
                ),
                ("/python/bin/python3", ""),
                (AUTO_PLUGIN_MANIFEST_PATH, &installed_plugin_manifest_json()),
                ("/project/.ty/plugins/django-ty.mock", "plugin artifact"),
                (
                    "/project/.ty/plugins/django-ty.plugin.json",
                    &manifest_json("django-ty", 0),
                ),
            ],
        );

        assert_plugin_diagnostics(&db, []);
        let [plugin] = db.project().settings(&db).plugins().plugins() else {
            panic!("expected one explicitly configured plugin");
        };
        assert_eq!(
            plugin.path(),
            &SystemPathBuf::from("/project/.ty/plugins/django-ty.mock")
        );
    }

    #[test]
    fn subclass_class_transform_claim_participates_in_semantic_environment() {
        let db = project_database(
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "minidjango"
            path = ".ty/plugins/minidjango.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/minidjango.plugin.json"
            "#,
            [
                ("/project/.ty/plugins/minidjango.mock", "plugin artifact"),
                (
                    "/project/.ty/plugins/minidjango.plugin.json",
                    &subclass_transform_manifest_json(),
                ),
            ],
        );

        assert_plugin_diagnostics(&db, []);

        let semantic_plugins = Program::get(&db).semantic_plugins(&db);
        let [plugin] = semantic_plugins.plugins() else {
            panic!("expected one semantic plugin");
        };

        assert_eq!(plugin.id(), "minidjango");
        assert_eq!(plugin.runtime(), SemanticPluginRuntime::Mock);
        assert_eq!(plugin.class_transform_claims(), ["minidjango.Model"]);
        assert!(plugin.project_index_enabled());
        assert_eq!(
            plugin.call_return_method_on_subclass_claims(),
            [
                SemanticPluginMethodClaim::on_subclass_of("minidjango.Manager", "filter"),
                SemanticPluginMethodClaim::on_subclass_of("minidjango.Manager", "get"),
            ]
        );
        assert!(plugin.call_signature_method_on_subclass_claims().is_empty());
    }

    #[test]
    fn settings_data_claim_participates_in_semantic_environment() {
        let db = project_database(
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "settings-reader"
            path = ".ty/plugins/settings-reader.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/settings-reader.plugin.json"
            config = { strict-settings = true }
            "#,
            [
                (
                    "/project/.ty/plugins/settings-reader.mock",
                    "plugin artifact",
                ),
                (
                    "/project/.ty/plugins/settings-reader.plugin.json",
                    &settings_data_manifest_json(),
                ),
            ],
        );

        assert_plugin_diagnostics(&db, []);

        let semantic_plugins = Program::get(&db).semantic_plugins(&db);
        let [plugin] = semantic_plugins.plugins() else {
            panic!("expected one semantic plugin");
        };

        assert_eq!(plugin.id(), "settings-reader");
        assert_eq!(plugin.runtime(), SemanticPluginRuntime::Mock);
        assert!(plugin.project_index_enabled());
        assert!(plugin.strict_settings());
        assert_eq!(plugin.settings_module_claims(), ["minidjango_settings"]);
        assert!(plugin.class_transform_claims().is_empty());
        assert!(plugin.call_return_claims().is_empty());
    }

    #[test]
    fn reports_invalid_plugin_artifact_path() {
        let db = project_database(
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "pydantic"
            path = ".ty/plugins/missing.mock"
            runtime = "mock"
            "#,
            [],
        );

        assert_plugin_diagnostics(
            &db,
            [(
                Severity::Error,
                "Plugin `pydantic` points to an artifact path that does not exist or is not a file",
            )],
        );
    }

    #[test]
    fn reports_bad_plugin_manifest() {
        let db = project_database(
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "pydantic"
            path = ".ty/plugins/pydantic.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/pydantic.plugin.json"
            "#,
            [(ARTIFACT_PATH, "plugin artifact"), (MANIFEST_PATH, "{")],
        );

        assert_plugin_diagnostics(
            &db,
            [(
                Severity::Error,
                "Failed to parse manifest for plugin `pydantic`",
            )],
        );
    }

    #[test]
    fn reports_stub_overlay_without_manifest_capability() {
        let db = project_database(
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "pydantic"
            path = ".ty/plugins/pydantic.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/pydantic.plugin.json"
            stub-overlay-path = ".ty/plugins/pydantic-stubs"
            "#,
            [
                (ARTIFACT_PATH, "plugin artifact"),
                (
                    MANIFEST_PATH,
                    &manifest_json_without_capabilities("pydantic", 0),
                ),
                ("/project/.ty/plugins/pydantic-stubs/__init__.py", ""),
            ],
        );

        assert_plugin_diagnostics(
            &db,
            [(
                Severity::Error,
                "Plugin `pydantic` configures a stub overlay path but the manifest does not declare the stub-overlays capability",
            )],
        );
        assert!(
            db.project()
                .settings(&db)
                .plugins()
                .active_stub_overlay_paths()
                .is_empty()
        );
    }

    #[test]
    fn reports_plugin_protocol_mismatch() {
        let db = project_database(
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "pydantic"
            path = ".ty/plugins/pydantic.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/pydantic.plugin.json"
            "#,
            [
                (ARTIFACT_PATH, "plugin artifact"),
                (MANIFEST_PATH, &manifest_json("pydantic", 99)),
            ],
        );

        assert_plugin_diagnostics(
            &db,
            [(
                Severity::Error,
                "Plugin `pydantic` uses unsupported protocol version 99.1; ty supports 0.3",
            )],
        );
    }

    // Runs on builds that do not embed the WASM runtime — which always includes the `wasm32`
    // `ty_wasm` build, where the runtime is compiled out. There, a `wasm` plugin runtime is
    // reported unsupported through a settings diagnostic.
    #[cfg(not(feature = "plugins-wasm"))]
    #[test]
    fn reports_unsupported_plugin_runtime() {
        let db = project_database(
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "pydantic"
            path = ".ty/plugins/pydantic.wasm"
            runtime = "wasm"
            trusted = true
            "#,
            [("/project/.ty/plugins/pydantic.wasm", "plugin artifact")],
        );

        assert_plugin_diagnostics(
            &db,
            [(
                Severity::Error,
                "Plugin `pydantic` uses unsupported runtime `wasm`",
            )],
        );
    }

    #[test]
    fn wasm_runtime_support_follows_build() {
        // `mock` always runs; `wasm` follows the build (embedded only on native + `plugins-wasm`);
        // `subprocess` is not implemented.
        assert!(PluginRuntimeSettings::Mock.is_supported());
        assert_eq!(
            PluginRuntimeSettings::Wasm.is_supported(),
            WASM_RUNTIME_SUPPORTED
        );
        assert!(!PluginRuntimeSettings::Subprocess.is_supported());

        // The `wasm32` `ty_wasm` build never embeds the runtime.
        #[cfg(target_arch = "wasm32")]
        assert!(!PluginRuntimeSettings::Wasm.is_supported());

        assert!(PluginRuntimeSettings::Mock.participates_in_semantic_hooks());
        assert_eq!(
            PluginRuntimeSettings::Wasm.participates_in_semantic_hooks(),
            WASM_RUNTIME_SUPPORTED
        );
        assert!(!PluginRuntimeSettings::Subprocess.participates_in_semantic_hooks());
    }

    #[cfg(feature = "plugins-wasm")]
    #[test]
    fn supported_wasm_plugin_participates_in_semantic_environment() {
        let db = project_database(
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "toy-field"
            path = ".ty/plugins/toy-field.wasm"
            runtime = "wasm"
            manifest-path = ".ty/plugins/toy-field.plugin.json"
            trusted = true
            config = { mode = "str" }
            "#,
            [
                (WASM_ARTIFACT_PATH, MINIMAL_WASM_PLUGIN),
                (WASM_MANIFEST_PATH, &wasm_manifest_json()),
            ],
        );

        assert_plugin_diagnostics(&db, []);

        let semantic_plugins = Program::get(&db).semantic_plugins(&db);
        let [plugin] = semantic_plugins.plugins() else {
            panic!("expected one semantic plugin");
        };

        assert_eq!(plugin.id(), "toy-field");
        assert_eq!(plugin.runtime(), SemanticPluginRuntime::Wasm);
        assert_eq!(plugin.call_return_claims(), ["toy.Field"]);
        assert_ne!(semantic_plugins.fingerprint(), 0);
    }

    #[cfg(feature = "plugins-wasm")]
    #[test]
    fn wasm_semantic_fingerprint_tracks_artifact_and_config() {
        let config = |mode: &str| {
            format!(
                r#"
                [plugins]
                enabled = true

                [[plugins.plugin]]
                id = "toy-field"
                path = ".ty/plugins/toy-field.wasm"
                runtime = "wasm"
                manifest-path = ".ty/plugins/toy-field.plugin.json"
                trusted = true
                config = {{ mode = "{mode}" }}
                "#
            )
        };

        let db = project_database(
            &config("str"),
            [
                (WASM_ARTIFACT_PATH, MINIMAL_WASM_PLUGIN),
                (WASM_MANIFEST_PATH, &wasm_manifest_json()),
            ],
        );
        let changed_artifact = project_database(
            &config("str"),
            [
                (
                    WASM_ARTIFACT_PATH,
                    "(module (memory (export \"memory\") 1))",
                ),
                (WASM_MANIFEST_PATH, &wasm_manifest_json()),
            ],
        );
        let changed_config = project_database(
            &config("int"),
            [
                (WASM_ARTIFACT_PATH, MINIMAL_WASM_PLUGIN),
                (WASM_MANIFEST_PATH, &wasm_manifest_json()),
            ],
        );

        let fingerprint = Program::get(&db).semantic_plugins(&db).fingerprint();
        assert_ne!(
            fingerprint,
            Program::get(&changed_artifact)
                .semantic_plugins(&changed_artifact)
                .fingerprint()
        );
        assert_ne!(
            fingerprint,
            Program::get(&changed_config)
                .semantic_plugins(&changed_config)
                .fingerprint()
        );
    }

    #[test]
    fn reports_disabled_plugin() {
        let db = project_database(
            r#"
            [plugins]
            enabled = false

            [[plugins.plugin]]
            id = "pydantic"
            path = ".ty/plugins/missing.mock"
            runtime = "mock"
            "#,
            [],
        );

        assert_plugin_diagnostics(
            &db,
            [(
                Severity::Warning,
                "Plugin `pydantic` is configured but plugins are disabled",
            )],
        );
    }

    #[test]
    fn reports_untrusted_plugin_runtime() {
        let db = project_database(
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "pydantic"
            path = ".ty/plugins/pydantic.wasm"
            runtime = "wasm"
            "#,
            [("/project/.ty/plugins/pydantic.wasm", "plugin artifact")],
        );

        assert_plugin_diagnostics(
            &db,
            [(
                Severity::Error,
                "Plugin `pydantic` is not trusted to execute local code",
            )],
        );
    }

    fn project_database<'a>(
        config: &str,
        files: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> ProjectDatabase {
        let system = TestSystem::default();
        let config = config.to_string();
        let mut all_files = vec![(SystemPathBuf::from(CONFIG_PATH), config.clone())];
        all_files.extend(
            files
                .into_iter()
                .map(|(path, content)| (SystemPathBuf::from(path), content.to_string())),
        );
        system
            .memory_file_system()
            .write_files_all(all_files)
            .expect("failed to write test files");

        let options = Options::from_toml_str(
            &config,
            ValueSource::File(Arc::new(SystemPathBuf::from(CONFIG_PATH))),
        )
        .expect("valid ty config");
        let metadata = ProjectMetadata::from_options(
            options,
            SystemPathBuf::from(PROJECT_ROOT),
            None,
            &FallibleStrategy,
        )
        .expect("valid project metadata");

        ProjectDatabase::fallible(metadata, system).expect("valid project database")
    }

    fn plugin_diagnostics(db: &ProjectDatabase) -> Vec<Diagnostic> {
        db.project()
            .check_settings(db)
            .into_iter()
            .filter(|diagnostic| diagnostic.id() == DiagnosticId::PluginConfiguration)
            .collect()
    }

    fn assert_plugin_diagnostics<const N: usize>(
        db: &ProjectDatabase,
        expected: [(Severity, &str); N],
    ) {
        let diagnostics = plugin_diagnostics(db);
        let actual = diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.severity(), diagnostic.primary_message()))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    fn manifest_json(id: &str, protocol_major: u16) -> String {
        format!(
            r#"{{
                "id": "{id}",
                "name": "Pydantic plugin",
                "version": "0.1.0",
                "protocol-version": {{ "major": {protocol_major}, "minor": 1 }},
                "ty-compatibility": {{ "requirement": ">=0.0.0" }},
                "runtime": {{ "kind": "mock" }},
                "capabilities": {{ "stub-overlays": true }}
            }}"#
        )
    }

    fn installed_plugin_manifest_json() -> String {
        r#"{
            "id": "django-ty",
            "name": "Django ty plugin",
            "version": "0.1.0",
            "protocol-version": { "major": 0, "minor": 1 },
            "ty-compatibility": { "requirement": ">=0.0.0" },
            "runtime": { "kind": "mock" },
            "capabilities": { "class-transform": true },
            "claims": {
                "classes": [
                    { "kind": "subclass-of", "base-qualified-name": "django.db.models.Model" }
                ]
            }
        }"#
        .to_string()
    }

    fn installed_plugin_manifest_with_stub_json() -> String {
        r#"{
            "id": "django-ty",
            "name": "Django ty plugin",
            "version": "0.1.0",
            "protocol-version": { "major": 0, "minor": 1 },
            "ty-compatibility": { "requirement": ">=0.0.0" },
            "runtime": { "kind": "mock" },
            "capabilities": { "stub-overlays": true },
            "stub-overlays": [
                {
                    "module": "django.db.models.manager",
                    "path": "stubs/django/db/models/manager.pyi"
                }
            ]
        }"#
        .to_string()
    }

    fn manifest_json_without_capabilities(id: &str, protocol_major: u16) -> String {
        format!(
            r#"{{
                "id": "{id}",
                "name": "Pydantic plugin",
                "version": "0.1.0",
                "protocol-version": {{ "major": {protocol_major}, "minor": 1 }},
                "ty-compatibility": {{ "requirement": ">=0.0.0" }},
                "runtime": {{ "kind": "mock" }}
            }}"#
        )
    }

    fn subclass_transform_manifest_json() -> String {
        r#"{
            "id": "minidjango",
            "name": "Mini-Django plugin",
            "version": "0.1.0",
            "protocol-version": { "major": 0, "minor": 1 },
            "ty-compatibility": { "requirement": ">=0.0.0" },
            "runtime": { "kind": "mock" },
            "capabilities": { "class-transform": true, "call-return": true, "project-index": true },
            "claims": {
                "classes": [
                    { "kind": "subclass-of", "base-qualified-name": "minidjango.Model" }
                ],
                "methods": [
                    { "kind": "on-subclass-of", "base-qualified-name": "minidjango.Manager", "method-name": "filter" },
                    { "kind": "on-subclass-of", "base-qualified-name": "minidjango.Manager", "method-name": "get" }
                ]
            }
        }"#
        .to_string()
    }

    fn settings_data_manifest_json() -> String {
        r#"{
            "id": "settings-reader",
            "name": "Settings reader plugin",
            "version": "0.1.0",
            "protocol-version": { "major": 0, "minor": 1 },
            "ty-compatibility": { "requirement": ">=0.0.0" },
            "runtime": { "kind": "mock" },
            "capabilities": { "settings-data": true },
            "claims": {
                "settings": [
                    { "module": "minidjango_settings" }
                ]
            }
        }"#
        .to_string()
    }

    #[cfg(feature = "plugins-wasm")]
    fn wasm_manifest_json() -> String {
        r#"{
            "id": "toy-field",
            "name": "Toy field WASM plugin",
            "version": "0.1.0",
            "protocol-version": { "major": 0, "minor": 1 },
            "ty-compatibility": { "requirement": ">=0.0.0" },
            "runtime": { "kind": "wasm", "artifact": ".ty/plugins/toy-field.wasm" },
            "capabilities": { "call-return": true },
            "claims": {
                "functions": [
                    { "qualified-name": "toy.Field" }
                ]
            }
        }"#
        .to_string()
    }
}
