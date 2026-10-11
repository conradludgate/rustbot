//! Check exported OTEL data, not just tracing's static callsite metadata.

use std::sync::{Arc, Mutex};

use ferrisbot_for_discord::helpers::discord_operation_span;
use opentelemetry::{
	Value,
	trace::{SpanKind, TracerProvider as _},
};
use opentelemetry_sdk::{
	error::OTelSdkResult,
	trace::{SdkTracerProvider, SpanData, SpanExporter},
};
use tracing_subscriber::layer::SubscriberExt as _;

#[derive(Clone, Debug, Default)]
struct Capture(Arc<Mutex<Vec<SpanData>>>);

impl SpanExporter for Capture {
	async fn export(&self, spans: Vec<SpanData>) -> OTelSdkResult {
		self.0.lock().unwrap().extend(spans);
		Ok(())
	}
}

#[test]
fn discord_sdk_spans_export_operation_names_and_external_peer() {
	let capture = Capture::default();
	let provider = SdkTracerProvider::builder()
		.with_simple_exporter(capture.clone())
		.build();
	let subscriber = tracing_subscriber::registry()
		.with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
	tracing::subscriber::with_default(subscriber, || {
		let parent = tracing::info_span!("highlight.notify");
		parent.in_scope(|| {
			for operation in ["guild.member", "user.direct_message", "reply.send"] {
				drop(discord_operation_span(operation));
			}
			drop(ferrisbot_for_discord::helpers::discord_attachment_span(
				"https://cdn.discordapp.com/attachments/private-path?signature=private-token",
			));
		});
	});
	provider.force_flush().unwrap();
	let spans = capture.0.lock().unwrap();
	let parent = spans
		.iter()
		.find(|span| span.name == "highlight.notify")
		.unwrap();
	assert_eq!(parent.span_kind, SpanKind::Internal);
	assert!(
		!parent
			.attributes
			.iter()
			.any(|a| a.key.as_str() == "peer.service")
	);
	let attachment = spans
		.iter()
		.find(|s| s.name == "discord.attachment.download")
		.unwrap();
	let addresses: Vec<_> = attachment
		.attributes
		.iter()
		.filter(|a| a.key.as_str() == "server.address")
		.collect();
	assert_eq!(addresses.len(), 1);
	assert_eq!(addresses[0].value, Value::from("cdn.discordapp.com"));
	assert!(!format!("{attachment:?}").contains("private-"));
	for (name, operation) in [
		("discord.guild.member", "guild.member"),
		("discord.user.direct_message", "user.direct_message"),
		("discord.reply.send", "reply.send"),
	] {
		let span = spans.iter().find(|span| span.name == name).unwrap();
		assert_eq!(span.span_kind, SpanKind::Client);
		assert_eq!(span.parent_span_id, parent.span_context.span_id());
		for (key, value) in [
			("peer.service", "discord"),
			("server.address", "discord.com"),
			("discord.operation", operation),
		] {
			assert!(
				span.attributes
					.iter()
					.any(|a| a.key.as_str() == key && a.value == Value::from(value.to_owned())),
				"{name} missing {key}={value}"
			);
		}
	}
}

