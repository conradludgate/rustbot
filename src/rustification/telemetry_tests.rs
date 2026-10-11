//! Verify real exported spans from the message/worker paths, including parentage.
use std::sync::{Arc, Mutex};

use opentelemetry::{
	Value,
	trace::{SpanKind, Status, TracerProvider as _},
};
use opentelemetry_sdk::{
	error::OTelSdkResult,
	trace::{SdkTracerProvider, SpanData, SpanExporter},
};
use serde_json::json;
use tracing::{Instrument as _, instrument::WithSubscriber as _};
use tracing_subscriber::{Layer as _, layer::SubscriberExt as _};
use wiremock::{
	Mock, MockServer, ResponseTemplate,
	matchers::{method, path},
};

use super::{
	RETRY,
	tests::{http, member, message, mock_member, mock_role, service},
};
// Isolate scoped subscriber registration and teardown across exported-data assertions.
static CAPTURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const JOINED: i64 = 1_700_000_000;
const DELAY: i64 = 1800;

#[derive(Clone, Debug, Default)]
struct Capture(Arc<Mutex<Vec<SpanData>>>);

impl SpanExporter for Capture {
	fn export(&self, spans: Vec<SpanData>) -> impl std::future::Future<Output = OTelSdkResult> {
		self.0.lock().unwrap().extend(spans);
		std::future::ready(Ok(()))
	}
}

fn capture() -> (
	Capture,
	SdkTracerProvider,
	impl tracing::Subscriber + Send + Sync,
) {
	let capture = Capture::default();
	let provider = SdkTracerProvider::builder()
		.with_simple_exporter(capture.clone())
		.build();
	let subscriber = tracing_subscriber::registry().with(
		tracing_opentelemetry::layer()
			.with_tracer(provider.tracer("rustification-test"))
			.with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
				metadata.target().starts_with("ferrisbot_for_discord")
			})),
	);
	(capture, provider, subscriber)
}

fn span<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
	spans
		.iter()
		.find(|span| span.name == name)
		.unwrap_or_else(|| panic!("missing {name}: {spans:#?}"))
}

fn attribute(span: &SpanData, key: &str, expected: impl Into<Value>) {
	let expected = expected.into();
	assert!(
		span.attributes
			.iter()
			.any(|a| a.key.as_str() == key && a.value == expected),
		"missing {key}={expected:?} on {}: {:?}",
		span.name,
		span.attributes
	);
}

fn child(span: &SpanData, parent: &SpanData) {
	assert_eq!(span.parent_span_id, parent.span_context.span_id());
	assert_eq!(span.span_context.trace_id(), parent.span_context.trace_id());
}

#[tokio::test]
async fn successful_assignment_exports_workflow_database_and_discord_children() {
	let _capture_guard = CAPTURE_LOCK.lock().await;
	let service = service().await;
	let server = MockServer::start().await;
	mock_member(&server, member(Some(JOINED), false, false), 1).await;
	mock_role(&server, 204, 1).await;
	let (capture, provider, subscriber) = capture();
	async {
		let origin = tracing::info_span!("test.gateway");
		async {
			let mut observed = message(Some(JOINED), false);
			observed.content = "private-message-body".into();
			service.observe_message(&observed).await.unwrap();
			// A scheduled batch must be a new trace, not a child of the startup/message span.
			service
				.process_due(&http(&server), JOINED + DELAY)
				.await
				.unwrap();
			service.next_wake(JOINED).await.unwrap();
		}
		.instrument(origin)
		.await;
	}
	.with_subscriber(subscriber)
	.await;
	provider.force_flush().unwrap();
	let spans = capture.0.lock().unwrap().clone();
	let observe = span(&spans, "rustification.observe_message");
	attribute(observe, "rustification.source", "message");
	child(observe, span(&spans, "test.gateway"));
	attribute(observe, "guild.id", "1");
	attribute(observe, "user.id", "2");
	let enqueue = span(&spans, "rustification.enqueue");
	child(enqueue, observe);
	attribute(enqueue, "rustification.queue_changed", true);
	child(span(&spans, "INSERT rustification_queue"), enqueue);
	let batch = span(&spans, "rustification.process_due");
	assert_eq!(batch.parent_span_id, opentelemetry::trace::SpanId::INVALID);
	assert_ne!(
		batch.span_context.trace_id(),
		observe.span_context.trace_id()
	);
	attribute(batch, "rustification.assignments", 1_i64);
	attribute(batch, "rustification.failures", 0_i64);
	let assignment = span(&spans, "rustification.assignment");
	assert_eq!(assignment.span_kind, SpanKind::Internal);
	attribute(assignment, "rustification.outcome", "assigned");
	attribute(assignment, "role.id", "3");
	child(assignment, batch);
	for name in ["discord.guild.member", "discord.member.add_role"] {
		let call = span(&spans, name);
		child(call, assignment);
		assert_eq!(call.span_kind, SpanKind::Client);
		attribute(call, "peer.service", "discord");
		assert!(!matches!(call.status, Status::Error { .. }));
	}
	for name in ["UPDATE rustification_queue", "DELETE rustification_queue"] {
		child(span(&spans, name), assignment);
	}
	let db: Vec<_> = spans
		.iter()
		.filter(|span| span.name.ends_with(" rustification_queue"))
		.collect();
	assert_eq!(db.len(), 5); // insert, due-select, retry-update, delete, wake-select
	for query in db {
		assert_eq!(query.span_kind, SpanKind::Client);
		attribute(query, "db.system.name", "sqlite");
		attribute(query, "db.collection.name", "rustification_queue");
		attribute(query, "db.query.summary", query.name.to_string());
		assert!(!query.attributes.iter().any(|a| matches!(
			a.key.as_str(),
			"db.query.text" | "peer.service" | "server.address"
		)));
	}
	assert!(!format!("{spans:?}").contains("private-message-body"));
	server.verify().await;
}

