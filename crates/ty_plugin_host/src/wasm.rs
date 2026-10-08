//! Wasmtime-backed [`PluginRunner`] that executes real `.wasm` plugin artifacts.
//!
//! The ABI is JSON over exported linear memory, matching the SDK's `handle_json` entry point. For
//! each call the runner:
//!
//! 1. serializes the [`PluginRequest`] to JSON,
//! 2. calls the guest's `ty_plugin_alloc` to reserve a buffer and writes the request into it,
//! 3. calls the guest's `ty_plugin_handle`, which returns a packed `(ptr << 32) | len` locating the
//!    JSON response in linear memory,
//! 4. reads and deserializes the response back into a [`PluginResponse`].
//!
//! Each call runs in a fresh [`Store`] with a fuel budget (a deterministic step bound standing in
//! for a timeout), a memory ceiling, and a response-size cap. No WASI is provided, so a plugin has
//! no access to the filesystem, environment, clock, or network — only the two functions above.

use std::collections::BTreeMap;
use std::path::Path;

use wasmtime::{
    Cache, CacheConfig, Config, Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder,
    Trap, TypedFunc,
};

use ty_plugin_protocol::{PluginRequest, PluginResponse};

use crate::{LoadedPlugin, PluginRunner, RuntimeError};

/// Resource bounds enforced on every plugin call.
#[derive(Debug, Clone, Copy)]
pub struct WasmLimits {
    /// Fuel budget per call. Exhausting it aborts the call as a [`RuntimeError::Timeout`], giving a
    /// deterministic bound on runaway plugins (unlike wall-clock time, fuel is reproducible).
    pub fuel: u64,
    /// Maximum linear-memory size, in bytes, a plugin may grow to during a call.
    pub max_memory_bytes: usize,
    /// Maximum accepted response size, in bytes. A larger response is rejected rather than decoded.
    pub max_response_bytes: usize,
}

