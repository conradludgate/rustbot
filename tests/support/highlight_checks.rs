//! Observe the production check span closing, including all awaited notifications.
//! Fauxcord broadcasts to every test bot, so both bot and message IDs must match.

use std::sync::{LazyLock, Once, OnceLock};

use anyhow::{Context as _, Result};
use tokio::sync::broadcast;
use tracing::{
	Subscriber,
	field::Visit,
	span::{Attributes, Id},
};
use tracing_subscriber::{
	Layer,
	layer::{Context, SubscriberExt as _},
	registry::LookupSpan,
};

static CHECKS: LazyLock<broadcast::Sender<Check>> = LazyLock::new(|| broadcast::channel(1024).0);
static INITIALIZE: Once = Once::new();
static TRACES: OnceLock<opentelemetry_sdk::trace::SdkTracerProvider> = OnceLock::new();

#[derive(Clone, Default)]
pub(super) struct Check {
	bot_id: u64,
	message_id: u64,
}

impl Visit for Check {
	fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
		match field.name() {
			"bot.id" => self.bot_id = value,
			"message.id" => self.message_id = value,
			_ => {}
		}
	}

	fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

struct CompletedChecks;

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for CompletedChecks {
	fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
		if attributes.metadata().name() != "highlight.check_message" {
			return;
		}
		let mut check = Check::default();
		attributes.record(&mut check);
		ctx.span(id)
			.expect("new span exists")
			.extensions_mut()
			.insert(check);
	}

	fn on_close(&self, id: Id, ctx: Context<'_, S>) {
		if let Some(check) = ctx
			.span(&id)
			.and_then(|span| span.extensions().get::<Check>().cloned())
		{
			let _ = CHECKS.send(check);
		}
	}
}

pub(super) fn initialize() {
	INITIALIZE.call_once(|| {
		let subscriber = tracing_subscriber::registry().with(CompletedChecks.with_filter(
			tracing_subscriber::filter::filter_fn(|metadata| {
				metadata.name() == "highlight.check_message"
			}),
		));
		let otel = std::env::var("FERRIS_TEST_OTLP_ENDPOINT")
			.ok()
			.map(|endpoint| {
				use opentelemetry::trace::TracerProvider as _;
				use opentelemetry_otlp::WithExportConfig as _;
				let exporter = opentelemetry_otlp::SpanExporter::builder()
					.with_http()
					.with_endpoint(endpoint)
					.build()
					.expect("build test OTLP exporter");
				let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
					.with_batch_exporter(exporter)
					.with_resource(
						opentelemetry_sdk::Resource::builder()
							.with_service_name("ferrisbot-integration")
							.build(),
					)
					.build();
				let layer = tracing_opentelemetry::layer()
					.with_tracer(provider.tracer("ferrisbot-test"))
					.with_filter(tracing_subscriber::EnvFilter::new("info,serenity=warn"));
				TRACES.set(provider).expect("store test tracer provider");
				layer
			});
		let subscriber = subscriber.with(otel);
		tracing::subscriber::set_global_default(subscriber)
			.expect("install integration-test highlight observer");
	});
}

pub(super) fn subscribe() -> broadcast::Receiver<Check> {
	CHECKS.subscribe()
}

pub(super) async fn wait(
	mut checks: broadcast::Receiver<Check>,
	bot_id: &str,
	message_id: &str,
) -> Result<()> {
	let bot_id: u64 = bot_id.parse()?;
	let message_id: u64 = message_id.parse()?;
	tokio::time::timeout(super::REPLY_TIMEOUT, async {
		loop {
			let check = checks
				.recv()
				.await
				.context("receiving completed highlight checks")?;
			if check.bot_id == bot_id && check.message_id == message_id {
				return Ok(());
			}
		}
	})
	.await
	.with_context(|| {
		format!("highlight check did not finish for bot {bot_id}, message {message_id}")
	})?
}

/// Optional local Jaeger export; ordinary snapshot tests need no tracing backend.
pub(super) fn flush_traces() {
	if let Some(provider) = TRACES.get() {
		provider.force_flush().expect("flush test traces");
	}
}
