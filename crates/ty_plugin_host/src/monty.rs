//! Monty-backed [`PluginRunner`] that executes Python plugin source inside the `monty` sandbox.
//!
//! The bundled `ty_plugin_sdk` prelude is prepended to every plugin source; each call binds the
//! serialized [`PluginRequest`] as `_ty_request_json` and reads back the JSON returned by the
//! prelude's `__ty_handle__` dispatcher. Every call evaluates the plugin fresh — hooks and
//! module-level state do not persist, matching the stateless-per-call WASM runtime.
//! Explicit stateless batches share initialization within one independent snapshot;
//! callers must attest that handlers depend only on their request and index.
//!
//! Two backends share this framing:
//!
//! * `InProcess` embeds the interpreter in `ty`; a hard interpreter abort (stack overflow,
//!   allocator failure) would take the process with it.
//! * `Pool` (`plugins-monty-pool` feature) feeds snippets to `monty` worker subprocesses
//!   resolved from `TY_MONTY_BIN` or `PATH`, so a crashed worker cannot take down `ty`.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use monty::{MontyRun, RunProgress};
#[cfg(feature = "plugins-monty-pool")]
use monty_types::NamedValues;
use monty_types::{
    CollectedStreams, CompileOptions, DateTimeSource, MontyObject, NameLookupResult, OsPolicy,
    PrintWriter, ProcessTime, RandomSeed, RandomStart, ResourceLimits, ResourceTracker, SleepMode,
};
use ty_plugin_protocol::{PluginRequest, PluginResponse};

#[cfg(feature = "plugins-monty-pool")]
use monty_pool::{Pool, PoolConfig, PoolError, ReplConfig, TurnEvent};

use crate::{LoadedPlugin, PluginRunner, RuntimeError};

/// The Python SDK prepended to every plugin source so hook names are ambient inside the sandbox.
const SDK_PRELUDE: &str = include_str!("../python/ty_plugin_sdk.py");

/// Sandbox global carrying the serialized [`PluginRequest`] for one call.
const REQUEST_GLOBAL: &str = "_ty_request_json";
const INDEX_LOOKUP: &str = "_ty_project_index_lookup";
const MAX_INDEX_LOOKUPS: usize = 1000;

/// Environment variable naming the `monty` binary for the pool backend.
#[cfg(feature = "plugins-monty-pool")]
const MONTY_BIN_ENV: &str = "TY_MONTY_BIN";

/// Plugin answers feed semantic caches, so ambient state is pinned deterministic:
/// epoch-fixed clock, zero sleep/process time, fixed random seed — matching the WASM runtime,
/// where the only observable world arrives through the protocol request.
fn os_policy() -> OsPolicy {
    OsPolicy {
        datetime: DateTimeSource::Fixed {
            unix_seconds: 0,
            microsecond: 0,
        },
        sleep: SleepMode::Zero,
        process_time: ProcessTime::Zero,
        random_start: RandomStart::Seed(RandomSeed::Bytes(vec![0])),
        ..OsPolicy::default()
    }
}

/// Resource bounds enforced on plugin calls.
///
/// `max_memory_bytes` applies only to pool workers (they arm `monty-alloc`); the in-process
/// backend cannot enforce it because `ty` owns the global allocator.
#[derive(Debug, Clone)]
pub struct MontyLimits {
    /// Wall-clock budget for a single plugin call, enforced inside the sandbox.
    pub max_feed_duration: Duration,
    /// Host-side deadline for one worker turn. Only the pool backend enforces it.
    pub request_timeout: Duration,
    /// Allocator-backed memory cap in bytes. See the struct docs for where it applies.
    pub max_memory_bytes: Option<usize>,
    /// Maximum Python call-stack depth.
    pub max_recursion_depth: usize,
    /// Maximum accepted response size, in bytes.
    pub max_response_bytes: usize,
}

