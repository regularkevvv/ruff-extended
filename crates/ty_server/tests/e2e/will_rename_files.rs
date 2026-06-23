use std::collections::HashMap;

use lsp_types::{FileRename, TextEdit, Uri};
use ruff_db::system::SystemPath;

use crate::notebook::NotebookBuilder;
use crate::{TestServer, TestServerBuilder};

#[test]
fn batch_file_and_regular_package_directory() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("old_module.py", "x = 1\n")?
        .with_file("old_pkg/__init__.py", "")?
        .with_file("old_pkg/sub.py", "x = 2\n")?
        .with_file(
            "consumer.py",
            "import old_module\nimport old_pkg.sub as sub\nprint(old_module.x, sub.x)\n",
        )?
        .build()
        .wait_until_workspaces_are_initialized();

    let mut changes = rename_changes(
        &mut server,
        &[("old_module.py", "new_module.py"), ("old_pkg", "new_pkg")],
    );
    let edits = changes
        .remove(&server.file_uri("consumer.py"))
        .expect("batch to edit the consumer");

    assert_eq!(
        replacement_texts(&edits),
        ["new_module", "new_pkg.sub", "new_module"]
    );
    assert!(changes.is_empty());

    Ok(())
}

#[test]
fn batch_spanning_workspaces_is_declined() -> anyhow::Result<()> {
    let workspace_a = SystemPath::new("repo/a");
    let workspace_b = SystemPath::new("repo/b");
    let nested_workspace = SystemPath::new("repo/a/pkg/nested");
    let mut server = TestServerBuilder::new()?
        .with_file("repo/pyproject.toml", "[tool.ty]\n")?
        .with_workspace(workspace_a, None)?
        .with_file("repo/a/pkg/__init__.py", "")?
        .with_file("repo/a/use_pkg.py", "import a.pkg\nprint(a.pkg)\n")?
        .with_workspace(nested_workspace, None)?
        .with_file("repo/a/old_a.py", "x = 1\n")?
        .with_file(
            "repo/a/use_a.py",
            "import a.old_a as value\nprint(value.x)\n",
        )?
        .with_workspace(workspace_b, None)?
        .with_file("repo/b/old_b.py", "x = 2\n")?
        .with_file(
            "repo/b/use_b.py",
            "import b.old_b as value\nprint(value.x)\n",
        )?
        .with_file("repo/outside.py", "x = 3\n")?
        .with_file(
            "repo/a/use_outside.py",
            "import outside\nprint(outside.x)\n",
        )?
        .build()
        .wait_until_workspaces_are_initialized();

    assert!(
        rename_edit(
            &mut server,
            &[
                ("repo/a/old_a.py", "repo/a/new_a.py"),
                ("repo/b/old_b.py", "repo/b/new_b.py"),
            ],
        )
        .is_none()
    );
    assert!(rename_edit(&mut server, &[("repo/a/old_a.py", "repo/b/new_a.py")]).is_none());
    assert!(rename_edit(&mut server, &[("repo/outside.py", "repo/outside_new.py")]).is_none());
    assert!(rename_edit(&mut server, &[("repo/a/pkg", "repo/a/new_pkg")]).is_none());

    Ok(())
}

#[test]
fn non_python_entries_are_ignored_but_python_failures_are_atomic() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("old.py", "x = 1\n")?
        .with_file("notes.txt", "notes\n")?
        .with_file("assets/readme.txt", "notes\n")?
        .with_file("consumer.py", "import old\nprint(old.x)\n")?
        .build()
        .wait_until_workspaces_are_initialized();

    let mut changes = rename_changes(
        &mut server,
        &[
            ("notes.txt", "renamed.txt"),
            ("assets", "renamed_assets"),
            ("old.py", "new.py"),
        ],
    );
    let edits = changes
        .remove(&server.file_uri("consumer.py"))
        .expect("the Python rename to edit its consumer");
    assert_eq!(replacement_texts(&edits), ["new", "new"]);
    assert!(changes.is_empty());
    assert!(
        rename_edit(
            &mut server,
            &[("old.py", "old.txt"), ("notes.txt", "other.txt")],
        )
        .is_none()
    );

    Ok(())
}

#[test]
fn namespace_package_directory_declines_batch() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("old.py", "x = 1\n")?
        .with_file("oldns/mod.py", "x = 2\n")?
        .with_file(
            "consumer.py",
            "import old\nimport oldns.mod\nprint(old.x, oldns.mod.x)\n",
        )?
        .build()
        .wait_until_workspaces_are_initialized();

    assert!(rename_edit(&mut server, &[("old.py", "new.py"), ("oldns", "newns")]).is_none());

    Ok(())
}

#[test]
fn open_files_are_candidates() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("old_pkg/__init__.py", "")?
        .with_file("old_pkg/sub.py", "x = 1\n")?
        .build()
        .wait_until_workspaces_are_initialized();

    let mut notebook = NotebookBuilder::virtual_file("consumer.ipynb");
    let first_cell_uri = notebook.add_python_cell("import old_pkg.sub as sub\nprint(sub.x)\n");
    let second_cell_uri = notebook.add_python_cell("import old_pkg.sub as sub\nprint(sub.x)\n");
    notebook.open(&mut server);
    server.collect_publish_diagnostic_notifications(2);

    let mut changes = rename_changes(&mut server, &[("old_pkg", "new_pkg")]);
    for uri in [first_cell_uri, second_cell_uri] {
        let edits = changes.remove(&uri).expect("candidate to receive edits");
        assert_eq!(replacement_texts(&edits), ["new_pkg.sub"]);
    }
    assert!(changes.is_empty());

    Ok(())
}

fn rename_changes(
    server: &mut TestServer,
    renames: &[(&str, &str)],
) -> HashMap<Uri, Vec<TextEdit>> {
    rename_edit(server, renames)
        .and_then(|edit| edit.changes)
        .expect("rename to produce workspace changes")
}

fn rename_edit(
    server: &mut TestServer,
    renames: &[(&str, &str)],
) -> Option<lsp_types::WorkspaceEdit> {
    let renames = renames
        .iter()
        .map(|(old, new)| FileRename {
            old_uri: server.file_uri(old).to_string(),
            new_uri: server.file_uri(new).to_string(),
        })
        .collect();
    server.will_rename_files(renames)
}

fn replacement_texts(edits: &[TextEdit]) -> Vec<&str> {
    edits.iter().map(|edit| edit.new_text.as_str()).collect()
}