impl Default for WasmLimits {
    fn default() -> Self {
        Self {
            fuel: 1_000_000_000,
            max_memory_bytes: 64 * 1024 * 1024,
            max_response_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Per-call store state carrying the memory limiter.
struct StoreState {
    limits: StoreLimits,
}

/// A [`PluginRunner`] that executes plugins compiled to WebAssembly through wasmtime.
///
/// Modules are compiled once via [`WasmRunner::with_plugin`] and reused; each [`execute`] call gets
/// a fresh, isolated [`Store`], so plugins hold no state across calls (which keeps results
/// deterministic and cache-friendly for the checker).
///
/// [`execute`]: PluginRunner::execute
pub struct WasmRunner {
    engine: Engine,
    modules: BTreeMap<String, Module>,
    limits: WasmLimits,
    cache: Option<Cache>,
}

impl WasmRunner {
    /// Create a runner whose calls are bounded by `limits`.
    pub fn new(limits: WasmLimits) -> Result<Self, RuntimeError> {
        Self::new_with_cache(limits, None)
    }

    /// Cache compiled native modules across runners in a trusted, absolute user cache directory.
    ///
    /// Wasmtime keys entries by module contents, compiler version, target and engine settings.
    /// Unavailable caches fall back to compilation. Relative paths are ignored because a project
    /// directory is not a trusted source of native executable code.
    pub fn new_with_cache(
        limits: WasmLimits,
        directory: Option<&Path>,
    ) -> Result<Self, RuntimeError> {
        let cache = directory.filter(|path| path.is_absolute()).and_then(|directory| {
            let mut config = CacheConfig::new();
            config.with_directory(directory);
            match Cache::new(config) {
                Ok(cache) => Some(cache),
                Err(error) => {
                    tracing::warn!(%error, "WASM compilation cache unavailable; compiling without it");
                    None
                }
            }
        });
        let mut config = Config::new();
        config.consume_fuel(true);
        config.cache(cache.clone());
        let engine =
            Engine::new(&config).map_err(|err| RuntimeError::Trap(engine_message(&err)))?;
        Ok(Self {
            engine,
            modules: BTreeMap::new(),
            limits,
            cache,
        })
    }

    /// Compile a plugin's WASM artifact (binary bytes or `.wat` text) and register it under its id.
    pub fn with_plugin(
        mut self,
        plugin_id: impl Into<String>,
        wasm: impl AsRef<[u8]>,
    ) -> Result<Self, RuntimeError> {
        self.add_plugin(plugin_id, wasm)?;
        Ok(self)
    }

    /// Compile a plugin's WASM artifact (binary bytes or `.wat` text) and register it under its id.
    pub fn add_plugin(
        &mut self,
        plugin_id: impl Into<String>,
        wasm: impl AsRef<[u8]>,
    ) -> Result<(), RuntimeError> {
        let plugin_id = plugin_id.into();
        let (hits, misses) = self.cache_counts();
        let module = Module::new(&self.engine, wasm)
            .map_err(|err| RuntimeError::Trap(format!("failed to compile plugin module: {err}")))?;
        let (new_hits, new_misses) = self.cache_counts();
        tracing::debug!(
            plugin = %plugin_id,
            cache_enabled = self.cache.is_some(),
            cache_hits = new_hits - hits,
            cache_misses = new_misses - misses,
            "Loaded WASM plugin module"
        );
        self.modules.insert(plugin_id, module);
        Ok(())
    }

    fn cache_counts(&self) -> (usize, usize) {
        self.cache
            .as_ref()
            .map_or((0, 0), |cache| (cache.cache_hits(), cache.cache_misses()))
    }
}

impl PluginRunner for WasmRunner {
    fn execute(
        &self,
        plugin: &LoadedPlugin,
        request: &PluginRequest,
    ) -> Result<PluginResponse, RuntimeError> {
        let module = self
            .modules
            .get(plugin.id())
            .ok_or(RuntimeError::UnsupportedRuntime(
                "wasm plugin artifact was not loaded into the runner",
            ))?;

        let request_json = serde_json::to_vec(request).map_err(|err| {
            RuntimeError::InvalidResponse(format!("failed to encode request: {err}"))
        })?;
        let request_len = u32::try_from(request_json.len())
            .map_err(|_| RuntimeError::InvalidResponse("request exceeds 4 GiB".to_string()))?;

        let state = StoreState {
            limits: StoreLimitsBuilder::new()
                .memory_size(self.limits.max_memory_bytes)
                .build(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|err| RuntimeError::Trap(engine_message(&err)))?;

        let instance = Instance::new(&mut store, module, &[])
            .map_err(|err| classify_call_error(&err, &mut store))?;

        let memory = instance.get_memory(&mut store, "memory").ok_or_else(|| {
            RuntimeError::InvalidResponse("plugin does not export `memory`".into())
        })?;
        let alloc: TypedFunc<u32, u32> = instance
            .get_typed_func(&mut store, "ty_plugin_alloc")
            .map_err(|err| {
                RuntimeError::InvalidResponse(format!("plugin export `ty_plugin_alloc`: {err}"))
            })?;
        let handle: TypedFunc<(u32, u32), u64> = instance
            .get_typed_func(&mut store, "ty_plugin_handle")
            .map_err(|err| {
                RuntimeError::InvalidResponse(format!("plugin export `ty_plugin_handle`: {err}"))
            })?;

        let request_ptr = alloc
            .call(&mut store, request_len)
            .map_err(|err| classify_call_error(&err, &mut store))?;
        memory
            .write(&mut store, request_ptr as usize, &request_json)
            .map_err(|err| {
                RuntimeError::Trap(format!("failed to write request into memory: {err}"))
            })?;

        let packed = handle
            .call(&mut store, (request_ptr, request_len))
            .map_err(|err| classify_call_error(&err, &mut store))?;
        let response_ptr = (packed >> 32) as usize;
        let response_len = (packed & 0xffff_ffff) as usize;

        if response_len > self.limits.max_response_bytes {
            return Err(RuntimeError::ResponseTooLarge);
        }

        let data = memory.data(&store);
        let bytes = data
            .get(response_ptr..response_ptr.saturating_add(response_len))
            .ok_or_else(|| {
                RuntimeError::InvalidResponse("response pointer out of bounds".into())
            })?;

        serde_json::from_slice(bytes).map_err(|err| {
            RuntimeError::InvalidResponse(format!("response is not valid JSON: {err}"))
        })
    }
}

/// Classify a failed guest call: fuel exhaustion becomes a [`RuntimeError::Timeout`], any other trap
/// becomes a [`RuntimeError::Trap`] carrying the trap description.
fn classify_call_error(err: &wasmtime::Error, store: &mut Store<StoreState>) -> RuntimeError {
    let trap = err.downcast_ref::<Trap>();
    let out_of_fuel = matches!(store.get_fuel(), Ok(0))
        || trap.is_some_and(|trap| trap.to_string().contains("fuel"));

    if out_of_fuel {
        return RuntimeError::Timeout;
    }

    match trap {
        Some(trap) => RuntimeError::Trap(trap.to_string()),
        None => RuntimeError::Trap(err.to_string()),
    }
}

/// Render a non-trap engine error (config/fuel setup) as a message.
fn engine_message(err: &wasmtime::Error) -> String {
    err.to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::thread;

    use tempfile::TempDir;
    use wasmtime::{Cache, CacheConfig, Config, Engine, Instance, Module, Store};

    use super::{WasmLimits, WasmRunner};

    const MODULE: &str = "(module (func (export \"value\") (result i32) i32.const 7))";

    fn runner(directory: &Path, source: &str) -> WasmRunner {
        WasmRunner::new_with_cache(WasmLimits::default(), Some(directory))
            .expect("engine builds")
            .with_plugin("example", source)
            .expect("module loads")
    }

    fn value(runner: &WasmRunner) -> i32 {
        let mut store = Store::new(&runner.engine, ());
        store.set_fuel(1000).expect("fuel is enabled");
        let instance = Instance::new(&mut store, &runner.modules["example"], &[])
            .expect("module instantiates");
        instance
            .get_typed_func::<(), i32>(&mut store, "value")
            .expect("export exists")
            .call(&mut store, ())
            .expect("export runs")
    }

    #[test]
    fn compiled_code_survives_runner_restart_and_preserves_fuel() {
        let directory = TempDir::new().expect("temporary cache");
        let cold = runner(directory.path(), MODULE);
        assert_eq!(cold.cache_counts(), (0, 1));
        assert_eq!(value(&cold), 7);
        drop(cold);

        let warm = runner(directory.path(), MODULE);
        assert_eq!(warm.cache_counts(), (1, 0));
        assert_eq!(value(&warm), 7);
        let mut store = Store::new(&warm.engine, ());
        store.set_fuel(0).expect("fuel is enabled");
        let instance =
            Instance::new(&mut store, &warm.modules["example"], &[]).expect("module instantiates");
        assert!(
            instance
                .get_typed_func::<(), i32>(&mut store, "value")
                .expect("export exists")
                .call(&mut store, ())
                .is_err()
        );
    }

    #[test]
    fn module_changes_invalidate_compiled_code() {
        let directory = TempDir::new().expect("temporary cache");
        assert_eq!(value(&runner(directory.path(), MODULE)), 7);
        let changed = runner(directory.path(), &MODULE.replace("const 7", "const 9"));
        assert_eq!(changed.cache_counts(), (0, 1));
        assert_eq!(value(&changed), 9);
        let restored = runner(directory.path(), MODULE);
        assert_eq!(restored.cache_counts(), (1, 0));
        assert_eq!(value(&restored), 7);
    }

    #[test]
    fn engine_settings_invalidate_compiled_code() {
        let directory = TempDir::new().expect("temporary cache");
        drop(runner(directory.path(), MODULE));
        let mut cache_config = CacheConfig::new();
        cache_config.with_directory(directory.path());
        let cache = Cache::new(cache_config).expect("cache builds");
        let mut config = Config::new();
        config.cache(Some(cache.clone()));
        // The production engine uses fuel instrumentation; this engine does not.
        let engine = Engine::new(&config).expect("engine builds");
        Module::new(&engine, MODULE).expect("module compiles for changed engine");
        assert_eq!((cache.cache_hits(), cache.cache_misses()), (0, 1));
    }

    #[test]
    fn invalid_cache_entries_are_recompiled() {
        let directory = TempDir::new().expect("temporary cache");
        drop(runner(directory.path(), MODULE));
        let modules =
            fs::read_dir(directory.path().join("modules")).expect("cache contains modules");
        let mut corrupted = 0;
        for compiler in modules {
            for entry in fs::read_dir(compiler.expect("compiler entry").path())
                .expect("compiler cache directory")
            {
                let path = entry.expect("module entry").path();
                if path.is_file() && path.extension().is_none() {
                    fs::write(path, b"invalid compressed entry").expect("corrupt test cache");
                    corrupted += 1;
                }
            }
        }
        assert_eq!(corrupted, 1);
        let recovered = runner(directory.path(), MODULE);
        assert_eq!(recovered.cache_counts(), (0, 1));
        assert_eq!(value(&recovered), 7);
        assert_eq!(runner(directory.path(), MODULE).cache_counts(), (1, 0));
    }

    #[test]
    fn cache_failure_and_relative_paths_keep_compilation_available() {
        let directory = TempDir::new().expect("temporary cache");
        let file = directory.path().join("file");
        fs::write(&file, "not a directory").expect("create file");
        let unavailable = runner(&file, MODULE);
        assert!(unavailable.cache.is_none());
        assert_eq!(value(&unavailable), 7);
        let relative = runner(Path::new("untrusted-project-cache"), MODULE);
        assert!(relative.cache.is_none());
        assert_eq!(value(&relative), 7);
    }

    #[test]
    fn concurrent_runners_share_cache_without_partial_artifacts() {
        let directory = TempDir::new().expect("temporary cache");
        thread::scope(|scope| {
            let threads: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| value(&runner(directory.path(), MODULE))))
                .collect();
            for thread in threads {
                assert_eq!(thread.join().expect("runner finishes"), 7);
            }
        });
        assert_eq!(runner(directory.path(), MODULE).cache_counts(), (1, 0));
    }

    #[test]
    fn plugin_input_cannot_supply_native_artifacts() {
        let uncached = WasmRunner::new(WasmLimits::default()).expect("engine builds");
        let compiled = Module::new(&uncached.engine, MODULE)
            .expect("module compiles")
            .serialize()
            .expect("module serializes");
        let directory = TempDir::new().expect("temporary cache");
        let mut cached = WasmRunner::new_with_cache(WasmLimits::default(), Some(directory.path()))
            .expect("engine builds");
        assert!(cached.add_plugin("untrusted", compiled).is_err());
    }
}
