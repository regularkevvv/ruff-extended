//! Authoring SDK for `ty` semantic extensions.
//!
//! This crate is the ergonomic front door for extension authors. It depends only on
//! [`ty_plugin_protocol`] — never on the checker internals (`ty_python_semantic`, Salsa, the AST,
//! or `ty_plugin_host`) — so an extension built against it stays decoupled from `ty`'s implementation
//! and can eventually target the WASM runtime unchanged.
//!
//! It provides three things:
//!
//! - [`ManifestBuilder`], a fluent builder that fills in protocol defaults and keeps an extension's
//!   declared capabilities in sync with its claims.
//! - The [`Plugin`] trait, which turns a set of typed hook methods into a single
//!   [`Plugin::handle`] request dispatcher (and a [`Plugin::handle_json`] wire entry point that a
//!   host transport can call).
//! - The [`dsl`] module of small constructors for parameters, fields, signatures, and responses.
//!
//! The raw protocol types remain available through the re-exported [`protocol`] module for
//! anything the helpers do not cover.

pub use serde_json;
pub use ty_plugin_protocol as protocol;

pub mod dsl;
mod manifest;
#[cfg(target_arch = "wasm32")]
pub mod wasm;

pub use manifest::ManifestBuilder;

use ty_plugin_protocol::{
    AnalyzeClassRequest, BuildProjectIndexRequest, CallRequest, DependencyRequest, PluginManifest,
    PluginRequest, PluginResponse, ResolveMemberRequest,
};

/// A semantic extension: a manifest plus the hooks it chooses to implement.
///
/// Implementors provide [`Plugin::manifest`] and override only the hook methods matching the
/// capabilities they declared. Every hook defaults to [`PluginResponse::NoChange`], so an extension
/// only writes code for the behavior it actually contributes. [`Plugin::handle`] then routes an
/// incoming [`PluginRequest`] to the right hook, giving an extension a single, uniform entry point
/// that mirrors how a runtime backend invokes it.
pub trait Plugin {
    /// The extension's manifest, describing its identity, capabilities, and claims.
    fn manifest(&self) -> PluginManifest;

    /// Hook for the `class-transform` capability: adjust a claimed class's fields, members, or
    /// constructor.
    fn analyze_class(&self, request: &AnalyzeClassRequest) -> PluginResponse {
        let _ = request;
        PluginResponse::NoChange
    }

    /// Hook for the `project-index` capability: build extension-owned project data and cross-symbol
    /// contributions from host-provided class and settings summaries.
    fn build_project_index(&self, request: &BuildProjectIndexRequest) -> PluginResponse {
        let _ = request;
        PluginResponse::NoChange
    }

    /// Hook for the `class-member` capability: resolve a claimed class-scope member.
    fn resolve_class_member(&self, request: &ResolveMemberRequest) -> PluginResponse {
        let _ = request;
        PluginResponse::NoChange
    }

    /// Hook for the `instance-member` capability: resolve a claimed instance-scope member.
    fn resolve_instance_member(&self, request: &ResolveMemberRequest) -> PluginResponse {
        let _ = request;
        PluginResponse::NoChange
    }

    /// Hook for the `call-signature` capability: replace the signature bound at a claimed call
    /// site before argument checking.
    fn adjust_call_signature(&self, request: &CallRequest) -> PluginResponse {
        let _ = request;
        PluginResponse::NoChange
    }

    /// Hook for the `call-return` capability: override the return type of a claimed call.
    fn adjust_call_return(&self, request: &CallRequest) -> PluginResponse {
        let _ = request;
        PluginResponse::NoChange
    }

    /// Hook for the `additional-dependencies` capability: declare extra files whose contents feed
    /// the plugin's fingerprint.
    fn additional_dependencies(&self, request: &DependencyRequest) -> PluginResponse {
        let _ = request;
        PluginResponse::NoChange
    }

    /// Route a decoded [`PluginRequest`] to the matching hook.
    ///
    /// A [`PluginRequest::Manifest`] request is answered from [`Plugin::manifest`]; every other
    /// variant dispatches to its hook method.
    fn handle(&self, request: &PluginRequest) -> PluginResponse {
        match request {
            PluginRequest::Manifest => PluginResponse::Manifest(self.manifest()),
            PluginRequest::BuildProjectIndex(request) => self.build_project_index(request),
            PluginRequest::AnalyzeClass(request) => self.analyze_class(request),
            PluginRequest::ResolveClassMember(request) => self.resolve_class_member(request),
            PluginRequest::ResolveInstanceMember(request) => self.resolve_instance_member(request),
            PluginRequest::AdjustCallSignature(request) => self.adjust_call_signature(request),
            PluginRequest::AdjustCallReturn(request) => self.adjust_call_return(request),
            PluginRequest::AdditionalDependencies(request) => self.additional_dependencies(request),
        }
    }

    /// Decode a JSON request, [`handle`](Plugin::handle) it, and encode the JSON response.
    ///
    /// This is the shape a wire transport (subprocess or WASM) wraps: bytes in, bytes out.
    fn handle_json(&self, request_json: &str) -> Result<String, DispatchError> {
        let request = serde_json::from_str::<PluginRequest>(request_json)
            .map_err(DispatchError::DecodeRequest)?;
        let response = self.handle(&request);
        serde_json::to_string(&response).map_err(DispatchError::EncodeResponse)
    }
}

/// An error from the JSON dispatch path of [`Plugin::handle_json`].
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// The incoming request bytes were not a valid [`PluginRequest`].
    #[error("failed to decode plugin request: {0}")]
    DecodeRequest(#[source] serde_json::Error),
    /// The produced response could not be serialized.
    #[error("failed to encode plugin response: {0}")]
    EncodeResponse(#[source] serde_json::Error),
}

/// Export a [`Plugin`] as a WASM extension artifact.
///
/// Invoke this once at the root of a `cdylib` crate, passing an expression that evaluates to a
/// [`Plugin`]. On `wasm32` targets it generates the two C-ABI exports the host's WASM runtime
/// calls — `ty_plugin_alloc` and `ty_plugin_handle` — wired to [`wasm::alloc`] and [`wasm::handle`].
/// On every other target it expands to nothing, so the extension crate still builds for the host.
///
/// ```ignore
/// ty_plugin_sdk::export_plugin!(my_crate::MyPlugin::default());
/// ```
#[macro_export]
macro_rules! export_plugin {
    ($plugin:expr) => {
        /// Reserve `len` bytes in linear memory for the host to write a request into.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn ty_plugin_alloc(len: u32) -> u32 {
            $crate::wasm::alloc(len)
        }

        /// Handle the JSON request at `[ptr, ptr + len)` and return a packed
        /// `(response_ptr << 32) | response_len` pointing at the JSON response.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn ty_plugin_handle(ptr: u32, len: u32) -> u64 {
            $crate::wasm::handle(&$plugin, ptr, len)
        }
    };
}