#[tokio::test]
async fn failed_role_assignment_exports_retry_deadline_and_error_status_without_payloads() {
	let _capture_guard = CAPTURE_LOCK.lock().await;
	let service = service().await;
	service
		.enqueue(serenity_user(), Some(JOINED), JOINED)
		.await
		.unwrap();
	let server = MockServer::start().await;
	mock_member(&server, member(Some(JOINED), false, false), 1).await;
	Mock::given(method("PUT"))
		.and(path("/api/v10/guilds/1/members/2/roles/3"))
		.respond_with(ResponseTemplate::new(403).set_body_json(json!({
			"code": 50013, "message": "private-response-body"
		})))
		.expect(1)
		.mount(&server)
		.await;
	let (capture, provider, subscriber) = capture();
	service
		.process_due(&http(&server), JOINED + DELAY)
		.with_subscriber(subscriber)
		.await
		.unwrap();
	provider.force_flush().unwrap();
	let spans = capture.0.lock().unwrap().clone();
	let assignment = span(&spans, "rustification.assignment");
	attribute(assignment, "rustification.outcome", "retry");
	attribute(
		assignment,
		"rustification.next_attempt_at",
		JOINED + DELAY + RETRY,
	);
	assert!(matches!(assignment.status, Status::Error { .. }));
	let call = span(&spans, "discord.member.add_role");
	child(call, assignment);
	assert!(matches!(call.status, Status::Error { .. }));
	attribute(call, "discord.error.code", 50013_i64);
	attribute(call, "error.type", "discord.api");
	let batch = span(&spans, "rustification.process_due");
	attribute(batch, "rustification.failures", 1_i64);
	assert!(matches!(batch.status, Status::Error { .. }));
	assert!(!format!("{spans:?}").contains("private-response-body"));
	assert!(!spans.iter().any(|s| s.name == "DELETE rustification_queue"));
	server.verify().await;
}

fn serenity_user() -> poise::serenity_prelude::UserId {
	poise::serenity_prelude::UserId::new(2)
}

#[tokio::test]
async fn welcome_duplicate_and_fast_path_spans_reflect_actual_queue_work() {
	let _capture_guard = CAPTURE_LOCK.lock().await;
	let service = service().await;
	let (capture, provider, subscriber) = capture();
	async {
		let mut welcome = message(Some(JOINED), false);
		welcome.kind = poise::serenity_prelude::MessageType::MemberJoin;
		service.observe_message(&welcome).await.unwrap();
		service.observe_message(&welcome).await.unwrap();
		service.pool.close().await;
		service
			.observe_message(&message(Some(JOINED), true))
			.await
			.unwrap();
	}
	.with_subscriber(subscriber)
	.await;
	provider.force_flush().unwrap();
	let spans = capture.0.lock().unwrap().clone();
	let observations: Vec<_> = spans
		.iter()
		.filter(|span| span.name == "rustification.observe_message")
		.collect();
	assert_eq!(observations.len(), 2); // No span at all for the Rustacean fast path.
	for observation in observations {
		attribute(observation, "rustification.source", "welcome");
	}
	let enqueues: Vec<_> = spans
		.iter()
		.filter(|span| span.name == "rustification.enqueue")
		.collect();
	assert_eq!(enqueues.len(), 2);
	attribute(enqueues[0], "rustification.queue_changed", true);
	attribute(enqueues[1], "rustification.queue_changed", false);
	assert_eq!(spans.len(), 6); // two observation/enqueue/INSERT trees
}

