use std::sync::Arc;

use ruff_db::files::File;
use ruff_db::system::SystemPathBuf;
use ty_combine::Combine;
use ty_python_semantic::AnalysisSettings;
use ty_python_semantic::lint::RuleSelection;

use crate::metadata::options::{InnerOverrideOptions, OutputFormat, PluginConfig};
use crate::{Db, glob::IncludeExcludeFilter};

/// The resolved [`super::Options`] for the project.
///
/// Unlike [`super::Options`], the struct has default values filled in and
/// uses representations that are optimized for reads (instead of preserving the source representation).
/// It's also not required that this structure precisely resembles the TOML schema, although
/// it's encouraged to use a similar structure.
///
/// It's worth considering to adding a salsa query for specific settings to
/// limit the blast radius when only some settings change. For example,
/// changing the terminal settings shouldn't invalidate any core type-checking queries.
/// This can be achieved by adding a salsa query for the type checking specific settings.
///
/// Settings that are part of [`ty_python_core::program::ProgramSettings`] are not included here.
#[derive(Clone, Debug, Eq, PartialEq, get_size2::GetSize)]
pub struct Settings {
    pub(super) rules: Arc<RuleSelection>,
    pub(super) terminal: TerminalSettings,
    pub(super) src: SrcSettings,
    pub(super) analysis: AnalysisSettings,
    pub(super) plugins: PluginSettings,

    /// Settings for configuration overrides that apply to specific file patterns.
    ///
    /// Each override can specify include/exclude patterns and rule configurations
    /// that apply to matching files. Multiple overrides can match the same file,
    /// with later overrides taking precedence.
    pub(super) overrides: Vec<Override>,
}

impl Settings {
    pub fn rules(&self) -> &RuleSelection {
        &self.rules
    }

    pub fn src(&self) -> &SrcSettings {
        &self.src
    }

    pub fn to_rules(&self) -> Arc<RuleSelection> {
        self.rules.clone()
    }

    pub fn terminal(&self) -> &TerminalSettings {
        &self.terminal
    }

    pub fn overrides(&self) -> &[Override] {
        &self.overrides
    }

    pub fn analysis(&self) -> &AnalysisSettings {
        &self.analysis
    }

    pub fn plugins(&self) -> &PluginSettings {
        &self.plugins
    }
}

#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub struct TerminalSettings {
    pub output_format: OutputFormat,
    pub error_on_warning: bool,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            output_format: OutputFormat::default(),
            error_on_warning: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub struct SrcSettings {
    pub respect_ignore_files: bool,
    pub files: IncludeExcludeFilter,
}
impl SrcSettings {
    pub(crate) fn default() -> Self {
        Self {
            respect_ignore_files: true,
            files: IncludeExcludeFilter::default(),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, get_size2::GetSize)]
pub struct PluginSettings {
    enabled: bool,
    plugins: Vec<PluginEntrySettings>,
    environment_fingerprint: PluginEnvironmentFingerprint,
    reload_paths: Vec<SystemPathBuf>,
    active_stub_overlay_paths: Vec<SystemPathBuf>,
}

impl PluginSettings {
    pub const fn new(
        enabled: bool,
        plugins: Vec<PluginEntrySettings>,
        environment_fingerprint: PluginEnvironmentFingerprint,
        reload_paths: Vec<SystemPathBuf>,
        active_stub_overlay_paths: Vec<SystemPathBuf>,
    ) -> Self {
        Self {
            enabled,
            plugins,
            environment_fingerprint,
            reload_paths,
            active_stub_overlay_paths,
        }
    }

    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn plugins(&self) -> &[PluginEntrySettings] {
        &self.plugins
    }

    pub const fn environment_fingerprint(&self) -> PluginEnvironmentFingerprint {
        self.environment_fingerprint
    }

    pub fn reload_paths(&self) -> &[SystemPathBuf] {
        &self.reload_paths
    }

