//! Durable role assignment without the privileged guild members intent.
use std::{sync::Arc, time::Duration};

use anyhow::Result;
use poise::serenity_prelude as serenity;
use sqlx::{FromRow, SqlitePool};
use tokio::{
	sync::{Mutex, Notify},
	task::JoinHandle,
};
use tracing::{Instrument as _, Span, warn};

use crate::{helpers::discord_operation, telemetry::sqlite_query};

/// Role eligibility delay, in seconds; zero allows immediate assignment.
#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(default)]
pub struct RustificationConfig {
	pub delay_seconds: u32,
}

impl Default for RustificationConfig {
	fn default() -> Self {
		Self {
			delay_seconds: 30 * 60,
		}
	}
}

const RETRY: i64 = 60;
const BATCH_SIZE: i64 = 100;

#[derive(Debug, FromRow)]
struct Assignment {
	user_id: String,
	joined_at: Option<i64>,
}

#[derive(Debug)]
pub struct Rustification {
	pool: SqlitePool,
	guild_id: serenity::GuildId,
	role_id: serenity::RoleId,
	delay_seconds: i64,
	wake: Arc<Notify>,
	worker: Mutex<Option<JoinHandle<()>>>,
}

impl Rustification {
	#[must_use]
	pub fn new(
		pool: SqlitePool,
		guild_id: serenity::GuildId,
		role_id: serenity::RoleId,
		config: RustificationConfig,
	) -> Self {
		Self {
			pool,
			guild_id,
			role_id,
			delay_seconds: i64::from(config.delay_seconds),
			wake: Arc::new(Notify::new()),
			worker: Mutex::new(None),
		}
	}