#[tokio::test]
async fn queue_errors_are_visible_on_query_and_workflow_spans() {
	let _capture_guard = CAPTURE_LOCK.lock().await;
	let service = service().await;
	sqlx::query("DROP TABLE rustification_queue")
		.execute(&service.pool)
		.await
		.unwrap();
	let server = MockServer::start().await;
	let (capture, provider, subscriber) = capture();
	async {
		assert!(
			service
				.observe_message(&message(Some(JOINED), false))
				.await
				.is_err()
		);
		assert!(
			service
				.process_due(&http(&server), JOINED + DELAY)
				.await
				.is_err()
		);
		assert!(service.next_wake(JOINED).await.is_err());
	}
	.with_subscriber(subscriber)
	.await;
	provider.force_flush().unwrap();
	let spans = capture.0.lock().unwrap().clone();
	for name in [
		"rustification.observe_message",
		"rustification.enqueue",
		"rustification.process_due",
		"rustification.next_wake",
	] {
		assert!(
			matches!(span(&spans, name).status, Status::Error { .. }),
			"{name} was not marked failed"
		);
	}
	for query in spans
		.iter()
		.filter(|span| span.name.ends_with(" rustification_queue"))
	{
		assert!(matches!(query.status, Status::Error { .. }));
		assert!(
			query
				.attributes
				.iter()
				.any(|a| a.key.as_str() == "db.response.status_code")
		);
	}
	assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn deferred_and_completed_decisions_export_distinct_non_error_outcomes() {
	let _capture_guard = CAPTURE_LOCK.lock().await;
	for (joined, pending, has_role, outcome, next_attempt) in [
		(
			Some(JOINED + DELAY - 10),
			false,
			false,
			"waiting",
			Some(JOINED + 2 * DELAY - 10),
		),
		(
			Some(JOINED),
			true,
			false,
			"screening_pending",
			Some(JOINED + DELAY + RETRY),
		),
		(
			None,
			false,
			false,
			"unknown_join_time",
			Some(JOINED + DELAY + RETRY),
		),
		(Some(JOINED), false, true, "already_has_role", None),
		(Some(JOINED), false, false, "departed", None),
	] {
		let service = service().await;
		service
			.enqueue(serenity_user(), Some(JOINED), JOINED)
			.await
			.unwrap();
		let server = MockServer::start().await;
		if outcome == "departed" {
			Mock::given(method("GET"))
				.and(path("/api/v10/guilds/1/members/2"))
				.respond_with(
					ResponseTemplate::new(404)
						.set_body_json(json!({"code": 10007, "message": "Unknown Member"})),
				)
				.expect(1)
				.mount(&server)
				.await;
		} else {
			mock_member(&server, member(joined, has_role, pending), 1).await;
		}
		let (capture, provider, subscriber) = capture();
		service
			.process_due(&http(&server), JOINED + DELAY)
			.with_subscriber(subscriber)
			.await
			.unwrap();
		provider.force_flush().unwrap();
		let spans = capture.0.lock().unwrap().clone();
		let assignment = span(&spans, "rustification.assignment");
		attribute(assignment, "rustification.outcome", outcome);
		assert!(
			!matches!(assignment.status, Status::Error { .. }),
			"{outcome} is not a workflow failure"
		);
		if let Some(due) = next_attempt {
			attribute(assignment, "rustification.next_attempt_at", due);
		}
		assert!(
			!spans
				.iter()
				.any(|span| span.name == "discord.member.add_role")
		);
		attribute(
			span(&spans, "rustification.process_due"),
			"rustification.failures",
			0_i64,
		);
		server.verify().await;
	}
}
