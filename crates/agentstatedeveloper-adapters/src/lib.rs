//! Default language adapter bundle for AgentStateDeveloper.
//!
//! Call [`default_adapters`] to get the full set of built-in adapters.
//! To add a new language: add its crate as a dependency here, instantiate
//! it in `default_adapters`, and add the crate to the workspace.

use std::sync::Arc;

use agentstatedeveloper_core::LanguageAdapter;
use agentstatedeveloper_csharp::CSharpAdapter;
use agentstatedeveloper_go::GoAdapter;
use agentstatedeveloper_java::JavaAdapter;
use agentstatedeveloper_kotlin::KotlinAdapter;
use agentstatedeveloper_python::PythonAdapter;
use agentstatedeveloper_ruby::RubyAdapter;
use agentstatedeveloper_rust::RustAdapter;
use agentstatedeveloper_swift::SwiftAdapter;
use agentstatedeveloper_typescript::TypeScriptAdapter;

/// Return one instance of every built-in language adapter.
///
/// Both the CLI (`asd index`) and the MCP server (`reindex` tool) call this
/// to get a consistent adapter set. Adding a new language means adding one
/// line here — callers need no changes.
pub fn default_adapters() -> Vec<Arc<dyn LanguageAdapter>> {
    vec![
        Arc::new(PythonAdapter::new()),
        Arc::new(TypeScriptAdapter::new()),
        Arc::new(RustAdapter::new()),
        Arc::new(GoAdapter::new()),
        Arc::new(JavaAdapter::new()),
        Arc::new(CSharpAdapter::new()),
        Arc::new(RubyAdapter::new()),
        Arc::new(KotlinAdapter::new()),
        Arc::new(SwiftAdapter::new()),
    ]
}
