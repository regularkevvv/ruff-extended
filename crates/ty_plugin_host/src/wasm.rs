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

use wasmtime::{
    Config, Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder, Trap, TypedFunc,
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
}

impl WasmRunner {
    /// Create a runner whose calls are bounded by `limits`.
    pub fn new(limits: WasmLimits) -> Result<Self, RuntimeError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine =
            Engine::new(&config).map_err(|err| RuntimeError::Trap(engine_message(&err)))?;
        Ok(Self {
            engine,
            modules: BTreeMap::new(),
            limits,
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
        let module = Module::new(&self.engine, wasm)
            .map_err(|err| RuntimeError::Trap(format!("failed to compile plugin module: {err}")))?;
        self.modules.insert(plugin_id.into(), module);
        Ok(())
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