#[tokio::test]
async fn external_http_spans_preserve_names_peers_and_response_status() {
	use ferrisbot_for_discord::telemetry::{ExternalHttpBackend, ExternalOperation};
	use tracing::instrument::WithSubscriber as _;
	use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

	let server = MockServer::start().await;
	for (endpoint, status) in [("/execute", 200), ("/api/compiler/secret-id/compile", 503)] {
		Mock::given(path(endpoint))
			.respond_with(ResponseTemplate::new(status))
			.mount(&server)
			.await;
	}
	let capture = Capture::default();
	let provider = SdkTracerProvider::builder()
		.with_simple_exporter(capture.clone())
		.build();
	let subscriber = tracing_subscriber::registry()
		.with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
	let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new())
		.with(reqwest_tracing::TracingMiddleware::<ExternalHttpBackend>::new())
		.build();
	async {
		for (path, operation, peer) in [
			("/execute", "rust-playground.execute", "rust-playground"),
			(
				"/api/compiler/secret-id/compile",
				"godbolt.compile",
				"godbolt",
			),
		] {
			client
				.post(format!("{}{path}?token=private", server.uri()))
				.with_extension(ExternalOperation::new(operation, peer))
				.send()
				.await
				.unwrap();
		}
	}
	.with_subscriber(subscriber)
	.await;
	provider.force_flush().unwrap();
	let spans = capture.0.lock().unwrap();
	assert_eq!(spans.len(), 2);
	for (name, peer, status) in [
		("rust-playground.execute", "rust-playground", 200i64),
		("godbolt.compile", "godbolt", 503),
	] {
		let span = spans.iter().find(|s| s.name == name).unwrap();
		assert_eq!(span.span_kind, SpanKind::Client);
		for (key, value) in [
			("peer.service", Value::from(peer)),
			("http.request.method", Value::from("POST")),
			("server.address", Value::from("127.0.0.1")),
			("http.response.status_code", Value::from(status)),
		] {
			assert!(
				span.attributes
					.iter()
					.any(|a| a.key.as_str() == key && a.value == value),
				"missing {key} on {name}: {:?}",
				span.attributes
			);
		}
		assert!(!format!("{span:?}").contains("private"));
		assert!(!format!("{span:?}").contains("secret-id"));
	}
	assert!(matches!(
		spans
			.iter()
			.find(|s| s.name == "godbolt.compile")
			.unwrap()
			.status,
		opentelemetry::trace::Status::Error { .. }
	));
}

#[tokio::test]
async fn sqlite_queries_export_database_attributes_without_bindings() {
	use ferrisbot_for_discord::telemetry::sqlite_query;
	use tracing::instrument::WithSubscriber as _;
	let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
	sqlx::query("CREATE TABLE private_values (value TEXT)")
		.execute(&pool)
		.await
		.unwrap();
	let capture = Capture::default();
	let provider = SdkTracerProvider::builder()
		.with_simple_exporter(capture.clone())
		.build();
	let subscriber = tracing_subscriber::registry()
		.with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
	async {
		sqlite_query(
			"INSERT",
			"private_values",
			sqlx::query("INSERT INTO private_values VALUES (?)")
				.bind("sensitive-bound-value")
				.execute(&pool),
		)
		.await
		.unwrap();
		let row: (String,) = sqlite_query(
			"SELECT",
			"private_values",
			sqlx::query_as("SELECT value FROM private_values").fetch_one(&pool),
		)
		.await
		.unwrap();
		assert_eq!(row.0, "sensitive-bound-value");
		assert!(
			sqlite_query(
				"SELECT",
				"missing_table",
				sqlx::query("SELECT * FROM missing_table").fetch_all(&pool)
			)
			.await
			.is_err()
		);
	}
	.with_subscriber(subscriber)
	.await;
	provider.force_flush().unwrap();
	let spans = capture.0.lock().unwrap();
	assert_eq!(spans.len(), 3);
	for span in spans.iter() {
		assert_eq!(span.span_kind, SpanKind::Client);
		for (key, value) in [
			("db.system.name", "sqlite"),
			("db.system", "sqlite"),
			("db.namespace", "main"),
			("db.query.summary", span.name.as_ref()),
		] {
			assert!(
				span.attributes
					.iter()
					.any(|a| a.key.as_str() == key && a.value == Value::from(value.to_owned())),
				"{key} missing"
			);
		}
		assert!(!span.attributes.iter().any(|a| matches!(
			a.key.as_str(),
			"server.address" | "peer.service" | "db.query.text"
		)));
		assert!(!format!("{span:?}").contains("sensitive-bound-value"));
	}
	let failed = spans
		.iter()
		.find(|s| s.name == "SELECT missing_table")
		.unwrap();
	assert!(matches!(
		failed.status,
		opentelemetry::trace::Status::Error { .. }
	));
	assert!(
		failed
			.attributes
			.iter()
			.any(|a| a.key.as_str() == "db.response.status_code")
	);
}