impl Default for MontyLimits {
    fn default() -> Self {
        Self {
            max_feed_duration: Duration::from_secs(5),
            request_timeout: Duration::from_secs(10),
            max_memory_bytes: None,
            max_recursion_depth: 1000,
            max_response_bytes: 8 * 1024 * 1024,
        }
    }
}

impl MontyLimits {
    fn resource_limits(&self) -> ResourceLimits {
        ResourceLimits {
            max_feed_duration: Some(self.max_feed_duration),
            max_memory: self.max_memory_bytes,
            max_recursion_depth: self.max_recursion_depth,
            // Ordinary calls allow no host suspensions. Opted-in index calls
            // receive a separately bounded lookup capability below.
            max_suspensions: 0,
            ..ResourceLimits::default()
        }
    }
}

#[cfg(feature = "plugins-monty-pool")]
struct PoolState {
    pool: Pool,
    runtime: tokio::runtime::Runtime,
    repl_config: ReplConfig,
}

enum Backend {
    /// Interpreter embedded in `ty`; compiled programs are cached per plugin.
    InProcess {
        runs: Mutex<BTreeMap<String, MontyRun>>,
    },
    /// `monty` worker subprocesses; the combined plugin script is fed per call.
    #[cfg(feature = "plugins-monty-pool")]
    Pool(Box<PoolState>),
}

/// A [`PluginRunner`] that executes Python plugin source through the `monty` interpreter.
pub struct MontyRunner {
    backend: Backend,
    /// The assembled `SDK prelude + plugin source + handler call` script per plugin.
    sources: BTreeMap<String, String>,
    limits: MontyLimits,
}

impl MontyRunner {
    /// Create a runner that executes plugins in-process.
    pub fn new(limits: MontyLimits) -> Result<Self, RuntimeError> {
        Ok(Self {
            backend: Backend::InProcess {
                runs: Mutex::new(BTreeMap::new()),
            },
            sources: BTreeMap::new(),
            limits,
        })
    }

    /// Create a runner backed by a `monty-pool` of worker subprocesses spawned from
    /// `binary_path` (resolved by [`resolve_monty_binary`] when `None`).
    #[cfg(feature = "plugins-monty-pool")]
    pub fn pool(
        limits: MontyLimits,
        binary_path: Option<std::path::PathBuf>,
    ) -> Result<Self, RuntimeError> {
        let Some(binary) = binary_path.or_else(resolve_monty_binary) else {
            return Err(RuntimeError::UnsupportedRuntime(
                "monty worker binary not found; install `pydantic-monty-runtime` or set TY_MONTY_BIN",
            ));
        };

        let runtime = tokio::runtime::Runtime::new()
            .map_err(|err| RuntimeError::Trap(format!("failed to start async runtime: {err}")))?;

        let mut config = PoolConfig::subprocess(binary);
        config.request_timeout = Some(limits.request_timeout);

        let pool = runtime.block_on(Pool::new(config)).map_err(|err| {
            RuntimeError::Trap(format!("failed to start monty worker pool: {err}"))
        })?;

        let repl_config = ReplConfig {
            limits: Some(limits.resource_limits()),
            type_check: false,
            os_policy: os_policy(),
            ..ReplConfig::default()
        };

        Ok(Self {
            backend: Backend::Pool(Box::new(PoolState {
                pool,
                runtime,
                repl_config,
            })),
            sources: BTreeMap::new(),
            limits,
        })
    }