	#[tracing::instrument(name = "rustification.start", skip_all,
        fields(guild.id = self.guild_id.get(), role.id = self.role_id.get(),
            rustification.delay_seconds = self.delay_seconds, rustification.worker_started = false))]
	pub async fn start(self: &Arc<Self>, http: Arc<serenity::Http>) {
		let mut worker = self.worker.lock().await;
		if worker.as_ref().is_some_and(|task| !task.is_finished()) {
			return;
		}
		// The worker must not keep the service alive after the bot drops its Data.
		Span::current().record("rustification.worker_started", true);
		let service = Arc::downgrade(self);
		let wake = self.wake.clone();
		*worker = Some(tokio::spawn(async move {
			loop {
				let Some(service) = service.upgrade() else {
					return;
				};
				if let Err(error) = service.process_due(&http, now()).await {
					warn!(%error, "Failed to process Rustacean assignments");
				}
				let sleep = match service.next_wake().await {
					Ok(sleep) => sleep,
					Err(error) => {
						warn!(guild.id = service.guild_id.get(), %error, "Failed to load Rustacean worker deadline");
						Duration::from_secs(15)
					}
				};
				drop(service);
				tokio::select! {
					() = wake.notified() => {},
					() = tokio::time::sleep(sleep) => {},
				}
			}
		}));
	}

	/// Both `USER_JOIN` system messages and ordinary messages enter here.
	/// Role-bearing messages take this fast path without SQLite or HTTP access.
	/// A missing member payload is repaired via the worker's individual-member GET.
	pub async fn observe_message(&self, message: &serenity::Message) -> Result<()> {
		if message.guild_id != Some(self.guild_id)
			|| message.author.bot
			|| message.webhook_id.is_some()
			|| message
				.member
				.as_ref()
				.is_some_and(|m| m.roles.contains(&self.role_id))
		{
			return Ok(());
		}
		let span = tracing::info_span!("rustification.observe_message",
			guild.id = self.guild_id.get(), user.id = message.author.id.get(),
			channel.id = message.channel_id.get(), message.id = message.id.get(),
			rustification.source = if message.kind == serenity::MessageType::MemberJoin { "welcome" } else { "message" },
			otel.status_code = tracing::field::Empty, error.type = tracing::field::Empty,
		);
		async {
			let joined_at = message
				.member
				.as_ref()
				.and_then(|m| m.joined_at)
				.map(|timestamp| timestamp.unix_timestamp());
			let result = self.enqueue(message.author.id, joined_at, now()).await;
			if result.is_err() {
				record_failure("rustification.queue");
			}
			result
		}
		.instrument(span)
		.await
	}

	#[tracing::instrument(name = "rustification.enqueue", skip_all,
        fields(guild.id = self.guild_id.get(), user.id = user_id.get(),
            rustification.delay_seconds = self.delay_seconds,
            rustification.queue_changed = tracing::field::Empty,
            otel.status_code = tracing::field::Empty, error.type = tracing::field::Empty))]
	async fn enqueue(
		&self,
		user_id: serenity::UserId,
		joined_at: Option<i64>,
		now: i64,
	) -> Result<()> {
		// Duplicate observations never reset a retry deadline or the original wait.
		// A known new join replaces the previous membership's deadline.
		let result = sqlite_query(
			"INSERT",
			"rustification_queue",
			sqlx::query(
				"INSERT INTO rustification_queue (guild_id, user_id, joined_at, next_attempt_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT (guild_id, user_id) DO UPDATE SET
                joined_at = excluded.joined_at, next_attempt_at = excluded.next_attempt_at
             WHERE excluded.joined_at IS NOT NULL
                AND rustification_queue.joined_at IS NOT excluded.joined_at",
			)
			.bind(self.guild_id.get().to_string())
			.bind(user_id.get().to_string())
			.bind(joined_at)
			.bind(joined_at.map_or(now, |joined| joined + self.delay_seconds))
			.execute(&self.pool),
		)
		.await
		.inspect_err(|_| record_failure("rustification.queue"))?;
		Span::current().record("rustification.queue_changed", result.rows_affected() != 0);
		if result.rows_affected() != 0 {
			self.wake.notify_one();
		}
		Ok(())
	}

	#[tracing::instrument(name = "rustification.next_wake", skip_all,
        fields(guild.id = self.guild_id.get(), rustification.sleep_seconds = tracing::field::Empty,
            otel.status_code = tracing::field::Empty, error.type = tracing::field::Empty))]
	async fn next_wake(&self) -> Result<Duration> {
		let due = sqlite_query(
			"SELECT",
			"rustification_queue",
			sqlx::query_scalar::<_, Option<i64>>(
				"SELECT MIN(next_attempt_at) FROM rustification_queue WHERE guild_id = ?",
			)
			.bind(self.guild_id.get().to_string())
			.fetch_one(&self.pool),
		)
		.await
		.inspect_err(|_| record_failure("rustification.queue"))?;
		let seconds = due.map_or(15, |due| (due - now()).clamp(1, 15));
		Span::current().record("rustification.sleep_seconds", seconds);
		Ok(Duration::from_secs(seconds.unsigned_abs()))
	}

	#[tracing::instrument(name = "rustification.process_due", parent = None, skip_all,
        fields(guild.id = self.guild_id.get(), role.id = self.role_id.get(),
            rustification.assignments = tracing::field::Empty,
            rustification.failures = tracing::field::Empty,
            otel.status_code = tracing::field::Empty, error.type = tracing::field::Empty))]
	async fn process_due(&self, http: &serenity::Http, now: i64) -> Result<()> {
		let assignments = sqlite_query(
			"SELECT",
			"rustification_queue",
			sqlx::query_as::<_, Assignment>(
				"SELECT user_id, joined_at FROM rustification_queue
             WHERE guild_id = ? AND next_attempt_at <= ? ORDER BY next_attempt_at LIMIT ?",
			)
			.bind(self.guild_id.get().to_string())
			.bind(now)
			.bind(BATCH_SIZE)
			.fetch_all(&self.pool),
		)
		.await;
		let assignments = match assignments {
			Ok(assignments) => assignments,
			Err(error) => {
				record_failure("rustification.queue");
				return Err(error.into());
			}
		};
		Span::current().record("rustification.assignments", assignments.len() as i64);
		let mut failures = 0_i64;
		for assignment in assignments {
			let span = tracing::info_span!("rustification.assignment",
				guild.id = self.guild_id.get(), user.id = %assignment.user_id,
				role.id = self.role_id.get(), rustification.delay_seconds = self.delay_seconds,
				rustification.outcome = tracing::field::Empty,
				rustification.retry_at = now + RETRY,
				rustification.next_attempt_at = tracing::field::Empty,
				otel.status_code = tracing::field::Empty, error.type = tracing::field::Empty,
			);
			let result = async {
				// Persist the retry inside the assignment span before any HTTP operation.
				let result = async {
					self.reschedule(&assignment, assignment.joined_at, now + RETRY)
						.await?;
					self.assign(http, &assignment, now).await
				}
				.await;
				if let Ok(outcome) = &result {
					Span::current().record("rustification.outcome", outcome);
				} else {
					record_failure("rustification.assignment");
					Span::current().record("rustification.outcome", "retry");
					Span::current().record("rustification.next_attempt_at", now + RETRY);
					warn!("Rustacean assignment failed; will retry");
				}
				result
			}
			.instrument(span)
			.await;
			if result.is_err() {
				failures += 1;
			}
		}
		Span::current().record("rustification.failures", failures);
		if failures != 0 {
			record_failure("rustification.assignment");
		}
		Ok(())
	}

	async fn assign(
		&self,
		http: &serenity::Http,
		assignment: &Assignment,
		now: i64,
	) -> Result<&'static str> {
		let user_id = serenity::UserId::new(assignment.user_id.parse()?);
		let member = match discord_operation(
			"guild.member",
			http.get_member(self.guild_id, user_id),
		)
		.await
		{
			Ok(member) => member,
			Err(serenity::Error::Http(serenity::HttpError::UnsuccessfulRequest(ref response)))
				if response.status_code == reqwest::StatusCode::NOT_FOUND
					&& matches!(response.error.code, 10007 | 10013) =>
			{
				self.remove(assignment).await?;
				return Ok("departed");
			}
			Err(error) => return Err(error.into()),
		};
		if member.user.bot || member.roles.contains(&self.role_id) {
			self.remove(assignment).await?;
			return Ok(if member.user.bot {
				"bot"
			} else {
				"already_has_role"
			});
		}
		let Some(joined_at) = member.joined_at.map(|timestamp| timestamp.unix_timestamp()) else {
			Span::current().record("rustification.next_attempt_at", now + RETRY);
			return Ok("unknown_join_time"); // Retain the retry; never grant early.
		};
		if joined_at + self.delay_seconds > now || member.pending {
			let due = if member.pending {
				(joined_at + self.delay_seconds).max(now + RETRY)
			} else {
				joined_at + self.delay_seconds
			};
			self.reschedule(assignment, Some(joined_at), due).await?;
			Span::current().record("rustification.next_attempt_at", due);
			return Ok(if member.pending {
				"screening_pending"
			} else {
				"waiting"
			});
		}
		discord_operation(
			"member.add_role",
			http.add_member_role(
				self.guild_id,
				user_id,
				self.role_id,
				Some(&format!(
					"Automatically rustified after {} seconds",
					self.delay_seconds
				)),
			),
		)
		.await?;
		self.remove(assignment).await?;
		Ok("assigned")
	}

	async fn remove(&self, assignment: &Assignment) -> Result<()> {
		// Do not remove a newer join observed while the HTTP request was in flight.
		sqlite_query(
			"DELETE",
			"rustification_queue",
			sqlx::query(
				"DELETE FROM rustification_queue WHERE guild_id = ? AND user_id = ? AND joined_at IS ?",
			)
			.bind(self.guild_id.get().to_string())
			.bind(&assignment.user_id)
			.bind(assignment.joined_at)
			.execute(&self.pool),
		)
		.await?;
		Ok(())
	}

	async fn reschedule(
		&self,
		assignment: &Assignment,
		joined_at: Option<i64>,
		due: i64,
	) -> Result<()> {
		sqlite_query(
			"UPDATE",
			"rustification_queue",
			sqlx::query(
				"UPDATE rustification_queue SET joined_at = ?, next_attempt_at = ?
                     WHERE guild_id = ? AND user_id = ? AND joined_at IS ?",
			)
			.bind(joined_at)
			.bind(due)
			.bind(self.guild_id.get().to_string())
			.bind(&assignment.user_id)
			.bind(assignment.joined_at)
			.execute(&self.pool),
		)
		.await?;
		Ok(())
	}
}

