//! Workspace previews retain the original's membership and publisher checks.

use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};

use crate::api::{blocking, Caller, Shared};
use crate::error::ApiError;
use crate::preview::{PreviewInput, PreviewOptions, PreviewPermit, MAX_INPUT};

use super::{auth, get as record, runtime, sdk, sources, Collection};

#[derive(Clone, Copy)]
enum Original {
    Source,
    Artifact,
}

pub(super) fn router() -> Router<Shared> {
    Router::new()
        .route(
            "/v1/workspaces/{workspace}/sources/{source}/preview",
            get(source_preview),
        )
        .route(
            "/v1/workspaces/{workspace}/sources/{source}/preview/assets/{asset}",
            get(source_asset),
        )
        .route(
            "/v1/workspaces/{workspace}/artifacts/{artifact}/frame",
            axum::routing::post(open_frame),
        )
        .route(
            "/v1/workspaces/{workspace}/artifacts/{artifact}/preview",
            get(artifact_preview),
        )
        .route(
            "/v1/workspaces/{workspace}/artifacts/{artifact}/preview/assets/{asset}",
            get(artifact_asset),
        )
}

async fn source_preview(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path((workspace, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    render(app, caller, workspace, id, Original::Source, query, None).await
}

async fn artifact_preview(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path((workspace, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    render(app, caller, workspace, id, Original::Artifact, query, None).await
}

async fn source_asset(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path((workspace, id, asset)): Path<(String, String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    render(
        app,
        caller,
        workspace,
        id,
        Original::Source,
        query,
        Some(asset),
    )
    .await
}

async fn artifact_asset(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path((workspace, id, asset)): Path<(String, String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    render(
        app,
        caller,
        workspace,
        id,
        Original::Artifact,
        query,
        Some(asset),
    )
    .await
}

/// The largest HTML output the console will frame.
const MAX_FRAME: usize = 8 * 1024 * 1024;

/// `POST .../artifacts/{artifact}/frame`: the address `/app/frame/{ticket}`
/// the console puts in a sandboxed iframe. It names an HTML output only.
async fn open_frame(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path((workspace, id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let companion = app
        .companion
        .clone()
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "the console is off"))?;
    authorize(&app, &caller, &workspace, &id, Original::Artifact)?;
    let grant = crate::companion::FrameGrant::new(&caller, &workspace, &id);
    // Name and size are checked now so a wrong format is refused before a frame exists.
    frame_html(&app, &grant).await?;
    let ticket = companion.grant_frame(&caller, &workspace, &id, std::time::Instant::now())?;
    Ok(Json(
        serde_json::json!({ "path": format!("/app/frame/{ticket}") }),
    ))
}

/// The bytes of an HTML output for its iframe, after the caller's current rights are checked.
pub(crate) async fn frame_html(
    app: &Shared,
    grant: &crate::companion::FrameGrant,
) -> Result<Vec<u8>, ApiError> {
    let (caller, workspace, id) = (&grant.caller, &grant.workspace, &grant.artifact);
    authorize(app, caller, workspace, id, Original::Artifact)?;
    let permit = app.preview.admit()?;
    let input = load(app, caller, workspace, id, Original::Artifact, &permit).await?;
    let name = input.name.to_ascii_lowercase();
    if !(name.ends_with(".html") || name.ends_with(".htm")) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "preview_unsupported",
            "only an HTML output can be opened in a frame",
        ));
    }
    if input.bytes.len() > MAX_FRAME {
        return Err(too_large());
    }
    authorize(app, caller, workspace, id, Original::Artifact)?;
    Ok(input.bytes)
}

async fn render(
    app: Shared,
    caller: Caller,
    workspace: String,
    id: String,
    original: Original,
    query: Option<String>,
    asset: Option<String>,
) -> Result<Response, ApiError> {
    let options = PreviewOptions::parse(query.as_deref())?;
    authorize(&app, &caller, &workspace, &id, original)?;
    let service = app.preview.clone();
    let permit = match &asset {
        Some(asset) => service.admit_asset(asset)?,
        None => service.admit()?,
    };
    let input = load(&app, &caller, &workspace, &id, original, &permit).await?;
    let response = match asset {
        Some(asset) => crate::preview::png(permit.asset(input, options, &asset).await?),
        None => {
            crate::preview::private(Json(permit.preview(input, options).await?).into_response())
        }
    };
    // Conversion may outlive a permission change. Cached derivatives carry no
    // independent grant and cannot bypass a revoked membership or credential.
    authorize(&app, &caller, &workspace, &id, original)?;
    Ok(response)
}

fn authorize(
    app: &crate::api::App,
    caller: &Caller,
    workspace: &str,
    id: &str,
    original: Original,
) -> Result<(), ApiError> {
    app.check_caller(caller)?;
    let row = auth::authorize_workspace(app, caller, workspace, "read")?;
    match original {
        Original::Source => {
            let source = record(app, Collection::Sources, id)?;
            if source.tenant != caller.tenant() || source.body["workspace_id"] != workspace {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "no such source",
                ));
            }
        }
        Original::Artifact => {
            let repo = runtime::resolve(app, &row)?;
            sources::find_artifact(app, caller, &row, &repo, id)?;
        }
    }
    Ok(())
}

async fn load(
    app: &Shared,
    caller: &Caller,
    workspace: &str,
    id: &str,
    original: Original,
    permit: &PreviewPermit<'_>,
) -> Result<PreviewInput, ApiError> {
    let (app, caller, workspace, id) = (
        app.clone(),
        caller.clone(),
        workspace.to_owned(),
        id.to_owned(),
    );
    let admission = permit.materialization_guard();
    blocking(move || {
        let _admission = admission;
        authorize(&app, &caller, &workspace, &id, original)?;
        match original {
            Original::Source => {
                let row = record(&app, Collection::Sources, &id)?;
                if row.body["size_bytes"]
                    .as_u64()
                    .is_none_or(|size| size > MAX_INPUT)
                {
                    return Err(too_large());
                }
                let (metadata, bytes) = sources::source_original(&app, &caller, &workspace, &id)?;
                Ok(PreviewInput {
                    bytes,
                    name: metadata.title,
                    media_type: metadata.media_type,
                    digest: metadata.digest,
                    tenant: caller.tenant().into(),
                })
            }
            Original::Artifact => {
                let row = auth::authorize_workspace(&app, &caller, &workspace, "read")?;
                let repo = runtime::resolve(&app, &row)?;
                let (metadata, reader) = sources::find_artifact(&app, &caller, &row, &repo, &id)?;
                if metadata.size > MAX_INPUT {
                    return Err(too_large());
                }
                let scratch = tempfile::tempdir().map_err(super::io)?;
                let path = scratch.path().join("original");
                let metadata = repo.yard.read_artifact(&reader, &id, &path).map_err(sdk)?;
                let bytes = crate::preview::read_regular(&path, MAX_INPUT)?;
                Ok(PreviewInput {
                    bytes,
                    name: metadata.name,
                    media_type: metadata.media_type,
                    digest: metadata.digest,
                    tenant: caller.tenant().into(),
                })
            }
        }
    })
    .await?
}

fn too_large() -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "preview_too_large",
        "document exceeds the 64 MiB preview limit",
    )
}