    pub fn active_stub_overlay_paths(&self) -> &[SystemPathBuf] {
        &self.active_stub_overlay_paths
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, get_size2::GetSize)]
pub struct PluginEnvironmentFingerprint(u64);

impl PluginEnvironmentFingerprint {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub struct PluginEntrySettings {
    id: String,
    runtime: PluginRuntimeSettings,
    path: SystemPathBuf,
    manifest_path: Option<SystemPathBuf>,
    config: PluginConfig,
    stub_overlay_path: Option<SystemPathBuf>,
    trusted: bool,
}

impl PluginEntrySettings {
    pub const fn new(
        id: String,
        runtime: PluginRuntimeSettings,
        path: SystemPathBuf,
        manifest_path: Option<SystemPathBuf>,
        config: PluginConfig,
        stub_overlay_path: Option<SystemPathBuf>,
        trusted: bool,
    ) -> Self {
        Self {
            id,
            runtime,
            path,
            manifest_path,
            config,
            stub_overlay_path,
            trusted,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn runtime(&self) -> PluginRuntimeSettings {
        self.runtime
    }

    pub fn path(&self) -> &SystemPathBuf {
        &self.path
    }

    pub fn manifest_path(&self) -> Option<&SystemPathBuf> {
        self.manifest_path.as_ref()
    }

    pub const fn config(&self) -> &PluginConfig {
        &self.config
    }

    pub fn with_config(mut self, config: PluginConfig) -> Self {
        self.config = config;
        self
    }

    pub fn stub_overlay_path(&self) -> Option<&SystemPathBuf> {
        self.stub_overlay_path.as_ref()
    }

    pub const fn trusted(&self) -> bool {
        self.trusted
    }
}

/// Whether this build of `ty` embeds the WASM plugin runtime.
///
/// The runtime (wasmtime) is compiled in only for native targets under the `plugins-wasm` feature.
/// The `wasm32` `ty_wasm` build never embeds it, so there a `wasm` plugin runtime is reported
/// unsupported and produces a settings diagnostic.
pub const WASM_RUNTIME_SUPPORTED: bool =
    cfg!(all(feature = "plugins-wasm", not(target_arch = "wasm32")));

#[derive(Debug, Clone, Copy, PartialEq, Eq, get_size2::GetSize)]
pub enum PluginRuntimeSettings {
    Wasm,
    Subprocess,
    Mock,
}

impl PluginRuntimeSettings {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wasm => "wasm",
            Self::Subprocess => "subprocess",
            Self::Mock => "mock",
        }
    }

    /// Whether this build can execute the runtime. `mock` is always available; `wasm` depends on
    /// [`WASM_RUNTIME_SUPPORTED`]; `subprocess` is not implemented yet.
    pub const fn is_supported(self) -> bool {
        match self {
            Self::Mock => true,
            Self::Wasm => WASM_RUNTIME_SUPPORTED,
            Self::Subprocess => false,
        }
    }

    /// Whether the host must be explicitly trusted before executing this runtime's local artifacts.
    pub const fn requires_trust(self) -> bool {
        matches!(self, Self::Wasm | Self::Subprocess)
    }

    /// Whether this runtime participates in semantic hooks for this build.
    pub const fn participates_in_semantic_hooks(self) -> bool {
        match self {
            Self::Mock => true,
            Self::Wasm => WASM_RUNTIME_SUPPORTED,
            Self::Subprocess => false,
        }
    }
}

/// A single configuration override that applies to files matching specific patterns.
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub struct Override {
    /// File pattern filter to determine which files this override applies to.
    pub(super) files: IncludeExcludeFilter,

    /// The raw options as specified in the configuration (minus `include` and `exclude`.
    /// Necessary to merge multiple overrides if necessary.
    pub(super) options: Arc<InnerOverrideOptions>,

    /// Pre-resolved rule selection for this override alone.
    /// Used for efficient lookup when only this override matches a file.
    pub(super) settings: Arc<OverrideSettings>,
}

impl Override {
    /// Returns whether this override applies to the given file path.
    pub fn matches_file(&self, path: &ruff_db::system::SystemPath) -> bool {
        use crate::glob::{GlobFilterCheckMode, IncludeResult};

        matches!(
            self.files
                .is_file_included(path, GlobFilterCheckMode::Adhoc),
            IncludeResult::Included { .. }
        )
    }
}

/// Resolves the settings for a given file.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn file_settings(db: &dyn Db, file: File) -> FileSettings {
    let settings = db.project().settings(db);