    /// Register a plugin's Python source under its id, wrapped in the SDK prelude and a
    /// trailing `__ty_handle__` call. The in-process backend compiles eagerly so syntax
    /// errors surface at load rather than first use.
    pub fn add_plugin(
        &mut self,
        plugin_id: impl Into<String>,
        source: impl AsRef<[u8]>,
    ) -> Result<(), RuntimeError> {
        let plugin_id = plugin_id.into();
        let source = std::str::from_utf8(source.as_ref()).map_err(|err| {
            RuntimeError::InvalidResponse(format!("plugin source is not valid UTF-8: {err}"))
        })?;
        let script = format!("{SDK_PRELUDE}\n\n{source}\n\n__ty_handle__({REQUEST_GLOBAL})");

        match &mut self.backend {
            Backend::InProcess { runs } => {
                let run = catch_interpreter_panic(|| {
                    MontyRun::new(
                        script.clone(),
                        &format!("{plugin_id}.py"),
                        vec![REQUEST_GLOBAL.to_string(), INDEX_LOOKUP.to_string()],
                        CompileOptions::default(),
                    )
                })?
                .map_err(|err| {
                    RuntimeError::Trap(format!("failed to compile plugin source: {err}"))
                })?
                .with_os_policy(os_policy());
                runs.get_mut()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(plugin_id.clone(), run);
            }
            #[cfg(feature = "plugins-monty-pool")]
            Backend::Pool(_) => {}
        }

        self.sources.insert(plugin_id, script);
        Ok(())
    }

    fn execute_in_process(
        &self,
        plugin: &LoadedPlugin,
        request_json: String,
        runs: &Mutex<BTreeMap<String, MontyRun>>,
        index: Option<&serde_json::Value>,
    ) -> Result<PluginResponse, RuntimeError> {
        let value = self.execute_in_process_value(plugin, request_json, runs, index)?;
        self.decode_response(&value)
    }

    fn execute_in_process_value(
        &self,
        plugin: &LoadedPlugin,
        request_json: String,
        runs: &Mutex<BTreeMap<String, MontyRun>>,
        index: Option<&serde_json::Value>,
    ) -> Result<MontyObject, RuntimeError> {
        let mut runs = runs.lock().map_err(|_| {
            RuntimeError::Trap(
                "Monty interpreter state was invalidated by a panic; reload the project"
                    .to_string(),
            )
        })?;
        let run = runs
            .get_mut(plugin.id())
            .ok_or(RuntimeError::UnsupportedRuntime(
                "monty plugin source was not loaded into the runner",
            ))?;

        if let Some(index) = index {
            // Indexed calls already execute an independent snapshot. Keep the
            // cache locked only while cloning that snapshot, so independent
            // invocations do not serialize their entire interpreter runs.
            let run = run.clone();
            drop(runs);
            return self.execute_indexed_value(plugin, request_json, run, index);
        }

        let mut print_output = CollectedStreams::default();
        let inputs = vec![MontyObject::string(request_json), MontyObject::none()];
        let value = run
            .run(
                inputs,
                ResourceTracker::new(self.limits.resource_limits()),
                PrintWriter::collect_streams(&mut print_output),
            )
            .map_err(|err| RuntimeError::Trap(err.to_string()))?;

        for (stream, text) in print_output.into_entries() {
            tracing::debug!(plugin = plugin.id(), ?stream, "{text}");
        }
        Ok(value)
    }

    fn execute_indexed_value(
        &self,
        plugin: &LoadedPlugin,
        request_json: String,
        run: MontyRun,
        index: &serde_json::Value,
    ) -> Result<MontyObject, RuntimeError> {
        let mut print_output = CollectedStreams::default();
        let inputs = vec![
            MontyObject::string(request_json),
            MontyObject::function(INDEX_LOOKUP, None),
        ];
        let value = {
            let started = Instant::now();
            let mut remaining = MAX_INDEX_LOOKUPS;
            let mut limits = self.limits.resource_limits();
            limits.max_suspensions = MAX_INDEX_LOOKUPS;
            let mut progress = run
                .start(
                    inputs,
                    ResourceTracker::new(limits),
                    PrintWriter::collect_streams(&mut print_output),
                )
                .map_err(|err| RuntimeError::Trap(err.to_string()))?;
            loop {
                if started.elapsed() > self.limits.max_feed_duration {
                    return Err(RuntimeError::Timeout);
                }
                if !matches!(progress, RunProgress::Complete(_)) {
                    remaining = remaining.checked_sub(1).ok_or_else(|| {
                        RuntimeError::Trap("project index lookup limit exceeded".into())
                    })?;
                }
                progress = match progress {
                    RunProgress::Complete(value) => break value,
                    RunProgress::FunctionCall(call)
                        if call.function_name == INDEX_LOOKUP && call.object_id.is_none() =>
                    {
                        let result =
                            lookup_index(index, &call.args, self.limits.max_response_bytes)?;
                        call.resume(
                            MontyObject::string(result),
                            PrintWriter::collect_streams(&mut print_output),
                        )
                    }
                    RunProgress::NameLookup(lookup) => lookup.resume(
                        NameLookupResult::Undefined,
                        PrintWriter::collect_streams(&mut print_output),
                    ),
                    _ => {
                        return Err(RuntimeError::Trap(
                            "Monty index lookup permits no other external operations".into(),
                        ));
                    }
                }
                .map_err(|err| RuntimeError::Trap(err.to_string()))?;
            }
        };

        for (stream, text) in print_output.into_entries() {
            tracing::debug!(plugin = plugin.id(), ?stream, "{text}");
        }

        Ok(value)
    }

