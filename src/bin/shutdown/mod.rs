//! Process signals stop the Gateway before the runtime and telemetry are dropped.

use std::{future::Future, io, time::Duration};

use poise::serenity_prelude as serenity;
use snafu::ResultExt as _;
use tracing::{info, warn};

use super::{AppError, SerenitySnafu, ShutdownSignalSnafu, ShutdownTimeoutSnafu};

#[cfg(unix)]
pub(super) fn signal() -> io::Result<impl Future<Output = io::Result<&'static str>>> {
	use tokio::signal::unix::{SignalKind, signal};
	let mut interrupt = signal(SignalKind::interrupt())?;
	let mut terminate = signal(SignalKind::terminate())?;
	Ok(async move {
		let (received, name) = tokio::select! {
			received = interrupt.recv() => (received, "SIGINT"),
			received = terminate.recv() => (received, "SIGTERM"),
		};
		received
			.map(|()| name)
			.ok_or_else(|| io::Error::other("shutdown signal stream closed"))
	})
}

#[cfg(not(unix))]
pub(super) fn signal() -> io::Result<impl Future<Output = io::Result<&'static str>>> {
	Ok(async {
		tokio::signal::ctrl_c().await?;
		Ok("Ctrl+C")
	})
}

pub(super) async fn run_until_shutdown(
	run: impl Future<Output = serenity::Result<()>>,
	signal: impl Future<Output = io::Result<&'static str>>,
	shutdown: impl Future<Output = ()>,
	grace_period: Duration,
) -> Result<(), AppError> {
	// The losing future is cancelled here. In particular, a signal during
	// start_autosharded's HTTP setup must not start new shards after shutdown.
	let signal = tokio::select! {
		result = run => return result.context(SerenitySnafu),
		signal = signal => signal.context(ShutdownSignalSnafu)?,
	};
	info!(signal, "shutdown requested; stopping Discord Gateway");
	if let Err(error) = tokio::time::timeout(grace_period, shutdown).await {
		warn!("Discord Gateway did not stop within the shutdown grace period");
		return Err(error).context(ShutdownTimeoutSnafu);
	}
	info!("Discord Gateway shutdown complete");
	Ok(())
}

/// Run outside Tokio: the production OTLP exporter uses a blocking HTTP client.
pub(super) fn flush_telemetry(provider: opentelemetry_sdk::trace::SdkTracerProvider) {
	if let Err(error) = provider.force_flush() {
		warn!(%error, "OpenTelemetry batch flush failed");
	}
	// Always close the exporter, even if the preceding flush failed.
	if let Err(error) = provider.shutdown() {
		warn!(%error, "OpenTelemetry tracer shutdown failed");
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::{
		future,
		sync::{
			Arc,
			atomic::{AtomicBool, Ordering},
		},
	};

	#[tokio::test]
	async fn signal_stops_gateway_after_cancelling_startup() {
		struct Startup(Arc<AtomicBool>);
		impl Drop for Startup {
			fn drop(&mut self) {
				self.0.store(true, Ordering::SeqCst);
			}
		}
		let cancelled = Arc::new(AtomicBool::new(false));
		let startup = Startup(cancelled.clone());
		let run = async move {
			let _startup = startup;
			future::pending::<serenity::Result<()>>().await
		};
		run_until_shutdown(
			run,
			async { Ok("SIGTERM") },
			async {
				assert!(cancelled.load(Ordering::SeqCst));
			},
			Duration::from_secs(1),
		)
		.await
		.unwrap();
	}

	#[tokio::test]
	async fn gateway_failure_returns_without_waiting_for_signal() {
		let result = run_until_shutdown(
			async { Err(serenity::Error::Other("gateway failed")) },
			future::pending(),
			async { panic!("shutdown should not run when the Gateway has returned") },
			Duration::from_secs(1),
		)
		.await;
		assert!(matches!(result, Err(AppError::Serenity { .. })));
	}

	#[tokio::test]
	async fn normal_gateway_exit_returns_without_waiting_for_signal() {
		run_until_shutdown(
			async { Ok(()) },
			future::pending(),
			future::pending(),
			Duration::from_secs(1),
		)
		.await
		.unwrap();
	}

	#[tokio::test]
	async fn signal_errors_are_returned() {
		let result = run_until_shutdown(
			future::pending(),
			async { Err(io::Error::other("signal handler failed")) },
			future::pending(),
			Duration::from_secs(1),
		)
		.await;
		assert!(matches!(result, Err(AppError::ShutdownSignal { .. })));
	}

	#[tokio::test]
	async fn stuck_gateway_shutdown_is_bounded() {
		let result = run_until_shutdown(
			future::pending(),
			async { Ok("SIGINT") },
			future::pending(),
			Duration::from_millis(1),
		)
		.await;
		assert!(matches!(result, Err(AppError::ShutdownTimeout { .. })));
	}
	#[derive(Clone, Debug)]
	struct RecordingExporter {
		events: Arc<std::sync::Mutex<Vec<String>>>,
		fail_export: bool,
	}

	impl opentelemetry_sdk::trace::SpanExporter for RecordingExporter {
		async fn export(
			&self,
			spans: Vec<opentelemetry_sdk::trace::SpanData>,
		) -> opentelemetry_sdk::error::OTelSdkResult {
			self.events
				.lock()
				.unwrap()
				.extend(spans.into_iter().map(|s| s.name.into_owned()));
			if self.fail_export {
				Err(opentelemetry_sdk::error::OTelSdkError::InternalFailure(
					"test export failure".into(),
				))
			} else {
				Ok(())
			}
		}
	}

	impl Drop for RecordingExporter {
		fn drop(&mut self) {
			self.events.lock().unwrap().push("exporter.dropped".into());
		}
	}

	fn check_batch_shutdown(fail_export: bool) {
		use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
		use opentelemetry_sdk::trace::{BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider};
		let events = Arc::new(std::sync::Mutex::new(Vec::new()));
		let processor = BatchSpanProcessor::builder(RecordingExporter {
			events: events.clone(),
			fail_export,
		})
		.with_batch_config(
			BatchConfigBuilder::default()
				.with_scheduled_delay(Duration::from_secs(60))
				.build(),
		)
		.build();
		let provider = SdkTracerProvider::builder()
			.with_span_processor(processor)
			.build();
		provider
			.tracer("shutdown-test")
			.start("queued-before-exit")
			.end();
		assert!(
			events.lock().unwrap().is_empty(),
			"span must still be buffered"
		);
		flush_telemetry(provider.clone());
		assert!(matches!(
			provider.shutdown(),
			Err(opentelemetry_sdk::error::OTelSdkError::AlreadyShutdown)
		));
		assert_eq!(
			*events.lock().unwrap(),
			["queued-before-exit", "exporter.dropped"]
		);
	}

	#[test]
	fn telemetry_shutdown_flushes_buffered_spans_before_closing_exporter() {
		check_batch_shutdown(false);
	}

	#[test]
	fn telemetry_shutdown_closes_exporter_even_when_flush_fails() {
		check_batch_shutdown(true);
	}
}