    let path = match file.path(db) {
        ruff_db::files::FilePath::System(path) => path,
        ruff_db::files::FilePath::SystemVirtual(_) | ruff_db::files::FilePath::Vendored(_) => {
            return FileSettings::Global;
        }
    };

    let mut matching_overrides = settings
        .overrides()
        .iter()
        .filter(|over| over.matches_file(path));

    let Some(first) = matching_overrides.next() else {
        // If the file matches no override, it uses the global settings.
        return FileSettings::Global;
    };

    let Some(second) = matching_overrides.next() else {
        tracing::debug!("Applying override for file `{path}`: {}", first.files);
        // If the file matches only one override, return that override's settings.
        return FileSettings::File(Arc::clone(&first.settings));
    };

    let mut filters = tracing::enabled!(tracing::Level::DEBUG)
        .then(|| format!("({}), ({})", first.files, second.files));

    let mut overrides = vec![Arc::clone(&first.options), Arc::clone(&second.options)];

    for over in matching_overrides {
        use std::fmt::Write;

        if let Some(filters) = &mut filters {
            let _ = write!(filters, ", ({})", over.files);
        }

        overrides.push(Arc::clone(&over.options));
    }

    if let Some(filters) = &filters {
        tracing::debug!("Applying multiple overrides for file `{path}`: {filters}");
    }

    merge_overrides(db, overrides, ())
}

/// Merges multiple override options, caching the result.
///
/// Overrides often apply to multiple files. This query ensures that we avoid
/// resolving the same override combinations multiple times.
///
/// ## What's up with the `()` argument?
///
/// This is to make Salsa happy because it requires that queries with only a single argument
/// take a salsa-struct as argument, which isn't the case here. The `()` enables salsa's
/// automatic interning for the arguments.
#[salsa::tracked(heap_size=ruff_memory_usage::heap_size)]
fn merge_overrides(db: &dyn Db, overrides: Vec<Arc<InnerOverrideOptions>>, _: ()) -> FileSettings {
    let mut overrides = overrides.into_iter().rev();
    let mut merged = (*overrides.next().unwrap()).clone();

    for option in overrides {
        merged.combine_with((*option).clone());
    }

    let metadata = db.project().metadata(db);

    // Merge with the project level options by replaying the individual options
    // in the correct precedence order.
    for options in metadata.options_in_precedence_order(metadata.options()) {
        merged.rules.combine_with(options.rules.clone());
        merged.analysis.combine_with(options.analysis.clone());
    }

    if merged.rules.is_none() && merged.analysis.is_none() {
        return FileSettings::Global;
    }

    let rules = merged.rules.unwrap_or_default();
    let analysis = merged.analysis.unwrap_or_default();

    // It's okay to ignore the errors here because the rules are eagerly validated
    // during `overrides.to_settings()`.
    let rules = rules.to_rule_selection(db, &mut Vec::new());
    let analysis = analysis.to_settings(db, &mut Vec::new());

    FileSettings::File(Arc::new(OverrideSettings { rules, analysis }))
}

/// The resolved settings for a file.
#[derive(Debug, Eq, PartialEq, Clone, get_size2::GetSize)]
pub enum FileSettings {
    /// The file uses the global settings.
    Global,

    /// The file has specific override settings.
    File(Arc<OverrideSettings>),
}

impl FileSettings {
    pub fn rules<'a>(&'a self, db: &'a dyn Db) -> &'a RuleSelection {
        match self {
            FileSettings::Global => db.project().settings(db).rules(),
            FileSettings::File(override_settings) => &override_settings.rules,
        }
    }

    pub fn analysis<'a>(&'a self, db: &'a dyn Db) -> &'a AnalysisSettings {
        match self {
            FileSettings::Global => db.project().settings(db).analysis(),
            FileSettings::File(override_settings) => &override_settings.analysis,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, get_size2::GetSize)]
pub struct OverrideSettings {
    pub(super) rules: RuleSelection,
    pub(super) analysis: AnalysisSettings,
}
