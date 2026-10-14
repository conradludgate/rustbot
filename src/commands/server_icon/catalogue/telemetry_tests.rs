//! Inspect exported traces, including the lifetime of a pending listener.
use std::sync::{Arc, Mutex};

use opentelemetry::trace::{SpanId, Status, TracerProvider as _};
use opentelemetry_sdk::{
	error::OTelSdkResult,
	trace::{SdkTracerProvider, SpanData, SpanExporter},
};
use tracing_subscriber::layer::SubscriberExt as _;

use super::*;

use crate::telemetry::TEST_CAPTURE_LOCK as CAPTURE_LOCK;

#[derive(Clone, Debug, Default)]
struct Capture(Arc<Mutex<Vec<SpanData>>>);

impl SpanExporter for Capture {
	fn export(&self, spans: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> {
		self.0.lock().unwrap().extend(spans);
		std::future::ready(Ok(()))
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_closes_while_listener_waits_and_actions_are_linked_root_traces() {
	let _guard = CAPTURE_LOCK.lock().await;
	let capture = Capture::default();
	let provider = SdkTracerProvider::builder()
		.with_simple_exporter(capture.clone())
		.build();
	let subscriber = tracing_subscriber::registry()
		.with(tracing_opentelemetry::layer().with_tracer(provider.tracer("catalogue-test")));
	let (send, mut receive) =
		tokio::sync::mpsc::channel::<(&'static str, bool, tokio::sync::oneshot::Sender<bool>)>(4);
	async {
		let listener = tracing::info_span!("command.server_icon.list").in_scope(|| {
			let origin = tracing::Span::current()
				.context()
				.span()
				.span_context()
				.clone();
			spawn_listener(async move {
				loop {
					assert!(
						tracing::Span::current().id().is_none(),
						"idle listener inherited a span"
					);
					let Some((action, fail, done)) = receive.recv().await else {
						return Ok(());
					};
					let result = linked_action(&origin, action, async {
						drop(discord_operation_span("message.edit"));
						if fail { Err(anyhow::anyhow!("private-interaction-token")) } else { Ok(()) }
					})
					// Even an unrelated ambient span must not become the parent.
					.instrument(tracing::info_span!("unrelated.dispatch"))
					.await;
					done.send(result.is_err()).unwrap();
					result?;
				}
			})
		});

		// This assertion runs before sending any clicks and before the listener
		// finishes. Capturing a tracing::Span instead of its SpanContext fails it.
		provider.force_flush().unwrap();
		assert!(
			capture
				.0
				.lock()
				.unwrap()
				.iter()
				.any(|span| span.name == "command.server_icon.list")
		);
		assert!(!listener.is_finished());
		for (action, fail) in [
			("click", false),
			("click", false),
			("expire", false),
			("click", true),
		] {
			let (done, wait) = tokio::sync::oneshot::channel();
			send.send((action, fail, done)).await.unwrap();
			assert_eq!(wait.await.unwrap(), fail);
		}
		listener.await.unwrap();
	}
	.with_subscriber(subscriber)
	.await;
	provider.force_flush().unwrap();
	let spans = capture.0.lock().unwrap();
	let command = spans
		.iter()
		.find(|span| span.name == "command.server_icon.list")
		.unwrap();
	let actions: Vec<_> = spans
		.iter()
		.filter(|span| span.name.starts_with("server_icon.catalogue."))
		.collect();
	assert_eq!(actions.len(), 4);
	let mut trace_ids = std::collections::HashSet::new();
	for action in &actions {
		assert_eq!(action.parent_span_id, SpanId::INVALID);
		assert_ne!(
			action.span_context.trace_id(),
			command.span_context.trace_id()
		);
		assert!(
			trace_ids.insert(action.span_context.trace_id()),
			"actions share a trace"
		);
		assert_eq!(action.links.links.len(), 1);
		assert_eq!(action.links.links[0].span_context, command.span_context);
		assert!(
			command.end_time <= action.start_time,
			"command remained open until an action"
		);
		let call = spans
			.iter()
			.find(|span| {
				span.name == "discord.message.edit"
					&& span.span_context.trace_id() == action.span_context.trace_id()
			})
			.unwrap();
		assert_eq!(call.parent_span_id, action.span_context.span_id());
	}
	assert_eq!(
		actions
			.iter()
			.filter(|span| matches!(span.status, Status::Error { .. }))
			.count(),
		1
	);
	assert!(!format!("{spans:?}").contains("private-interaction-token"));
}

#[tokio::test]
async fn actions_still_work_without_a_recorded_command_context() {
	let _guard = CAPTURE_LOCK.lock().await;
	let capture = Capture::default();
	let provider = SdkTracerProvider::builder()
		.with_simple_exporter(capture.clone())
		.build();
	let subscriber = tracing_subscriber::registry()
		.with(tracing_opentelemetry::layer().with_tracer(provider.tracer("catalogue-test")));
	linked_action(&SpanContext::NONE, "click", async { Ok(()) })
		.with_subscriber(subscriber)
		.await
		.unwrap();
	provider.force_flush().unwrap();
	let spans = capture.0.lock().unwrap();
	assert_eq!(spans.len(), 1);
	assert_eq!(spans[0].parent_span_id, SpanId::INVALID);
	assert_eq!(spans[0].links.links.len(), 0);
}
