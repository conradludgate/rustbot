//! Each case owns a bot, a Discord guild, SQLite, and its upstream HTTP mocks.

mod fauxcord;

use std::{
	fs,
	path::{Path, PathBuf},
	sync::Arc,
	time::Duration,
};

use anyhow::{Context, Result, bail};
use ferrisbot_for_discord::{BotConfig, build_bot, types::ExternalApiBases};
use poise::serenity_prelude as serenity;
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use wiremock::{
	Mock, MockServer, Request, ResponseTemplate,
	matchers::{method, path},
};

use fauxcord::TestDiscord;

const REPLY_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const PLAYGROUND_PROGRESS: &str = "_Running code on playground..._";

pub struct TestBot {
	pub api: MockServer,

	discord: TestDiscord,
	database: SqlitePool,
	bot_task: Option<JoinHandle<Result<(), serenity::Error>>>,
	shard_manager: Arc<serenity::ShardManager>,

	// Keep the copied images available until the bot has stopped.
	_icon_files: TempDir,
}

pub struct CommandResponse {
	pub reply: String,
	pub requests: Vec<Request>,
}

impl TestBot {
	pub async fn start() -> Result<Self> {
		let api = MockServer::start().await;
		let discord = TestDiscord::register().await?;
		let icon_files = prepare_icon_files()?;
		let database = prepare_database(&discord.guild_id, icon_files.path()).await?;

		let config = BotConfig {
			secret_store: discord.secrets(),
			database: Some(database.clone()),
			intents: serenity::GatewayIntents::non_privileged()
				| serenity::GatewayIntents::GUILD_MEMBERS
				| serenity::GatewayIntents::MESSAGE_CONTENT,
			discord_api_proxy: Some(discord.base_url.clone()),
			external_apis: ExternalApiBases::new(api.uri(), api.uri()),
			server_icon_directory: icon_files.path().to_owned(),
		};

		let mut bot = build_bot(config).await?;
		let shard_manager = bot.shard_manager.clone();
		let bot_task = tokio::spawn(async move { bot.start().await });

		let mut test = Self {
			api,
			discord,
			database,
			bot_task: Some(bot_task),
			shard_manager,
			_icon_files: icon_files,
		};

		// Production setup posts the modmail prompt after registering commands.
		let modmail_channel = test.discord.modmail_channel.clone();
		test.wait_for_message(&modmail_channel, |_| true)
			.await
			.context("waiting for bot setup")?;

		Ok(test)
	}

	/// Each bot has a cold cache and must fetch both metadata lists.
	pub async fn mock_godbolt_metadata(&self) {
		for (endpoint, name) in [
			("/api/compilers/rust", "godbolt-compilers-rust.json"),
			("/api/libraries/rust", "godbolt-libraries-rust.json"),
		] {
			Mock::given(method("GET"))
				.and(path(endpoint))
				.respond_with(fixture(name))
				.expect(1)
				.mount(&self.api)
				.await;
		}
	}

	/// Run one command without a prefix, clean up its bot, then verify HTTP mocks.
	pub async fn send(mut self, command: &str) -> Result<CommandResponse> {
		let reply = self.run_command(command).await;

		// Clean up even when injection or polling fails; report the command error first.
		self.stop_bot().await;
		let cleanup = self.discord.cleanup().await;

		let reply = reply.with_context(|| format!("command: {command}"))?;
		cleanup.context("removing the Fauxcord test setup")?;

		let requests = self
			.api
			.received_requests()
			.await
			.context("WireMock request recording disabled")?;

		self.api.verify().await;

		Ok(CommandResponse { reply, requests })
	}

	async fn run_command(&mut self, command: &str) -> Result<String> {
		self.discord.inject_command(command).await?;

		let channel = self.discord.command_channel.clone();
		self.wait_for_message(&channel, is_final_reply).await
	}

	async fn wait_for_message(
		&mut self,
		channel: &str,
		accept: fn(&str) -> bool,
	) -> Result<String> {
		let deadline = tokio::time::Instant::now() + REPLY_TIMEOUT;

		loop {
			let replies = self.discord.bot_messages(channel).await?;

			if let Some(reply) = replies.iter().find(|content| accept(content)) {
				return Ok(reply.clone());
			}

			if self.bot_task.as_ref().is_some_and(JoinHandle::is_finished) {
				let result = self
					.bot_task
					.take()
					.context("test bot already stopped")?
					.await
					.context("test bot task panicked")?;

				bail!("bot stopped before replying: {result:?}");
			}

			if tokio::time::Instant::now() >= deadline {
				bail!(
					"no bot message within {REPLY_TIMEOUT:?}\nobserved replies: {replies:#?}\nupstream requests: {:#?}",
					self.api.received_requests().await,
				);
			}

			tokio::time::sleep(POLL_INTERVAL).await;
		}
	}

	async fn stop_bot(&mut self) {
		// Bound Gateway shutdown so a stuck connection cannot prevent cancellation.
		let _ =
			tokio::time::timeout(Duration::from_secs(5), self.shard_manager.shutdown_all()).await;

		if let Some(task) = self.bot_task.take() {
			task.abort();
			let _ = task.await;
		}

		self.database.close().await;
	}
}

impl Drop for TestBot {
	fn drop(&mut self) {
		// On panic or failed startup, the test runtime also drops remaining tasks.
		if let Some(task) = self.bot_task.take() {
			task.abort();
		}
	}
}

// Current cases have a Playground placeholder followed by one final reply.
// A reply is final when it no longer starts with the progress text.
// This heuristic only handles one result; polling never uses the expected snapshot.
fn is_final_reply(content: &str) -> bool {
	!content.starts_with(PLAYGROUND_PROGRESS)
}

/// Replay the captured HTTP status and JSON body exactly as stored.
pub fn fixture(name: &str) -> ResponseTemplate {
	let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
	let body =
		fs::read(directory.join(name)).unwrap_or_else(|error| panic!("fixture {name}: {error}"));

	let status = fs::read_to_string(directory.join(format!("{name}.status")))
		.unwrap_or_else(|error| panic!("fixture status {name}: {error}"))
		.trim()
		.parse::<u16>()
		.expect("fixture status must be an HTTP status code");

	ResponseTemplate::new(status).set_body_raw(body, "application/json")
}

fn prepare_icon_files() -> Result<TempDir> {
	let files = tempfile::tempdir()?;
	let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/server-icons");

	for name in ["server_icon_ferris.png", "server_icon_owo.png"] {
		fs::copy(source.join(name), files.path().join(name))?;
	}

	Ok(files)
}

async fn prepare_database(guild_id: &str, icons: &Path) -> Result<SqlitePool> {
	let database = SqlitePoolOptions::new()
		.max_connections(1)
		.connect("sqlite::memory:")
		.await?;

	sqlx::migrate!().run(&database).await?;

	// Seed a known icon and pause rotation to avoid random choices and deadlines.
	sqlx::query(
		"INSERT INTO server_icon_rotation
		 (guild_id, next_rotation_at, paused_at, current_icon_path)
		 VALUES (?, 4102444800, 1, ?)",
	)
	.bind(guild_id)
	.bind(icons.join("server_icon_owo.png").to_string_lossy().as_ref())
	.execute(&database)
	.await?;

	Ok(database)
}
