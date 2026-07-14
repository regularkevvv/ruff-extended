use insta_cmd::assert_cmd_snapshot;

#[cfg(feature = "plugins-wasm")]
use ty_plugin_protocol::{
    CallableSignature, ClassPatch, FieldPatch, MemberAccessPatch, MemberPatch, MemberPatchMode,
    Parameter, ParameterKind, PluginResponse, TypeExpr,
};

use crate::CliTest;

#[cfg(feature = "plugins-wasm")]
fn wasm_plugin_returning(response: &PluginResponse) -> anyhow::Result<String> {
    let response = serde_json::to_string(response)?;
    let response_data = serde_json::to_string(&response)?;

    Ok(format!(
        r#"(module
          (memory (export "memory") 1)
          (data (i32.const 0) {response_data})
          (func (export "ty_plugin_alloc") (param i32) (result i32) i32.const 1024)
          (func (export "ty_plugin_handle") (param i32 i32) (result i64) i64.const {response_len}))"#,
        response_len = response.len(),
    ))
}

#[test]
fn cli_config_args_toml_string_basic() -> anyhow::Result<()> {
    let case = CliTest::with_file("test.py", r"print(x)  # [unresolved-reference]")?;

    // Long flag
    assert_cmd_snapshot!(case.command().arg("--warn").arg("unresolved-reference").arg("--config").arg("terminal.error-on-warning=false"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    warning[unresolved-reference]: Name `x` used when not defined
     --> test.py:1:7
      |
    1 | print(x)  # [unresolved-reference]
      |       ^
      |

    Found 1 diagnostic

    ----- stderr -----
    ");

    // Short flag
    assert_cmd_snapshot!(case.command().arg("--warn").arg("unresolved-reference").arg("-c").arg("terminal.error-on-warning=false"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    warning[unresolved-reference]: Name `x` used when not defined
     --> test.py:1:7
      |
    1 | print(x)  # [unresolved-reference]
      |       ^
      |

    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

#[test]
fn cli_config_args_overrides_ty_toml() -> anyhow::Result<()> {
    let case = CliTest::with_files(vec![
        (
            "ty.toml",
            r#"
            [terminal]
            error-on-warning = false
            "#,
        ),
        ("test.py", r"print(x)  # [unresolved-reference]"),
    ])?;

    // Exit code of 0 due to the setting in `ty.toml`
    assert_cmd_snapshot!(case.command().arg("--warn").arg("unresolved-reference"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    warning[unresolved-reference]: Name `x` used when not defined
     --> test.py:1:7
      |
    1 | print(x)  # [unresolved-reference]
      |       ^
      |

    Found 1 diagnostic

    ----- stderr -----
    ");

    // Exit code of 1 because the `ty.toml` setting is overwritten by `--config`
    assert_cmd_snapshot!(case.command().arg("--warn").arg("unresolved-reference").arg("--config").arg("terminal.error-on-warning=true"), @"
    success: false
    exit_code: 1
    ----- stdout -----
    warning[unresolved-reference]: Name `x` used when not defined
     --> test.py:1:7
      |
    1 | print(x)  # [unresolved-reference]
      |       ^
      |

    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

#[test]
fn cli_config_args_later_overrides_earlier() -> anyhow::Result<()> {
    let case = CliTest::with_file("test.py", r"print(x)  # [unresolved-reference]")?;
    assert_cmd_snapshot!(case.command().arg("--warn").arg("unresolved-reference").arg("--config").arg("terminal.error-on-warning=true").arg("--config").arg("terminal.error-on-warning=false"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    warning[unresolved-reference]: Name `x` used when not defined
     --> test.py:1:7
      |
    1 | print(x)  # [unresolved-reference]
      |       ^
      |

    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

#[test]
fn cli_config_args_invalid_option() -> anyhow::Result<()> {
    let case = CliTest::with_file("test.py", r"print(1)")?;
    assert_cmd_snapshot!(case.command().arg("--config").arg("bad-option=true"), @"
    success: false
    exit_code: 2
    ----- stdout -----

    ----- stderr -----
    error: TOML parse error at line 1, column 1
      |
    1 | bad-option=true
      | ^^^^^^^^^^
    unknown field `bad-option`, expected one of `environment`, `src`, `rules`, `terminal`, `analysis`, `plugins`, `overrides`


    Usage: ty <COMMAND>

    For more information, try '--help'.
    ");

    Ok(())
}

#[test]
fn config_file_override() -> anyhow::Result<()> {
    // Set `error-on-warning` to false in the configuration file
    // Explicitly set `--warn unresolved-reference` to ensure the rule warns instead of errors
    let case = CliTest::with_files(vec![
        ("test.py", r"print(x)  # [unresolved-reference]"),
        (
            "ty-override.toml",
            r#"
            [terminal]
            error-on-warning = false
            "#,
        ),
    ])?;

    // Ensure the configuration file is loaded via the CLI argument
    assert_cmd_snapshot!(case.command().arg("--warn").arg("unresolved-reference").arg("--config-file").arg("ty-override.toml"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    warning[unresolved-reference]: Name `x` used when not defined
     --> test.py:1:7
      |
    1 | print(x)  # [unresolved-reference]
      |       ^
      |

    Found 1 diagnostic

    ----- stderr -----
    ");

    // Ensure the configuration file is loaded via the environment variable
    assert_cmd_snapshot!(case.command().arg("--warn").arg("unresolved-reference").env("TY_CONFIG_FILE", "ty-override.toml"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    warning[unresolved-reference]: Name `x` used when not defined
     --> test.py:1:7
      |
    1 | print(x)  # [unresolved-reference]
      |       ^
      |

    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

#[test]
fn invalid_configuration_file() -> anyhow::Result<()> {
    let case = CliTest::with_files([("ty.toml", "x"), ("test.py", "")])?;

    assert_cmd_snapshot!(case.command().arg("--config-file").arg("ty.toml"), @"
    success: false
    exit_code: 2
    ----- stdout -----

    ----- stderr -----
    ty failed
      Cause: Error loading configuration file at <temp_dir>/ty.toml
      Cause: <temp_dir>/ty.toml is not a valid `ty.toml`
      Cause: TOML parse error at line 1, column 2
      |
    1 | x
      |  ^
    key with no value, expected `=`
    ");

    Ok(())
}

#[test]
fn config_file_invalid_plugin_configuration() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        ("test.py", ""),
        (
            "ty.toml",
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "pydantic"
            path = ".ty/plugins/missing.mock"
            runtime = "mock"
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @r#"
    success: false
    exit_code: 1
    ----- stdout -----
    error[plugin-configuration]: Plugin `pydantic` points to an artifact path that does not exist or is not a file
     --> ty.toml:7:8
      |
    7 | path = ".ty/plugins/missing.mock"
      |        ^^^^^^^^^^^^^^^^^^^^^^^^^^ `<temp_dir>/.ty/plugins/missing.mock` does not exist or is not a file
      |

    Found 1 diagnostic

    ----- stderr -----
    "#);

    Ok(())
}

#[test]
fn config_file_plugin_stub_overlay_affects_type_checking() -> anyhow::Result<()> {
    let site_packages_foo = if cfg!(windows) {
        ".venv/Lib/site-packages/foo/__init__.py"
    } else {
        ".venv/lib/python3.13/site-packages/foo/__init__.py"
    };

    let case = CliTest::with_files([
        (
            "ty.toml",
            r#"
            [environment]
            python = ".venv"

            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "overlay"
            path = ".ty/plugins/overlay.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/overlay.plugin.json"
            stub-overlay-path = ".ty/plugins/stubs"
            "#,
        ),
        (".venv/pyvenv.cfg", "home = ./\nversion_info = 3.13.0"),
        (site_packages_foo, "def make():\n    return 1\n"),
        (".ty/plugins/overlay.mock", "plugin artifact"),
        (
            ".ty/plugins/overlay.plugin.json",
            r#"{
                "id": "overlay",
                "name": "Overlay plugin",
                "version": "0.1.0",
                "protocol-version": { "major": 0, "minor": 1 },
                "ty-compatibility": { "requirement": ">=0.0.0" },
                "runtime": { "kind": "mock" },
                "capabilities": { "stub-overlays": true }
            }"#,
        ),
        (
            ".ty/plugins/stubs/foo/__init__.pyi",
            "def make() -> str: ...\n",
        ),
        ("test.py", "from foo import make\nvalue: int = make()\n"),
    ])?;

    assert_cmd_snapshot!(case.command(), @r#"
    success: false
    exit_code: 1
    ----- stdout -----
    error[invalid-assignment]: Object of type `str` is not assignable to `int`
     --> test.py:2:8
      |
    2 | value: int = make()
      |        ---   ^^^^^^ Incompatible value of type `str`
      |        |
      |        Declared type
      |

    Found 1 diagnostic

    ----- stderr -----
    "#);

    Ok(())
}

#[test]
fn config_file_mock_plugin_class_transform_affects_type_checking() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "ty.toml",
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "toy-model"
            path = ".ty/plugins/toy-model.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/toy-model.plugin.json"
            "#,
        ),
        (".ty/plugins/toy-model.mock", "plugin artifact"),
        (
            ".ty/plugins/toy-model.plugin.json",
            r#"{
                "id": "toy-model",
                "name": "Toy model plugin",
                "version": "0.1.0",
                "protocol-version": { "major": 0, "minor": 1 },
                "ty-compatibility": { "requirement": ">=0.0.0" },
                "runtime": { "kind": "mock" },
                "capabilities": { "class-transform": true },
                "claims": {
                    "classes": [
                        { "kind": "exact", "qualified-name": "toy.Model" }
                    ]
                }
            }"#,
        ),
        ("toy.py", "class Model: ...\n"),
        (
            "test.py",
            r#"from toy import Model

class User(Model):
    name: str
    age: int = 0

user = User(name="Ada")
bad_user = User(name=1)
bad_name: int = user.name
"#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @"
    success: false
    exit_code: 1
    ----- stdout -----
    error[invalid-argument-type]: Argument is incorrect
     --> test.py:8:17
      |
    8 | bad_user = User(name=1)
      |                 ^^^^^^ Expected `str`, found `Literal[1]`
      |

    error[invalid-assignment]: Object of type `str` is not assignable to `int`
     --> test.py:9:11
      |
    9 | bad_name: int = user.name
      |           ---   ^^^^^^^^^ Incompatible value of type `str`
      |           |
      |           Declared type
      |

    Found 2 diagnostics

    ----- stderr -----
    ");

    Ok(())
}

#[test]
fn config_file_mock_plugin_member_hooks_affect_type_checking() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "ty.toml",
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "toy-members"
            path = ".ty/plugins/toy-members.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/toy-members.plugin.json"
            "#,
        ),
        (".ty/plugins/toy-members.mock", "plugin artifact"),
        (
            ".ty/plugins/toy-members.plugin.json",
            r#"{
                "id": "toy-members",
                "name": "Toy member plugin",
                "version": "0.1.0",
                "protocol-version": { "major": 0, "minor": 1 },
                "ty-compatibility": { "requirement": ">=0.0.0" },
                "runtime": { "kind": "mock" },
                "capabilities": { "class-member": true, "instance-member": true },
                "claims": {
                    "attributes": [
                        {
                            "kind": "exact",
                            "owner-qualified-name": "toy.Model",
                            "attribute-name": "dynamic_field",
                            "scope": "instance"
                        },
                        {
                            "kind": "exact",
                            "owner-qualified-name": "toy.Model",
                            "attribute-name": "dynamic_class_field",
                            "scope": "class"
                        },
                        {
                            "kind": "exact",
                            "owner-qualified-name": "toy.Existing",
                            "attribute-name": "dynamic_field",
                            "scope": "instance"
                        }
                    ]
                }
            }"#,
        ),
        (
            "toy.py",
            r#"class Model: ...

class Existing:
    dynamic_field: int
"#,
        ),
        (
            "test.py",
            r#"from toy import Existing, Model

model = Model()
dynamic_value: str = model.dynamic_field
bad_dynamic_value: int = model.dynamic_field

class_value: str = Model.dynamic_class_field
bad_class_value: int = Model.dynamic_class_field

existing = Existing()
existing_value: int = existing.dynamic_field
bad_existing_value: str = existing.dynamic_field
"#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @r#"
    success: false
    exit_code: 1
    ----- stdout -----
    error[invalid-assignment]: Object of type `str` is not assignable to `int`
     --> test.py:5:20
      |
    5 | bad_dynamic_value: int = model.dynamic_field
      |                    ---   ^^^^^^^^^^^^^^^^^^^ Incompatible value of type `str`
      |                    |
      |                    Declared type
      |

    error[invalid-assignment]: Object of type `str` is not assignable to `int`
     --> test.py:8:18
      |
    8 | bad_class_value: int = Model.dynamic_class_field
      |                  ---   ^^^^^^^^^^^^^^^^^^^^^^^^^ Incompatible value of type `str`
      |                  |
      |                  Declared type
      |

    error[invalid-assignment]: Object of type `int` is not assignable to `str`
      --> test.py:12:21
       |
    12 | bad_existing_value: str = existing.dynamic_field
       |                     ---   ^^^^^^^^^^^^^^^^^^^^^^ Incompatible value of type `int`
       |                     |
       |                     Declared type
       |

    Found 3 diagnostics

    ----- stderr -----
    "#);

    Ok(())
}

#[test]
fn config_file_mock_plugin_call_hooks_affect_type_checking() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "ty.toml",
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "toy-field"
            path = ".ty/plugins/toy-field.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/toy-field.plugin.json"

            [[plugins.plugin]]
            id = "toy-widget"
            path = ".ty/plugins/toy-widget.mock"
            runtime = "mock"
            manifest-path = ".ty/plugins/toy-widget.plugin.json"
            "#,
        ),
        (".ty/plugins/toy-field.mock", "plugin artifact"),
        (
            ".ty/plugins/toy-field.plugin.json",
            r#"{
                "id": "toy-field",
                "name": "Toy field plugin",
                "version": "0.1.0",
                "protocol-version": { "major": 0, "minor": 1 },
                "ty-compatibility": { "requirement": ">=0.0.0" },
                "runtime": { "kind": "mock" },
                "capabilities": { "call-return": true },
                "claims": {
                    "functions": [
                        { "qualified-name": "toy.Field" }
                    ]
                }
            }"#,
        ),
        (".ty/plugins/toy-widget.mock", "plugin artifact"),
        (
            ".ty/plugins/toy-widget.plugin.json",
            r#"{
                "id": "toy-widget",
                "name": "Toy widget plugin",
                "version": "0.1.0",
                "protocol-version": { "major": 0, "minor": 1 },
                "ty-compatibility": { "requirement": ">=0.0.0" },
                "runtime": { "kind": "mock" },
                "capabilities": { "call-signature": true },
                "claims": {
                    "functions": [
                        { "qualified-name": "toy.Widget" }
                    ]
                }
            }"#,
        ),
        (
            "toy.py",
            r#"def Field(default: object = None) -> str:
    return ""


class Widget: ...
"#,
        ),
        (
            "test.py",
            r#"from toy import Field, Widget

# The call-return hook overrides `Field`'s declared `str` return with `int`.
good_field: int = Field(default=0)
bad_field: str = Field(default=0)

# The call-signature hook adjusts `Widget`'s constructor to require `value: int`.
good_widget: Widget = Widget(value=1)
bad_widget = Widget(value="x")
missing_widget = Widget()
"#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @r#"
    success: false
    exit_code: 1
    ----- stdout -----
    error[invalid-assignment]: Object of type `int` is not assignable to `str`
     --> test.py:5:12
      |
    5 | bad_field: str = Field(default=0)
      |            ---   ^^^^^^^^^^^^^^^^ Incompatible value of type `int`
      |            |
      |            Declared type
      |

    error[invalid-argument-type]: Argument is incorrect
     --> test.py:9:21
      |
    9 | bad_widget = Widget(value="x")
      |                     ^^^^^^^^^ Expected `int`, found `Literal["x"]`
      |

    error[missing-argument]: No argument provided for required parameter `value`
      --> test.py:10:18
       |
    10 | missing_widget = Widget()
       |                  ^^^^^^^^
       |

    Found 3 diagnostics

    ----- stderr -----
    "#);

    Ok(())
}

#[cfg(feature = "plugins-wasm")]
#[test]
fn config_file_wasm_plugin_call_return_affects_type_checking() -> anyhow::Result<()> {
    const WASM_FIELD_PLUGIN: &str = r#"
        (module
          (memory (export "memory") 1)
          (data (i32.const 0) "{\"kind\":\"call-return-patch\",\"return-type\":{\"expression\":\"str\",\"mode\":\"annotation\"}}")
          (func (export "ty_plugin_alloc") (param i32) (result i32) i32.const 1024)
          (func (export "ty_plugin_handle") (param i32 i32) (result i64) i64.const 83))
    "#;
    const TEST_PY: &str = r#"from typing_extensions import reveal_type
from toy import Field

reveal_type(Field(default=0))
"#;

    let without_plugin = CliTest::with_files([
        (
            "toy.py",
            r#"def Field(default: object = None) -> int:
    return 1
"#,
        ),
        ("test.py", TEST_PY),
    ])?;

    assert_cmd_snapshot!(without_plugin.command(), @r#"
    success: true
    exit_code: 0
    ----- stdout -----
    info[revealed-type]: Revealed type
     --> test.py:4:13
      |
    4 | reveal_type(Field(default=0))
      |             ^^^^^^^^^^^^^^^^ `int`
      |

    Found 1 diagnostic

    ----- stderr -----
    "#);

    let with_plugin = CliTest::with_files([
        (
            "ty.toml",
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "toy-field"
            path = ".ty/plugins/toy-field.wasm"
            runtime = "wasm"
            manifest-path = ".ty/plugins/toy-field.plugin.json"
            trusted = true
            "#,
        ),
        (".ty/plugins/toy-field.wasm", WASM_FIELD_PLUGIN),
        (
            ".ty/plugins/toy-field.plugin.json",
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
            }"#,
        ),
        (
            "toy.py",
            r#"def Field(default: object = None) -> int:
    return 1
"#,
        ),
        ("test.py", TEST_PY),
    ])?;

    assert_cmd_snapshot!(with_plugin.command(), @r#"
    success: true
    exit_code: 0
    ----- stdout -----
    info[revealed-type]: Revealed type
     --> test.py:4:13
      |
    4 | reveal_type(Field(default=0))
      |             ^^^^^^^^^^^^^^^^ `str`
      |

    Found 1 diagnostic

    ----- stderr -----
    "#);

    Ok(())
}

#[cfg(feature = "plugins-wasm")]
#[test]
fn config_file_wasm_plugin_call_signature_affects_type_checking() -> anyhow::Result<()> {
    const WASM_WIDGET_PLUGIN: &str = r#"
        (module
          (memory (export "memory") 1)
          (data (i32.const 0) "{\"kind\":\"call-signature-patch\",\"signature\":{\"parameters\":[{\"name\":\"value\",\"kind\":\"keyword-only\",\"type-expr\":{\"expression\":\"int\",\"mode\":\"annotation\"},\"required\":true}],\"return-type\":{\"expression\":\"object\",\"mode\":\"annotation\"}}}")
          (func (export "ty_plugin_alloc") (param i32) (result i32) i32.const 1024)
          (func (export "ty_plugin_handle") (param i32 i32) (result i64) i64.const 226))
    "#;

    let case = CliTest::with_files([
        (
            "ty.toml",
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "toy-widget"
            path = ".ty/plugins/toy-widget.wasm"
            runtime = "wasm"
            manifest-path = ".ty/plugins/toy-widget.plugin.json"
            trusted = true
            "#,
        ),
        (".ty/plugins/toy-widget.wasm", WASM_WIDGET_PLUGIN),
        (
            ".ty/plugins/toy-widget.plugin.json",
            r#"{
                "id": "toy-widget",
                "name": "Toy widget WASM plugin",
                "version": "0.1.0",
                "protocol-version": { "major": 0, "minor": 1 },
                "ty-compatibility": { "requirement": ">=0.0.0" },
                "runtime": { "kind": "wasm", "artifact": ".ty/plugins/toy-widget.wasm" },
                "capabilities": { "call-signature": true },
                "claims": {
                    "functions": [
                        { "qualified-name": "toy.Widget" }
                    ]
                }
            }"#,
        ),
        ("toy.py", "class Widget: ...\n"),
        (
            "test.py",
            r#"from toy import Widget

good_widget: Widget = Widget(value=1)
bad_widget = Widget(value="x")
missing_widget = Widget()
"#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @r#"
    success: false
    exit_code: 1
    ----- stdout -----
    error[invalid-argument-type]: Argument is incorrect
     --> test.py:4:21
      |
    4 | bad_widget = Widget(value="x")
      |                     ^^^^^^^^^ Expected `int`, found `Literal["x"]`
      |

    error[missing-argument]: No argument provided for required parameter `value`
     --> test.py:5:18
      |
    5 | missing_widget = Widget()
      |                  ^^^^^^^^
      |

    Found 2 diagnostics

    ----- stderr -----
    "#);

    Ok(())
}

#[cfg(feature = "plugins-wasm")]
#[test]
fn config_file_wasm_plugin_class_transform_affects_type_checking() -> anyhow::Result<()> {
    let constructor_parameter = Parameter {
        name: Some("name".to_string()),
        kind: ParameterKind::KeywordOnly,
        type_expr: Some(TypeExpr::annotation("str")),
        required: true,
    };
    let wasm_model_plugin = wasm_plugin_returning(&PluginResponse::ClassPatch(ClassPatch {
        fields: vec![FieldPatch {
            name: "name".to_string(),
            mode: MemberPatchMode::FillOnMiss,
            descriptor: None,
            instance_get_type: TypeExpr::annotation("str"),
            instance_set_type: None,
            constructor_parameter: Some(constructor_parameter.clone()),
            has_default: false,
        }],
        class_members: Vec::new(),
        instance_members: Vec::new(),
        constructor: Some(CallableSignature {
            parameters: vec![constructor_parameter],
            return_type: TypeExpr::annotation("object"),
        }),
        diagnostics: Vec::new(),
    }))?;

    let case = CliTest::with_files([
        (
            "ty.toml",
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "toy-model"
            path = ".ty/plugins/toy-model.wasm"
            runtime = "wasm"
            manifest-path = ".ty/plugins/toy-model.plugin.json"
            trusted = true
            "#,
        ),
        (
            ".ty/plugins/toy-model.plugin.json",
            r#"{
                "id": "toy-model",
                "name": "Toy model WASM plugin",
                "version": "0.1.0",
                "protocol-version": { "major": 0, "minor": 1 },
                "ty-compatibility": { "requirement": ">=0.0.0" },
                "runtime": { "kind": "wasm", "artifact": ".ty/plugins/toy-model.wasm" },
                "capabilities": { "class-transform": true },
                "claims": {
                    "classes": [
                        { "kind": "exact", "qualified-name": "toy.Model" }
                    ]
                }
            }"#,
        ),
        ("toy.py", "class Model: ...\n"),
        (
            "test.py",
            r#"from toy import Model

class User(Model):
    pass

user = User(name="Ada")
bad_user = User(name=1)
bad_name: int = user.name
"#,
        ),
    ])?;
    case.write_file(".ty/plugins/toy-model.wasm", &wasm_model_plugin)?;

    assert_cmd_snapshot!(case.command(), @r#"
    success: false
    exit_code: 1
    ----- stdout -----
    error[invalid-argument-type]: Argument is incorrect
     --> test.py:7:17
      |
    7 | bad_user = User(name=1)
      |                 ^^^^^^ Expected `str`, found `Literal[1]`
      |

    error[invalid-assignment]: Object of type `str` is not assignable to `int`
     --> test.py:8:11
      |
    8 | bad_name: int = user.name
      |           ---   ^^^^^^^^^ Incompatible value of type `str`
      |           |
      |           Declared type
      |

    Found 2 diagnostics

    ----- stderr -----
    "#);

    Ok(())
}

#[cfg(feature = "plugins-wasm")]
#[test]
fn config_file_wasm_plugin_member_hooks_affect_type_checking() -> anyhow::Result<()> {
    let wasm_member_plugin = wasm_plugin_returning(&PluginResponse::MemberPatch(MemberPatch {
        name: "dynamic".to_string(),
        mode: MemberPatchMode::FillOnMiss,
        access: MemberAccessPatch::value(TypeExpr::annotation("str")),
        read_only: false,
        diagnostics: Vec::new(),
    }))?;

    let case = CliTest::with_files([
        (
            "ty.toml",
            r#"
            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "toy-members"
            path = ".ty/plugins/toy-members.wasm"
            runtime = "wasm"
            manifest-path = ".ty/plugins/toy-members.plugin.json"
            trusted = true
            "#,
        ),
        (
            ".ty/plugins/toy-members.plugin.json",
            r#"{
                "id": "toy-members",
                "name": "Toy members WASM plugin",
                "version": "0.1.0",
                "protocol-version": { "major": 0, "minor": 1 },
                "ty-compatibility": { "requirement": ">=0.0.0" },
                "runtime": { "kind": "wasm", "artifact": ".ty/plugins/toy-members.wasm" },
                "capabilities": { "instance-member": true },
                "claims": {
                    "attributes": [
                        {
                            "kind": "exact",
                            "owner-qualified-name": "toy.Model",
                            "attribute-name": "dynamic",
                            "scope": "instance"
                        }
                    ]
                }
            }"#,
        ),
        ("toy.py", "class Model: ...\n"),
        (
            "test.py",
            r#"from typing_extensions import reveal_type
from toy import Model

model = Model()
reveal_type(model.dynamic)
bad_dynamic: int = model.dynamic
"#,
        ),
    ])?;
    case.write_file(".ty/plugins/toy-members.wasm", &wasm_member_plugin)?;

    assert_cmd_snapshot!(case.command(), @r#"
    success: false
    exit_code: 1
    ----- stdout -----
    info[revealed-type]: Revealed type
     --> test.py:5:13
      |
    5 | reveal_type(model.dynamic)
      |             ^^^^^^^^^^^^^ `str`
      |

    error[invalid-assignment]: Object of type `str` is not assignable to `int`
     --> test.py:6:14
      |
    6 | bad_dynamic: int = model.dynamic
      |              ---   ^^^^^^^^^^^^^ Incompatible value of type `str`
      |              |
      |              Declared type
      |

    Found 2 diagnostics

    ----- stderr -----
    "#);

    Ok(())
}

#[cfg(feature = "plugins-wasm")]
#[test]
fn config_file_wasm_plugin_runtime_error_falls_back_to_no_change() -> anyhow::Result<()> {
    const CRASHING_FIELD_PLUGIN: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "ty_plugin_alloc") (param i32) (result i32) i32.const 1024)
          (func (export "ty_plugin_handle") (param i32 i32) (result i64) unreachable))
    "#;

    let case = CliTest::with_files([
        (
            "ty.toml",
            r#"
            [terminal]
            error-on-warning = false

            [plugins]
            enabled = true

            [[plugins.plugin]]
            id = "toy-field"
            path = ".ty/plugins/toy-field.wasm"
            runtime = "wasm"
            manifest-path = ".ty/plugins/toy-field.plugin.json"
            trusted = true
            "#,
        ),
        (".ty/plugins/toy-field.wasm", CRASHING_FIELD_PLUGIN),
        (
            ".ty/plugins/toy-field.plugin.json",
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
            }"#,
        ),
        (
            "toy.py",
            r#"def Field(default: object = None) -> int:
    return 1
"#,
        ),
        (
            "test.py",
            r#"from typing_extensions import reveal_type
from toy import Field

reveal_type(Field(default=0))
"#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @r#"
    success: true
    exit_code: 0
    ----- stdout -----
    warning[plugin-configuration]: Plugin `toy-field` failed while handling a semantic hook
     --> test.py:4:13
      |
    4 | reveal_type(Field(default=0))
      |             ^^^^^^^^^^^^^^^^ plugin trapped: wasm trap: wasm `unreachable` instruction executed
      |
    info: The plugin crashed while handling a request. Report this to the plugin author; update or disable the plugin to continue.

    info[revealed-type]: Revealed type
     --> test.py:4:13
      |
    4 | reveal_type(Field(default=0))
      |             ^^^^^^^^^^^^^^^^ `int`
      |

    Found 2 diagnostics

    ----- stderr -----
    "#);

    Ok(())
}
