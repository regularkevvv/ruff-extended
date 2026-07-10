use anyhow::Result;
use insta::assert_json_snapshot;
use lsp_types::{FileChangeType, FileEvent};
use ruff_db::system::SystemPath;
use serde_json::{Map, json};
use ty_server::{ClientOptions, WorkspaceOptions};

use crate::TestServerBuilder;
use crate::pull_diagnostics::filter_result_id;

#[test]
fn configuration_file() -> Result<()> {
    let _filter = filter_result_id();

    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
def foo() -> str:
    return a
";

    let builder = TestServerBuilder::new()?;

    let settings_path = builder.file_path("ty2.toml");

    let mut server = builder
        .with_workspace(
            workspace_root,
            Some(ClientOptions {
                workspace: WorkspaceOptions {
                    configuration_file: Some(settings_path.to_string()),
                    ..WorkspaceOptions::default()
                },
                ..ClientOptions::default()
            }),
        )?
        .with_file(foo, foo_content)?
        .with_file(
            settings_path,
            r#"
[rules]
unresolved-reference="warn"
        "#,
        )?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);
    let diagnostics = server.document_diagnostic_request(foo, None);

    assert_json_snapshot!(diagnostics);

    Ok(())
}

#[test]
fn invalid_configuration_file() -> Result<()> {
    let _filter = filter_result_id();

    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
def foo() -> str:
    return a
";

    let builder = TestServerBuilder::new()?;

    let settings_path = builder.file_path("ty2.toml");

    let mut server = builder
        .with_workspace(
            workspace_root,
            Some(ClientOptions {
                workspace: WorkspaceOptions {
                    configuration_file: Some(settings_path.to_string()),
                    ..WorkspaceOptions::default()
                },
                ..ClientOptions::default()
            }),
        )?
        .with_file(foo, foo_content)?
        .with_file(
            settings_path,
            r#"
[rule]
unresolved-reference="warn"
        "#,
        )?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);
    let show_message = server.await_notification::<lsp_types::ShowMessageNotification>();
    let diagnostics = server.document_diagnostic_request(foo, None);

    assert_json_snapshot!(show_message, @r#"
    {
      "type": 1,
      "message": "Failed to load project for workspace file://<temp_dir>/src. Please refer to the logs for more details."
    }
    "#);
    assert_json_snapshot!(diagnostics);

    Ok(())
}