impl Drop for Rustification {
	fn drop(&mut self) {
		if let Some(worker) = self.worker.get_mut().take() {
			worker.abort();
		}
	}
}

fn record_failure(kind: &'static str) {
	Span::current().record("otel.status_code", "ERROR");
	Span::current().record("error.type", kind);
}

fn now() -> i64 {
	serenity::Timestamp::now().unix_timestamp()
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;
	use sqlx::sqlite::SqlitePoolOptions;
	use wiremock::{
		Mock, MockServer, ResponseTemplate,
		matchers::{method, path},
	};

	const MEMBER_PATH: &str = "/api/v10/guilds/1/members/2";
	const ROLE_PATH: &str = "/api/v10/guilds/1/members/2/roles/3";
	const DELAY: i64 = 30 * 60;
	const JOINED: i64 = 1_700_000_000;

	pub(super) async fn service() -> Rustification {
		let pool = SqlitePoolOptions::new()
			.max_connections(1)
			.connect("sqlite::memory:")
			.await
			.unwrap();
		sqlx::migrate!().run(&pool).await.unwrap();
		Rustification::new(
			pool,
			serenity::GuildId::new(1),
			serenity::RoleId::new(3),
			RustificationConfig::default(),
		)
	}

	pub(super) fn http(server: &MockServer) -> serenity::Http {
		serenity::HttpBuilder::new("test-token")
			.proxy(server.uri())
			.ratelimiter_disabled(true)
			.client(reqwest::Client::builder().no_proxy().build().unwrap())
			.build()
	}

	pub(super) fn message(joined_at: Option<i64>, has_role: bool) -> serenity::Message {
		let mut message = serenity::Message::default();
		message.guild_id = Some(serenity::GuildId::new(1));
		message.author.id = serenity::UserId::new(2);
		// Empty content deliberately models a bot without Message Content access.
		message.member = Some(Box::new(serde_json::from_value(json!({
            "roles": if has_role { vec!["3"] } else { vec![] },
            "joined_at": joined_at.map(|t| serenity::Timestamp::from_unix_timestamp(t).unwrap().to_string()),
        })).unwrap()));
		message
	}

	pub(super) fn member(
		joined_at: Option<i64>,
		has_role: bool,
		pending: bool,
	) -> serde_json::Value {
		json!({
			"user": { "id": "2", "username": "newcomer", "discriminator": "0", "avatar": null },
			"roles": if has_role { vec!["3"] } else { vec![] },
			"joined_at": joined_at.map(|t| serenity::Timestamp::from_unix_timestamp(t).unwrap().to_string()),
			"deaf": false, "mute": false, "flags": 0, "pending": pending,
		})
	}

	async fn queue(service: &Rustification) -> Vec<(Option<i64>, i64)> {
		sqlx::query_as(
			"SELECT joined_at, next_attempt_at FROM rustification_queue ORDER BY user_id",
		)
		.fetch_all(&service.pool)
		.await
		.unwrap()
	}

	pub(super) async fn mock_member(server: &MockServer, body: serde_json::Value, count: u64) {
		Mock::given(method("GET"))
			.and(path(MEMBER_PATH))
			.respond_with(ResponseTemplate::new(200).set_body_json(body))
			.expect(count)
			.mount(server)
			.await;
	}

	pub(super) async fn mock_role(server: &MockServer, status: u16, count: u64) {
		Mock::given(method("PUT"))
			.and(path(ROLE_PATH))
			.respond_with(
				ResponseTemplate::new(status)
					.set_body_json(json!({"code": 50013, "message": "Missing permissions"})),
			)
			.expect(count)
			.mount(server)
			.await;
	}

	#[tokio::test]
	async fn ordinary_message_repairs_a_join_missed_during_downtime() {
		let service = service().await;
		let server = MockServer::start().await;
		service
			.observe_message(&message(Some(JOINED), false))
			.await
			.unwrap();
		mock_member(&server, member(Some(JOINED), false, false), 1).await;
		mock_role(&server, 204, 1).await;
		service
			.process_due(&http(&server), JOINED + DELAY)
			.await
			.unwrap();
		assert_eq!(queue(&service).await, []);
		// All HTTP mocks assert exactly one GET and one role PUT.
		server.verify().await;
	}

	#[tokio::test]
	async fn welcome_and_repeated_messages_do_not_reset_the_wait() {
		let service = service().await;
		let server = MockServer::start().await;
		let mut welcome = message(Some(JOINED), false);
		welcome.kind = serenity::MessageType::MemberJoin;
		service.observe_message(&welcome).await.unwrap();
		for _ in 0..10 {
			service
				.observe_message(&message(Some(JOINED), false))
				.await
				.unwrap();
		}
		assert_eq!(queue(&service).await, vec![(Some(JOINED), JOINED + DELAY)]);
		service
			.process_due(&http(&server), JOINED + DELAY - 1)
			.await
			.unwrap();
		assert!(server.received_requests().await.unwrap().is_empty());
		mock_member(&server, member(Some(JOINED), false, false), 1).await;
		mock_role(&server, 204, 1).await;
		service
			.process_due(&http(&server), JOINED + DELAY)
			.await
			.unwrap();
		service
			.process_due(&http(&server), JOINED + DELAY + 1)
			.await
			.unwrap();
		assert_eq!(queue(&service).await, []);
		server.verify().await;
	}

	#[tokio::test]
	async fn already_rustified_messages_do_not_access_sqlite() {
		let service = service().await;
		service.pool.close().await;
		for _ in 0..100 {
			service
				.observe_message(&message(Some(JOINED), true))
				.await
				.unwrap();
		}
	}

	#[tokio::test]
	async fn bots_webhooks_other_guilds_and_dms_are_ignored() {
		let service = service().await;
		service.pool.close().await;
		let original = message(Some(JOINED), false);
		let mut bot = original.clone();
		bot.author.bot = true;
		let mut webhook = original.clone();
		webhook.webhook_id = Some(serenity::WebhookId::new(4));
		let mut other_guild = original.clone();
		other_guild.guild_id = Some(serenity::GuildId::new(4));
		let mut dm = original;
		dm.guild_id = None;
		for message in [bot, webhook, other_guild, dm] {
			service.observe_message(&message).await.unwrap();
		}
	}

	#[tokio::test]
	async fn pending_assignments_survive_a_service_restart() {
		let directory = tempfile::tempdir().unwrap();
		let options = sqlx::sqlite::SqliteConnectOptions::new()
			.filename(directory.path().join("roles.sqlite3"))
			.create_if_missing(true);
		let pool = SqlitePoolOptions::new()
			.max_connections(1)
			.connect_with(options.clone())
			.await
			.unwrap();
		sqlx::migrate!().run(&pool).await.unwrap();
		let service = Rustification::new(
			pool.clone(),
			serenity::GuildId::new(1),
			serenity::RoleId::new(3),
			RustificationConfig::default(),
		);
		service
			.enqueue(serenity::UserId::new(2), Some(JOINED), JOINED)
			.await
			.unwrap();
		drop(service);
		pool.close().await;
		let reopened = SqlitePoolOptions::new()
			.max_connections(1)
			.connect_with(options)
			.await
			.unwrap();
		let restored = Rustification::new(
			reopened,
			serenity::GuildId::new(1),
			serenity::RoleId::new(3),
			RustificationConfig::default(),
		);
		let server = MockServer::start().await;
		mock_member(&server, member(Some(JOINED), false, false), 1).await;
		mock_role(&server, 204, 1).await;
		restored
			.process_due(&http(&server), JOINED + DELAY + 500)
			.await
			.unwrap();
		assert_eq!(queue(&restored).await, []);
		server.verify().await;
	}

	#[tokio::test]
	async fn rejoin_detected_by_worker_resets_the_deadline() {
		let service = service().await;
		service
			.enqueue(serenity::UserId::new(2), Some(JOINED), JOINED)
			.await
			.unwrap();
		let rejoined = JOINED + DELAY - 10;
		let server = MockServer::start().await;
		mock_member(&server, member(Some(rejoined), false, false), 2).await;
		service
			.process_due(&http(&server), JOINED + DELAY)
			.await
			.unwrap();
		assert_eq!(
			queue(&service).await,
			vec![(Some(rejoined), rejoined + DELAY)]
		);
		service
			.process_due(&http(&server), rejoined + DELAY - 1)
			.await
			.unwrap();
		mock_role(&server, 204, 1).await;
		service
			.process_due(&http(&server), rejoined + DELAY)
			.await
			.unwrap();
		assert_eq!(queue(&service).await, []);
		server.verify().await;
	}

	#[tokio::test]
	async fn a_new_join_observed_during_processing_is_not_deleted_by_old_work() {
		let service = service().await;
		service
			.enqueue(serenity::UserId::new(2), Some(JOINED), JOINED)
			.await
			.unwrap();
		let old = Assignment {
			user_id: "2".into(),
			joined_at: Some(JOINED),
		};
		let rejoined = JOINED + 100;
		service
			.enqueue(serenity::UserId::new(2), Some(rejoined), rejoined)
			.await
			.unwrap();
		service.remove(&old).await.unwrap();
		service
			.reschedule(&old, old.joined_at, JOINED + RETRY)
			.await
			.unwrap();
		assert_eq!(
			queue(&service).await,
			vec![(Some(rejoined), rejoined + DELAY)]
		);
	}

	#[tokio::test]
	async fn a_departed_member_is_removed_from_the_queue() {
		let service = service().await;
		service
			.enqueue(serenity::UserId::new(2), Some(JOINED), JOINED)
			.await
			.unwrap();
		let server = MockServer::start().await;
		Mock::given(method("GET"))
			.and(path(MEMBER_PATH))
			.respond_with(
				ResponseTemplate::new(404)
					.set_body_json(json!({"code": 10007, "message": "Unknown Member"})),
			)
			.expect(1)
			.mount(&server)
			.await;
		service
			.process_due(&http(&server), JOINED + DELAY)
			.await
			.unwrap();
		assert_eq!(queue(&service).await, []);
		server.verify().await;
	}

	#[tokio::test]
	async fn an_existing_role_is_not_granted_again() {
		let service = service().await;
		service
			.enqueue(serenity::UserId::new(2), Some(JOINED), JOINED)
			.await
			.unwrap();
		let server = MockServer::start().await;
		mock_member(&server, member(Some(JOINED), true, false), 1).await;
		service
			.process_due(&http(&server), JOINED + DELAY)
			.await
			.unwrap();
		assert_eq!(queue(&service).await, []);
		assert_eq!(server.received_requests().await.unwrap().len(), 1);
		server.verify().await;
	}

	#[tokio::test]
	async fn missing_message_member_data_is_fetched_and_scheduled_safely() {
		let service = service().await;
		let mut welcome = message(None, false);
		welcome.member = None;
		welcome.kind = serenity::MessageType::MemberJoin;
		service.observe_message(&welcome).await.unwrap();
		let current = now();
		let joined = current - 100;
		let server = MockServer::start().await;
		mock_member(&server, member(Some(joined), false, false), 1).await;
		service.process_due(&http(&server), current).await.unwrap();
		assert_eq!(queue(&service).await, vec![(Some(joined), joined + DELAY)]);
		assert_eq!(server.received_requests().await.unwrap().len(), 1);
		server.verify().await;
	}

	#[tokio::test]
	async fn screening_and_unknown_join_times_never_grant_a_role() {
		for (joined, pending) in [(Some(JOINED), true), (None, false)] {
			let service = service().await;
			service
				.enqueue(serenity::UserId::new(2), Some(JOINED), JOINED)
				.await
				.unwrap();
			let server = MockServer::start().await;
			mock_member(&server, member(joined, false, pending), 1).await;
			service
				.process_due(&http(&server), JOINED + DELAY)
				.await
				.unwrap();
			assert_eq!(
				queue(&service).await,
				vec![(Some(JOINED), JOINED + DELAY + RETRY)]
			);
			assert_eq!(server.received_requests().await.unwrap().len(), 1);
			server.verify().await;
		}
	}

	#[tokio::test]
	async fn failed_role_assignment_retries_without_message_spam_resetting_backoff() {
		let service = service().await;
		service
			.enqueue(serenity::UserId::new(2), Some(JOINED), JOINED)
			.await
			.unwrap();
		let server = MockServer::start().await;
		let client = http(&server);
		mock_member(&server, member(Some(JOINED), false, false), 1).await;
		mock_role(&server, 403, 1).await;
		let due = JOINED + DELAY;
		service.process_due(&client, due).await.unwrap();
		for _ in 0..10 {
			service
				.observe_message(&message(Some(JOINED), false))
				.await
				.unwrap();
		}
		assert_eq!(queue(&service).await, vec![(Some(JOINED), due + RETRY)]);
		service.process_due(&client, due + RETRY - 1).await.unwrap();
		server.verify().await;
		server.reset().await;
		mock_member(&server, member(Some(JOINED), false, false), 1).await;
		mock_role(&server, 204, 1).await;
		service.process_due(&client, due + RETRY).await.unwrap();
		assert_eq!(queue(&service).await, []);
		server.verify().await;
	}

	#[tokio::test]
	async fn member_fetch_failure_preserves_the_assignment() {
		let service = service().await;
		service
			.enqueue(serenity::UserId::new(2), Some(JOINED), JOINED)
			.await
			.unwrap();
		let server = MockServer::start().await;
		Mock::given(method("GET"))
			.and(path(MEMBER_PATH))
			.respond_with(
				ResponseTemplate::new(403)
					.set_body_json(json!({"code": 50001, "message": "Missing Access"})),
			)
			.expect(1)
			.mount(&server)
			.await;
		service
			.process_due(&http(&server), JOINED + DELAY)
			.await
			.unwrap();
		assert_eq!(
			queue(&service).await,
			vec![(Some(JOINED), JOINED + DELAY + RETRY)]
		);
		server.verify().await;
	}
	#[tokio::test]
	async fn repeated_ready_events_start_only_one_worker_and_drop_stops_it() {
		let service = Arc::new(service().await);
		let server = MockServer::start().await;
		let joined = now() - DELAY - 10;
		mock_member(&server, member(Some(joined), false, false), 1).await;
		mock_role(&server, 204, 1).await;
		let client = Arc::new(http(&server));
		service.start(client.clone()).await;
		let worker_id = service.worker.lock().await.as_ref().unwrap().id();
		service.start(client).await;
		assert_eq!(
			service.worker.lock().await.as_ref().unwrap().id(),
			worker_id
		);
		// Enqueuing after startup wakes the sleeping worker; no 30-minute sleep in tests.
		service
			.observe_message(&message(Some(joined), false))
			.await
			.unwrap();
		tokio::time::timeout(Duration::from_secs(2), async {
			while !queue(&service).await.is_empty() {
				tokio::task::yield_now().await;
			}
		})
		.await
		.expect("worker did not process the new assignment");
		server.verify().await;
		let weak = Arc::downgrade(&service);
		drop(service);
		tokio::time::timeout(Duration::from_secs(2), async {
			while weak.upgrade().is_some() {
				tokio::task::yield_now().await;
			}
		})
		.await
		.expect("worker retained service after shutdown");
	}

	#[tokio::test]
	async fn configured_delay_controls_both_queue_deadline_and_eligibility() {
		for delay_seconds in [0, 2, 3600] {
			let base = service().await;
			let service = Rustification::new(
				base.pool.clone(),
				base.guild_id,
				base.role_id,
				RustificationConfig { delay_seconds },
			);
			let delay = i64::from(delay_seconds);
			let server = MockServer::start().await;
			let client = http(&server);
			service
				.enqueue(serenity::UserId::new(2), Some(JOINED), JOINED)
				.await
				.unwrap();
			assert_eq!(queue(&service).await, vec![(Some(JOINED), JOINED + delay)]);
			service
				.process_due(&client, JOINED + delay - 1)
				.await
				.unwrap();
			assert!(server.received_requests().await.unwrap().is_empty());
			mock_member(&server, member(Some(JOINED), false, false), 1).await;
			mock_role(&server, 204, 1).await;
			service.process_due(&client, JOINED + delay).await.unwrap();
			assert_eq!(queue(&service).await, []);
			server.verify().await;
		}
	}
}

#[cfg(test)]
mod telemetry_tests;
