use std::collections::HashMap;

use lsp_types::{RenameFilesParams, TextEdit, Uri, WillRenameFilesRequest, WorkspaceEdit};
use ruff_db::Db as _;
use ruff_db::system::{System, SystemPathBuf};
use ty_ide::{PathRename, will_rename_paths};
use ty_project::{Db as _, ProjectDatabase};

use crate::document::ToRangeExt;
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

/// Handles `workspace/willRenameFiles` requests for Python modules and directories.
pub(crate) struct WillRenameFilesHandler;

impl RequestHandler for WillRenameFilesHandler {
    type RequestType = WillRenameFilesRequest;
}

impl BackgroundRequestHandler for WillRenameFilesHandler {
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: RenameFilesParams,
    ) -> crate::server::Result<Option<WorkspaceEdit>> {
        let Some(system) = snapshot.projects().first().map(ProjectDatabase::system) else {
            return Ok(None);
        };
        let mut project_index = None;
        let mut renames = Vec::new();

        for rename in params.files {
            let (Some(old_path), Some(new_path)) = (
                file_uri_to_path(&rename.old_uri),
                file_uri_to_path(&rename.new_uri),
            ) else {
                return Ok(None);
            };

            let (path_rename, is_directory) =
                if let Some(extension @ ("py" | "pyi")) = old_path.extension() {
                    if new_path.extension() != Some(extension) {
                        return Ok(None);
                    }
                    (PathRename::file(old_path.clone(), new_path.clone()), false)
                } else if is_relevant_python_directory(system, &old_path) {
                    (
                        PathRename::directory(old_path.clone(), new_path.clone()),
                        true,
                    )
                } else {
                    continue;
                };

            let Some(owner) = snapshot.enclosing_project_index_for_path(&old_path) else {
                return Ok(None);
            };
            if snapshot.enclosing_project_index_for_path(&new_path) != Some(owner)
                || project_index.is_some_and(|index| index != owner)
                || is_directory && snapshot.path_contains_other_workspace(owner, &old_path)
            {
                return Ok(None);
            }
            project_index.get_or_insert(owner);
            renames.push(path_rename);
        }

        let Some(project_index) = project_index else {
            return Ok(None);
        };
        if snapshot.language_services_disabled(project_index) {
            return Ok(None);
        }

        let db = &snapshot.projects()[project_index];
        let project = db.project();
        let indexed_files = project.files(db);
        let open_files = project.open_files(db);
        let edits = will_rename_paths(
            db,
            &renames,
            (&indexed_files)
                .into_iter()
                .chain(open_files.iter().copied()),
            |file| {
                file.path(db).as_system_path().is_none_or(|path| {
                    snapshot.enclosing_project_index_for_path(path) == Some(project_index)
                })
            },
        );
        let Some(changes) = lsp_edits(db, snapshot.position_encoding(), edits) else {
            return Ok(None);
        };
        Ok((!changes.is_empty()).then_some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }
}

impl RetriableRequestHandler for WillRenameFilesHandler {
    const RETRY_ON_CANCELLATION: bool = true;
}

fn lsp_edits(
    db: &ProjectDatabase,
    encoding: crate::PositionEncoding,
    edits: Vec<ty_ide::FileRenameEdit>,
) -> Option<HashMap<Uri, Vec<TextEdit>>> {
    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    for edit in edits {
        let (file, range, new_text) = edit.into_parts();
        let range = range.to_lsp_range(db, file, encoding)?;
        let location = range.into_location()?;
        let edit = TextEdit {
            range: location.range,
            new_text,
        };
        changes.entry(location.uri).or_default().push(edit);
    }

    Some(changes)
}

/// Returns whether a directory is relevant to a Python rename batch.
///
/// Regular packages and directories containing Python source are relevant. Read errors are also
/// treated as relevant so the batch is declined rather than partially rewritten.
fn is_relevant_python_directory(system: &dyn System, path: &SystemPathBuf) -> bool {
    if !system.is_directory(path) {
        return false;
    }
    if [path.join("__init__.py"), path.join("__init__.pyi")]
        .iter()
        .any(|init| system.is_file(init))
    {
        return true;
    }

    let Ok(mut entries) = system.read_directory(path) else {
        return true;
    };
    entries.any(|entry| {
        let Ok(entry) = entry else {
            return true;
        };
        let file_type = entry.file_type();
        let path = entry.into_path();
        file_type.is_file() && matches!(path.extension(), Some("py" | "pyi"))
            || file_type.is_directory() && is_relevant_python_directory(system, &path)
    })
}

fn file_uri_to_path(uri: &str) -> Option<SystemPathBuf> {
    let uri = Uri::parse(uri).ok()?;
    SystemPathBuf::from_path_buf(uri.to_file_path().ok()?).ok()
}
