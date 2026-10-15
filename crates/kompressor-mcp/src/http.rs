//! Mounting the tools on the shared HTTP listener.
//!
//! The transport is streamable HTTP in stateless mode: every POST is an
//! independent exchange carrying its own credential, with no session held
//! between calls. That is what lets the service scale horizontally — any
//! replica can answer any request — and it is also what makes the tenant a
//! property of the request rather than of a handshake that happened earlier on
//! some other pod.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use kompressor_core::error::AuthError;
use kompressor_core::AppCore;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::handler::KompressorTools;
use crate::identity::resolve;
use crate::limit::RetrievalLimiter;

/// Mount the MCP tools as an `axum` router, ready to be merged with the other
/// surfaces on one listener.
///
/// The router serves exactly one path, `config.mcp.path` (default `/mcp`), so
/// merging it alongside the compression API and the proxy cannot collide.
/// Whether to mount it at all is the caller's decision: this function does not
/// consult `config.mcp.enabled`, because a binary that merged a disabled surface
/// and got an empty router would be harder to diagnose than one that never
/// merged it.
pub fn mcp_router(core: Arc<AppCore>) -> Router {
    let mcp = core.config().mcp.clone();
    let limiter = Arc::new(RetrievalLimiter::new(mcp.max_retrievals_per_minute));

    let config = StreamableHttpServerConfig::default()
        // Stateless: no session is created, each POST stands alone, and the
        // reply is a plain JSON body rather than an event stream.
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        // Empty means accept any Host, which is the right default for a service
        // reached by its own DNS name. An SDK default of localhost-only host
        // checking rejects a request carrying the service's real DNS name as
        // misdirected: the caller's connection hangs and the retrieval
        // capability silently never registers. Whether a Host check applies is a
        // property of the deployment, so it is a setting and not a built-in
        // default.
        .with_allowed_hosts(mcp.allowed_hosts.clone())
        .with_max_request_body_bytes(core.config().server.max_body_bytes);

    let tools = {
        let core = core.clone();
        let limiter = limiter.clone();
        move || {
            Ok(KompressorTools::from_connection(
                core.clone(),
                limiter.clone(),
            ))
        }
    };
    let service: StreamableHttpService<KompressorTools, LocalSessionManager> =
        StreamableHttpService::new(tools, Arc::new(LocalSessionManager::default()), config);

    let path = mount_path(&mcp.path);
    let router = if path == "/" {
        // `nest_service` at the root is not a nesting at all, and axum refuses
        // it. A deployment that gave the whole listener over to MCP still has to
        // work.
        Router::new().fallback_service(service)
    } else {
        Router::new().nest_service(&path, service)
    };
    router
        // Tenant resolution runs before the transport, so by the time any tool
        // body executes the tenant is already decided and nothing a tool call
        // says about it can matter.
        .layer(middleware::from_fn_with_state(core, authenticate))
}

/// Normalise a configured path into one `axum` will accept.
///
/// A missing leading slash and a trailing one are both things an operator writes
/// without thinking, and neither should be the difference between a working
/// deployment and a router that panics at startup.
fn mount_path(configured: &str) -> String {
    let trimmed = configured.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

/// Resolve the request's credential and attach the identity for the tools.
async fn authenticate(
    State(core): State<Arc<AppCore>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    match resolve(core.auth(), request.headers()) {
        Ok(identity) => {
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        // Refused at the transport, not inside a tool: a caller whose credential
        // is wrong has a client to fix, and telling the model about it would
        // invite it to retry with arguments it invented.
        Err(error) => unauthorized(error).into_response(),
    }
}

fn unauthorized(error: AuthError) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let reason = match error {
        AuthError::Missing => "no credential was presented",
        AuthError::Unknown => "the credential was not recognised",
        AuthError::Disabled => "the credential has been disabled",
    };
    (
        StatusCode::UNAUTHORIZED,
        [("content-type", "application/json")],
        serde_json::json!({
            "error": {
                "type": "unauthorized",
                "message": format!(
                    "{reason}. Present an API key as `Authorization: Bearer <key>`, `X-Api-Key` \
                     or `X-Kompressor-Key`."
                ),
            }
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_path_is_normalised_rather_than_taken_literally() {
        assert_eq!(mount_path("/mcp"), "/mcp");
        assert_eq!(mount_path("mcp"), "/mcp");
        assert_eq!(mount_path("/mcp/"), "/mcp");
        assert_eq!(mount_path(" /tools/mcp "), "/tools/mcp");
        assert_eq!(mount_path("/"), "/");
        assert_eq!(mount_path(""), "/");
    }
}