    #[cfg(feature = "plugins-monty-pool")]
    fn execute_pool(
        &self,
        plugin: &LoadedPlugin,
        request_json: String,
        state: &PoolState,
    ) -> Result<PluginResponse, RuntimeError> {
        let script = self
            .sources
            .get(plugin.id())
            .ok_or(RuntimeError::UnsupportedRuntime(
                "monty plugin source was not loaded into the runner",
            ))?;

        let mut repl_config = state.repl_config.clone();
        repl_config.script_name = format!("{}.py", plugin.id());
        let plugin_id = plugin.id().to_string();
        let script = script.clone();

        let result = state.runtime.block_on(async {
            let mut session = state
                .pool
                .checkout(&repl_config)
                .await
                .map_err(map_pool_error)?;

            let inputs = NamedValues::from(vec![
                (
                    REQUEST_GLOBAL.to_string(),
                    MontyObject::string(request_json),
                ),
                (INDEX_LOOKUP.to_string(), MontyObject::none()),
            ]);
            let mut on_print = monty_pool::on_print_sync(|_stream, text| {
                tracing::debug!(plugin = plugin_id, "{text}");
            });

            let feed = session
                .feed(script, inputs, Vec::new(), true, &mut on_print)
                .await;
            let _ = session.finish().await;

            match feed.map_err(map_pool_error)? {
                TurnEvent::Complete(value) => Ok(value),
                other => Err(RuntimeError::Trap(format!(
                    "plugin suspended on an unsupported host call: {other:?}"
                ))),
            }
        })?;

        self.decode_response(&result)
    }

    fn decode_response(&self, value: &MontyObject) -> Result<PluginResponse, RuntimeError> {
        let text = value.as_ref().as_str().ok_or_else(|| {
            RuntimeError::InvalidResponse("plugin returned a non-string value".to_string())
        })?;

        if text.len() > self.limits.max_response_bytes {
            return Err(RuntimeError::ResponseTooLarge);
        }

        // Large project indexes contain opaque JSON plus thousands of typed
        // declarations. Decoding the internally tagged enum buffers that whole
        // tree before decoding the payload. A small discriminator pass lets us
        // decode it directly, retaining the original decoder for every failure.
        if text.len() > 64 * 1024 {
            #[derive(serde::Deserialize)]
            struct Kind<'a> {
                kind: &'a str,
            }
            if let Ok(Kind {
                kind: "project-index",
            }) = serde_json::from_str(text)
                && let Ok(index) = serde_json::from_str(text)
            {
                return Ok(PluginResponse::ProjectIndex(index));
            }
        }
        serde_json::from_str(text).map_err(|err| {
            RuntimeError::InvalidResponse(format!("plugin returned invalid response JSON: {err}"))
        })
    }
}

