//! Low-cardinality names and semantic attributes for external HTTP and SQLite.

use http::Extensions;
use reqwest::{Request, Response};
use reqwest_tracing::ReqwestOtelSpanBackend;
use tracing::{Instrument as _, Span};

#[derive(Clone, Copy)]
pub struct ExternalOperation {
	name: &'static str,
	peer: &'static str,
}

impl ExternalOperation {
	#[must_use]
	pub const fn new(name: &'static str, peer: &'static str) -> Self {
		Self { name, peer }
	}
}

pub struct ExternalHttpBackend;

impl ReqwestOtelSpanBackend for ExternalHttpBackend {
	fn on_request_start(req: &Request, extensions: &mut Extensions) -> Span {
		let operation = extensions.get::<ExternalOperation>();
		let name =
			operation.map_or_else(|| format!("http.{}", req.method()), |o| o.name.to_owned());
		let span = tracing::info_span!("http.client",
			otel.name = name, otel.kind = "client",
			http.request.method = req.method().as_str(),
			url.scheme = req.url().scheme(),
			server.address = req.url().host_str().unwrap_or(""),
			server.port = tracing::field::Empty,
			peer.service = tracing::field::Empty,
			http.response.status_code = tracing::field::Empty,
			otel.status_code = tracing::field::Empty,
			error.type = tracing::field::Empty,
		);
		if let Some(port) = req.url().port_or_known_default() {
			span.record("server.port", i64::from(port));
		}
		if let Some(operation) = operation {
			span.record("peer.service", operation.peer);
		}
		span
	}

	fn on_request_end(
		span: &Span,
		outcome: &reqwest_middleware::Result<Response>,
		_: &mut Extensions,
	) {
		match outcome {
			Ok(response) => {
				let status = response.status();
				span.record("http.response.status_code", i64::from(status.as_u16()));
				if status.is_client_error() || status.is_server_error() {
					span.record("otel.status_code", "ERROR");
					span.record("error.type", status.as_str());
				}
			}
			Err(error) => {
				span.record("otel.status_code", "ERROR");
				// Error Display/Debug can contain URLs with credentials or query parameters.
				let kind = match error {
					reqwest_middleware::Error::Reqwest(e) if e.is_timeout() => "timeout",
					reqwest_middleware::Error::Reqwest(e) if e.is_connect() => "connection",
					_ => "http.request.failed",
				};
				span.record("error.type", kind);
			}
		}
	}
}

/// SQLite is embedded: use database client attributes without inventing a remote host.
#[must_use]
pub fn sqlite_span(operation: &'static str, collection: &'static str) -> Span {
	let summary = format!("{operation} {collection}").trim_end().to_owned();
	let span = tracing::info_span!("sqlite.query",
		otel.name = summary.as_str(), otel.kind = "client",
		db.system.name = "sqlite",
		// Keep legacy attributes for consumers still migrating to stable DB conventions.
		db.system = "sqlite", db.namespace = "main", db.name = "main",
		db.operation.name = operation, db.operation = operation,
		db.collection.name = tracing::field::Empty,
		db.query.summary = summary.as_str(),
		db.response.status_code = tracing::field::Empty,
		otel.status_code = tracing::field::Empty,
		error.type = tracing::field::Empty,
	);
	if !collection.is_empty() {
		span.record("db.collection.name", collection);
	}
	span
}

/// Trace each execution rather than marking a multi-query workflow as one DB call.
pub async fn sqlite_query<T>(
	operation: &'static str,
	collection: &'static str,
	query: impl std::future::Future<Output = Result<T, sqlx::Error>>,
) -> Result<T, sqlx::Error> {
	let span = sqlite_span(operation, collection);
	async {
		let result = query.await;
		if let Err(error) = &result
			&& !matches!(error, sqlx::Error::RowNotFound)
		{
			span.record("otel.status_code", "ERROR");
			let code = error
				.as_database_error()
				.and_then(sqlx::error::DatabaseError::code);
			span.record("error.type", code.as_deref().unwrap_or("sqlx.error"));
			if let Some(code) = code {
				span.record("db.response.status_code", code.as_ref());
			}
		}
		result
	}
	.instrument(span.clone())
	.await
}