#[test]
fn plugin_manifest_setting_diagnostics_update_on_watched_file_change() -> Result<()> {
    let workspace_root = SystemPath::new("project");
    let ty_toml = SystemPath::new("project/ty.toml");
    let manifest = SystemPath::new("project/.ty/plugins/pydantic.plugin.json");

    let mut server = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(
            ty_toml,
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "pydantic"
            path = ".ty/plugins/pydantic.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/pydantic.plugin.json"
            "#,
        )?
        .with_file("project/.ty/plugins/pydantic.mock", "plugin artifact")?
        .with_file(manifest, "{")?
        .build()
        .wait_until_workspaces_are_initialized();

    let diagnostics = server.collect_publish_diagnostic_notifications(1);
    assert_json_snapshot!(diagnostics, @r#"
    {
      "file://<temp_dir>/project/ty.toml": [
        {
          "range": {
            "start": {
              "line": 8,
              "character": 28
            },
            "end": {
              "line": 8,
              "character": 62
            }
          },
          "severity": 1,
          "code": "plugin-configuration",
          "source": "ty",
          "message": "Failed to parse manifest for plugin `pydantic`: EOF while parsing an object at line 1 column 1"
        }
      ]
    }
    "#);

    server.write_file(
        manifest,
        r#"{
            "id": "pydantic",
            "name": "Pydantic plugin",
            "version": "0.1.0",
            "protocol-version": { "major": 0, "minor": 1 },
            "ty-compatibility": { "requirement": ">=0.0.0" },
            "runtime": { "kind": "mock" }
        }"#,
    )?;

    server.did_change_watched_files(vec![FileEvent {
        uri: server.file_uri(manifest),
        kind: FileChangeType::Changed,
    }]);

    let diagnostics = server.collect_publish_diagnostic_notifications(1);
    assert_json_snapshot!(diagnostics, @r#"
    {
      "file://<temp_dir>/project/ty.toml": []
    }
    "#);

    Ok(())
}

#[test]
fn plugin_stub_overlay_diagnostics_update_on_watched_file_change() -> Result<()> {
    let _filter = filter_result_id();

    let workspace_root = SystemPath::new("project");
    let main = SystemPath::new("project/main.py");
    let ty_toml = SystemPath::new("project/ty.toml");
    let overlay = SystemPath::new("project/.ty/plugins/stubs/foo/__init__.pyi");
    let main_content = "from foo import make\nvalue: int = make()\n";

    let mut server = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(main, main_content)?
        .with_file(
            ty_toml,
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "overlay"
            path = ".ty/plugins/overlay.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/overlay.plugin.json"
            stub-overlay-path = ".ty/plugins/stubs"
            "#,
        )?
        .with_file("project/.ty/plugins/overlay.mock", "plugin artifact")?
        .with_file(
            "project/.ty/plugins/overlay.plugin.json",
            r#"{
                "id": "overlay",
                "name": "Overlay plugin",
                "version": "0.1.0",
                "protocol-version": { "major": 0, "minor": 1 },
                "ty-compatibility": { "requirement": ">=0.0.0" },
                "runtime": { "kind": "mock" },
                "capabilities": { "stub-overlays": true }
            }"#,
        )?
        .with_file(overlay, "def make() -> int: ...\n")?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(main, main_content, 1);
    let diagnostics = server.document_diagnostic_request(main, None);
    assert_json_snapshot!(diagnostics, @r#"
    {
      "items": [],
      "kind": "full"
    }
    "#);

    server.write_file(overlay, "def make() -> str: ...\n")?;
    server.did_change_watched_files(vec![FileEvent {
        uri: server.file_uri(overlay),
        kind: FileChangeType::Changed,
    }]);

    let diagnostics = server.document_diagnostic_request(main, None);
    assert_json_snapshot!(diagnostics, @r#"
    {
      "resultId": "[RESULT_ID]",
      "items": [
        {
          "range": {
            "start": {
              "line": 1,
              "character": 13
            },
            "end": {
              "line": 1,
              "character": 19
            }
          },
          "severity": 1,
          "code": "invalid-assignment",
          "codeDescription": {
            "href": "https://ty.dev/rules#invalid-assignment"
          },
          "source": "ty",
          "message": "Object of type `str` is not assignable to `int`"
        }
      ],
      "kind": "full"
    }
    "#);

    Ok(())
}

#[test]
fn configuration_overrides() -> Result<()> {
    let _filter = filter_result_id();

    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
def foo() -> str:
    return a
";

    let mut server = TestServerBuilder::new()?
        .with_workspace(
            workspace_root,
            Some(ClientOptions {
                workspace: WorkspaceOptions {
                    configuration: Some(
                        Map::from_iter([(
                            "rules".to_string(),
                            json!({"unresolved-reference": "warn"}),
                        )])
                        .into(),
                    ),
                    ..WorkspaceOptions::default()
                },
                ..ClientOptions::default()
            }),
        )?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);
    let diagnostics = server.document_diagnostic_request(foo, None);

    assert_json_snapshot!(diagnostics);

    Ok(())
}

#[test]
fn unsupported_editor_python_version() -> Result<()> {
    let _filter = filter_result_id();

    let workspace_root = SystemPath::new("src");
    let main = SystemPath::new("src/main.py");
    let python_home = "base/bin";
    let base_python = if cfg!(target_os = "windows") {
        "base/bin/python.exe"
    } else {
        "base/bin/python"
    };
    let python = if cfg!(target_os = "windows") {
        "venv/Scripts/python.exe"
    } else {
        "venv/bin/python"
    };
    let site_packages_foo = if cfg!(target_os = "windows") {
        "venv/Lib/site-packages/foo.py"
    } else {
        "venv/lib/python3.16/site-packages/foo.py"
    };
    // The import proves we still use the editor-selected environment for module resolution even
    // when we ignore its unsupported reported Python version.
    let foo_content = "\
import foo
import sys
from typing_extensions import reveal_type

reveal_type(sys.version_info[:2])
";

    let builder = TestServerBuilder::new()?;
    let python_home = builder.file_path(python_home);
    let sys_prefix = builder.file_path("venv");

    let workspace_options: ClientOptions = serde_json::from_value(json!({
        "pythonExtension": {
            "activeEnvironment": {
                "executable": {
                    "sysPrefix": sys_prefix,
                },
                "version": {
                    "major": 3,
                    "minor": 16,
                }
            }
        }
    }))?;

    let mut server = builder
        .with_workspace(workspace_root, Some(workspace_options))?
        .with_file(main, foo_content)?
        .with_file(base_python, "")?
        .with_file(python, "")?
        .with_file(
            "venv/pyvenv.cfg",
            format!("version_info = 3.16.0\nhome = {python_home}\n"),
        )?
        .with_file(site_packages_foo, "")?
        .build()
        .wait_until_workspaces_are_initialized();

    // The unsupported version inferred from the selected environment surfaces as a
    // settings diagnostic on the environment's `pyvenv.cfg`.
    let diagnostics = server.collect_publish_diagnostic_notifications(1);
    assert_json_snapshot!(diagnostics, @r#"
    {
      "file://<temp_dir>/venv/pyvenv.cfg": [
        {
          "range": {
            "start": {
              "line": 0,
              "character": 15
            },
            "end": {
              "line": 0,
              "character": 21
            }
          },
          "severity": 2,
          "code": "unsupported-python-version",
          "source": "ty",
          "message": "Ignoring unsupported inferred Python version `3.16`; ty will use Python 3.14 instead.\n\ninfo: Expected one of `3.7`, `3.8`, `3.9`, `3.10`, `3.11`, `3.12`, `3.13`, `3.14`, `3.15`.\ninfo: Set `environment.python-version` explicitly to override the inferred version.\ninfo: The version was inferred from your virtual environment metadata."
        }
      ]
    }
    "#);

    server.open_text_document(main, foo_content, 1);
    let diagnostics = server.document_diagnostic_request(main, None);

    assert_json_snapshot!(diagnostics, @r#"
    {
      "resultId": "[RESULT_ID]",
      "items": [
        {
          "range": {
            "start": {
              "line": 4,
              "character": 12
            },
            "end": {
              "line": 4,
              "character": 32
            }
          },
          "severity": 3,
          "code": "revealed-type",
          "source": "ty",
          "message": "Revealed type: `tuple[Literal[3], Literal[14]]`"
        }
      ],
      "kind": "full"
    }
    "#);

    Ok(())
}

#[cfg_attr(windows, ignore = "site-packages layout inference is Unix-only")]
#[test]
fn unsupported_inferred_python_version_setting_diagnostic() -> Result<()> {
    let workspace_root = SystemPath::new("project");
    let main = SystemPath::new("project/main.py");
    let python_home = "base/bin";
    let base_python = if cfg!(target_os = "windows") {
        "base/bin/python.exe"
    } else {
        "base/bin/python"
    };
    let python = if cfg!(target_os = "windows") {
        "project/.venv/Scripts/python.exe"
    } else {
        "project/.venv/bin/python"
    };
    let site_packages = if cfg!(target_os = "windows") {
        "project/.venv/Lib/site-packages/foo.py"
    } else {
        "project/.venv/lib/python3.16/site-packages/foo.py"
    };

    let builder = TestServerBuilder::new()?;
    let python_home = builder.file_path(python_home);

    let mut server = builder
        .with_workspace(workspace_root, None)?
        .with_file(main, "x = 1\n")?
        .with_file(base_python, "")?
        .with_file(python, "")?
        .with_file(
            "project/.venv/pyvenv.cfg",
            format!("home = {python_home}\n"),
        )?
        .with_file(site_packages, "")?
        .build()
        .wait_until_workspaces_are_initialized();

    let diagnostics = server.collect_publish_diagnostic_notifications(1);

    assert_json_snapshot!(diagnostics);

    Ok(())
}

#[cfg_attr(windows, ignore = "site-packages layout inference is Unix-only")]
#[test]
fn unsupported_inferred_python_version_setting_diagnostic_for_system_interpreter() -> Result<()> {
    let workspace_root = SystemPath::new("project");
    let main = SystemPath::new("project/main.py");
    let python = "python/bin/python3.16";
    let site_packages = "python/lib/python3.16/site-packages/foo.py";

    let builder = TestServerBuilder::new()?;
    let sys_prefix = builder.file_path("python");
    let python_uri = builder.file_uri(python);

    let workspace_options: ClientOptions = serde_json::from_value(json!({
        "pythonExtension": {
            "activeEnvironment": {
                "executable": {
                    "uri": python_uri,
                    "sysPrefix": sys_prefix,
                }
            }
        }
    }))?;

    let mut server = builder
        .with_workspace(workspace_root, Some(workspace_options))?
        .with_file(main, "x = 1\n")?
        .with_file(python, "")?
        .with_file(site_packages, "")?
        .build()
        .wait_until_workspaces_are_initialized();

    let diagnostics = server.collect_publish_diagnostic_notifications(1);

    assert_json_snapshot!(diagnostics, @r#"
    {
      "file://<temp_dir>/python/bin/python3.16": [
        {
          "range": {
            "start": {
              "line": 0,
              "character": 0
            },
            "end": {
              "line": 0,
              "character": 0
            }
          },
          "severity": 2,
          "code": "unsupported-python-version",
          "source": "ty",
          "message": "Ignoring unsupported inferred Python version `3.16`; ty will use Python 3.14 instead.\n\ninfo: Expected one of `3.7`, `3.8`, `3.9`, `3.10`, `3.11`, `3.12`, `3.13`, `3.14`, `3.15`.\ninfo: Set `environment.python-version` explicitly to override the inferred version.\ninfo: The version was inferred from the `lib/python3.16/site-packages` directory layout."
        }
      ]
    }
    "#);

    Ok(())
}

#[test]
fn configuration_file_and_overrides() -> Result<()> {
    let _filter = filter_result_id();

    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
def foo() -> str:
    return a
";

    let builder = TestServerBuilder::new()?;

    let settings_path = builder.file_path("ty2.toml");

    let mut server = builder
        .with_workspace(
            workspace_root,
            Some(ClientOptions {
                workspace: WorkspaceOptions {
                    configuration_file: Some(settings_path.to_string()),
                    configuration: Some(
                        Map::from_iter([(
                            "rules".to_string(),
                            json!({"unresolved-reference": "ignore"}),
                        )])
                        .into(),
                    ),
                    ..WorkspaceOptions::default()
                },
                ..ClientOptions::default()
            }),
        )?
        .with_file(foo, foo_content)?
        .with_file(
            settings_path,
            r#"
[rules]
unresolved-reference="warn"
        "#,
        )?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);
    let diagnostics = server.document_diagnostic_request(foo, None);

    assert_json_snapshot!(diagnostics);

    Ok(())
}