impl PluginRunner for MontyRunner {
    fn execute_stateless_batch_with_project_index(
        &self,
        plugin: &LoadedPlugin,
        requests: &[&PluginRequest],
        index: &serde_json::Value,
    ) -> Result<Vec<PluginResponse>, RuntimeError> {
        if requests.len() > 64 {
            return Err(RuntimeError::InvalidResponse(
                "stateless batch exceeds 64 requests".into(),
            ));
        }
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        #[allow(irrefutable_let_patterns)]
        if plugin.manifest().capabilities.project_index_lookup
            && let Backend::InProcess { runs } = &self.backend
        {
            let mut encoded = Vec::with_capacity(requests.len());
            for request in requests {
                let mut request = (*request).clone();
                let slot = request.project_index_mut().ok_or_else(|| {
                    RuntimeError::InvalidResponse("request has no project index".into())
                })?;
                *slot = None;
                encoded.push(
                    serde_json::to_string(&request)
                        .map_err(|error| RuntimeError::InvalidResponse(error.to_string()))?,
                );
            }
            let payload = serde_json::to_string(&encoded)
                .map_err(|error| RuntimeError::InvalidResponse(error.to_string()))?;
            catch_interpreter_panic(|| {
                let value = self.execute_in_process_value(plugin, payload, runs, Some(index))?;
                let text = value.as_ref().as_str().ok_or_else(|| {
                    RuntimeError::InvalidResponse("batch returned a non-string value".into())
                })?;
                if text.len() > self.limits.max_response_bytes {
                    return Err(RuntimeError::ResponseTooLarge);
                }
                let replies: Vec<String> = serde_json::from_str(text).map_err(|error| {
                    RuntimeError::InvalidResponse(format!("invalid batch response: {error}"))
                })?;
                if replies.len() != requests.len() {
                    return Err(RuntimeError::InvalidResponse(
                        "batch response length mismatch".into(),
                    ));
                }
                replies
                    .into_iter()
                    .map(|reply| self.decode_response(&MontyObject::string(reply)))
                    .collect()
            })?
        } else {
            requests
                .iter()
                .map(|request| self.execute_with_project_index(plugin, request, index))
                .collect()
        }
    }

    fn execute_with_project_index(
        &self,
        plugin: &LoadedPlugin,
        request: &PluginRequest,
        index: &serde_json::Value,
    ) -> Result<PluginResponse, RuntimeError> {
        let mut request = request.clone();
        let slot = request
            .project_index_mut()
            .ok_or_else(|| RuntimeError::InvalidResponse("request has no project index".into()))?;
        *slot = None;
        if plugin.manifest().capabilities.project_index_lookup
            && let Backend::InProcess { runs } = &self.backend
        {
            let request_json = serde_json::to_string(&request)
                .map_err(|err| RuntimeError::InvalidResponse(err.to_string()))?;
            return catch_interpreter_panic(|| {
                self.execute_in_process(plugin, request_json, runs, Some(index))
            })?;
        }
        *request.project_index_mut().unwrap() = Some(index.clone());
        self.execute(plugin, &request)
    }

    fn execute(
        &self,
        plugin: &LoadedPlugin,
        request: &PluginRequest,
    ) -> Result<PluginResponse, RuntimeError> {
        let request_json = serde_json::to_string(request).map_err(|err| {
            RuntimeError::InvalidResponse(format!("failed to encode request: {err}"))
        })?;

        match &self.backend {
            Backend::InProcess { runs } => catch_interpreter_panic(|| {
                self.execute_in_process(plugin, request_json, runs, None)
            })?,
            #[cfg(feature = "plugins-monty-pool")]
            Backend::Pool(state) => self.execute_pool(plugin, request_json, state),
        }
    }
}

