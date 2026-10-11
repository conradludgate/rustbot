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
			for operation in ["guild.member", "user.direct_message", "say"] {
				drop(discord_operation_span(operation));
			}
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
	for (name, operation) in [
		("discord.guild.member", "guild.member"),
		("discord.user.direct_message", "user.direct_message"),
		("discord.say", "say"),
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
					.any(|a| a.key.as_str() == key && a.value == Value::from(value)),
				"{name} missing {key}={value}"
			);
		}
	}
}