/// An invocation borrows only its own plugin's selected index. Keys have no
/// interpretation outside that JSON value, and every reply is a fresh copy.
fn lookup_index(
    index: &serde_json::Value,
    args: &monty_types::CallArgs,
    limit: usize,
) -> Result<String, RuntimeError> {
    let invalid = || RuntimeError::Trap("invalid project index lookup".into());
    if args.args().len() != 2 || args.kwargs().len() != 0 {
        return Err(invalid());
    }
    let operation = args
        .arg(0)
        .and_then(|value| value.as_str())
        .ok_or_else(invalid)?;
    let path = args
        .arg(1)
        .and_then(|value| value.as_str())
        .ok_or_else(invalid)?;
    if path.len() > 64 * 1024 {
        return Err(invalid());
    }
    let path: Vec<String> = serde_json::from_str(path).map_err(|_| invalid())?;
    if path.len() > 128 {
        return Err(invalid());
    }
    let value = path
        .iter()
        .try_fold(index, |value, key| value.as_object()?.get(key));
    let result = match operation {
        "value" => serde_json::to_string(&(value.is_some(), value)),
        "keys" => serde_json::to_string(
            &value
                .and_then(serde_json::Value::as_object)
                .map(|value| value.keys().collect::<Vec<_>>())
                .unwrap_or_default(),
        ),
        _ => return Err(invalid()),
    }
    .map_err(|err| RuntimeError::InvalidResponse(err.to_string()))?;
    if result.len() > limit {
        return Err(RuntimeError::Trap(
            "project index lookup exceeds the response limit".into(),
        ));
    }
    Ok(result)
}

/// Unwinding invalidates the locked interpreter cache. Later calls reject its poisoned mutex
/// rather than reusing interpreter state that may have been partially mutated.
/// Native aborts, including stack overflow and allocator failure, cannot be caught here.
fn catch_interpreter_panic<T>(f: impl FnOnce() -> T) -> Result<T, RuntimeError> {
    catch_unwind(AssertUnwindSafe(f))
        .map_err(|_| RuntimeError::Trap("Monty interpreter panicked".to_string()))
}

/// Resolve the `monty` worker binary: `TY_MONTY_BIN` wins, then `monty` on `PATH` (which is where
/// the `pydantic-monty-runtime` wheel installs it).
#[cfg(feature = "plugins-monty-pool")]
fn resolve_monty_binary() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os(MONTY_BIN_ENV) {
        return Some(std::path::PathBuf::from(path));
    }
    which::which("monty").ok()
}

#[cfg(feature = "plugins-monty-pool")]
fn map_pool_error(err: PoolError) -> RuntimeError {
    match err {
        PoolError::Timeout { .. } => RuntimeError::Timeout,
        PoolError::Runtime(exc) => RuntimeError::Trap(exc.to_string()),
        other => RuntimeError::Trap(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Backend, MontyLimits, MontyRunner, catch_interpreter_panic};
    use crate::{LoadedPlugin, PluginRunner};
    use ty_plugin_protocol::PluginRequest;
    use ty_plugin_sdk::ManifestBuilder;

    #[test]
    fn interpreter_panic_invalidates_cached_state() {
        let mut runner = MontyRunner::new(MontyLimits::default()).expect("runner builds");
        runner
            .add_plugin(
                "example",
                "def __ty_handle__(request):\n    return json.dumps(no_change())",
            )
            .expect("plugin compiles");
        let plugin = LoadedPlugin {
            manifest: ManifestBuilder::new("example", "Example", "0.1.0").build(),
        };
        runner
            .execute(&plugin, &PluginRequest::Manifest)
            .expect("interpreter works before panic");
        let runs = match &runner.backend {
            Backend::InProcess { runs } => runs,
            #[cfg(feature = "plugins-monty-pool")]
            Backend::Pool(_) => panic!("expected embedded interpreter"),
        };
        let error = catch_interpreter_panic(|| {
            let _guard = runs.lock().expect("cache is valid before panic");
            panic!("simulated interpreter panic");
        })
        .expect_err("interpreter panic is contained");
        assert!(error.to_string().contains("interpreter panicked"));
        let error = runner
            .execute(&plugin, &PluginRequest::Manifest)
            .expect_err("invalidated state is rejected");
        assert!(error.to_string().contains("invalidated by a panic"));
    }
}
