use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Error, anyhow, bail};
use poise::serenity_prelude as serenity;
use rand::{Rng, seq::IteratorRandom};
use sqlx::{FromRow, SqlitePool};
use tokio::{
	sync::{Mutex, Notify},
	task::JoinHandle,
};
use tracing::{Instrument, info, warn};

use crate::types::Context;

const GITHUB_ICON_URL: &str =
	"https://github.com/conradludgate/rustbot/blob/main/assets/server-icons/";
const ICON_CHANGE_TIMEOUT: Duration = Duration::from_secs(60);
const ROTATION_MIN_SECONDS: u64 = 60 * 60 * 24;
const ROTATION_MAX_SECONDS: u64 = 60 * 60 * 48;

#[derive(Clone, Debug, PartialEq, Eq)]
struct IconChoice {
	suffix: String,
	path: String,
	filename: String,
}

#[derive(Debug, FromRow)]
struct RotationState {
	next_rotation_at: i64,
	paused_at: Option<i64>,
	pending_icon_path: Option<String>,
	current_icon_path: Option<String>,
}

#[derive(Debug)]
pub struct ServerIconRotation {
	icon_directory: PathBuf,
	pool: SqlitePool,
	guild_id: serenity::GuildId,
	wake: Arc<Notify>,
	operation_lock: Arc<Mutex<()>>,
	worker: Mutex<Option<JoinHandle<()>>>,
}

impl ServerIconRotation {
	#[must_use]
	pub fn new(pool: SqlitePool, guild_id: serenity::GuildId, icon_directory: PathBuf) -> Self {
		Self {
			icon_directory,
			pool,
			guild_id,
			wake: Arc::new(Notify::new()),
			operation_lock: Arc::new(Mutex::new(())),
			worker: Mutex::new(None),
		}
	}

	pub async fn start(&self, http: Arc<serenity::Http>) {
		let mut worker = self.worker.lock().await;
		if worker.as_ref().is_some_and(|task| !task.is_finished()) {
			return;
		}

		let icon_directory = self.icon_directory.clone();
		let pool = self.pool.clone();
		let guild_id = self.guild_id;
		let wake = self.wake.clone();
		let operation_lock = self.operation_lock.clone();
		info!(
			guild.id = guild_id.get(),
			"Starting server icon rotation worker"
		);
		*worker = Some(tokio::spawn(async move {
			run_rotation_worker(http, pool, guild_id, wake, operation_lock, icon_directory).await;
		}));
	}

	#[tracing::instrument(name = "server_icon_rotation.status", skip_all, err(Debug))]
	async fn status(&self) -> Result<RotationStatus, Error> {
		let state = ensure_state(&self.pool, self.guild_id).await?;
		Ok(RotationStatus {
			paused: state.paused_at.is_some(),
			next_rotation_at: state.next_rotation_at,
			current_icon_path: state.current_icon_path,
		})
	}

	#[tracing::instrument(name = "server_icon_rotation.pause", skip_all, err(Debug))]
	async fn pause(&self) -> Result<bool, Error> {
		let _lock = self.operation_lock.lock().await;
		let state = ensure_state(&self.pool, self.guild_id).await?;
		if state.paused_at.is_some() {
			return Ok(false);
		}

		sqlx::query("UPDATE server_icon_rotation SET paused_at = ? WHERE guild_id = ?")
			.bind(now_seconds())
			.bind(self.guild_id.get().to_string())
			.execute(&self.pool)
			.await
			.context("Failed to pause server icon rotation")?;
		self.wake.notify_one();
		Ok(true)
	}

	#[tracing::instrument(name = "server_icon_rotation.resume", skip_all, err(Debug))]
	async fn resume(&self) -> Result<bool, Error> {
		let _lock = self.operation_lock.lock().await;
		let state = ensure_state(&self.pool, self.guild_id).await?;
		let Some(paused_at) = state.paused_at else {
			return Ok(false);
		};

		let next_rotation_at = resume_deadline(state.next_rotation_at, paused_at, now_seconds());
		sqlx::query(
			"UPDATE server_icon_rotation SET paused_at = NULL, next_rotation_at = ? WHERE guild_id = ?",
		)
		.bind(next_rotation_at)
		.bind(self.guild_id.get().to_string())
		.execute(&self.pool)
		.await
		.context("Failed to resume server icon rotation")?;
		self.wake.notify_one();
		Ok(true)
	}

	#[tracing::instrument(
		name = "server_icon_rotation.set_icon",
		skip_all,
		fields(icon.suffix = %suffix),
		err(Debug),
	)]
	async fn set_icon(
		&self,
		ctx: &impl serenity::CacheHttp,
		suffix: &str,
	) -> Result<IconChoice, Error> {
		let choices = icon_choices(&self.icon_directory).await?;
		let Some(choice) = choices.into_iter().find(|choice| choice.suffix == suffix) else {
			bail!("Unknown icon `{suffix}`. Use `/server_icon list` to see the available names.");
		};

		let _lock = self.operation_lock.lock().await;
		let state = ensure_state(&self.pool, self.guild_id).await?;
		sqlx::query("UPDATE server_icon_rotation SET pending_icon_path = ? WHERE guild_id = ?")
			.bind(&choice.path)
			.bind(self.guild_id.get().to_string())
			.execute(&self.pool)
			.await
			.context("Failed to save the requested server icon")?;

		match apply_icon(ctx, self.guild_id, &choice.path, &self.icon_directory).await {
			Ok(()) => {
				complete_pending_icon(
					&self.pool,
					self.guild_id,
					&choice.path,
					state.paused_at.is_some(),
				)
				.await?;
			}
			Err(error) => {
				self.wake.notify_one();
				return Err(error)
					.context("Failed to apply the requested icon; it will be retried");
			}
		}

		self.wake.notify_one();
		Ok(choice)
	}
}

impl Drop for ServerIconRotation {
	fn drop(&mut self) {
		if let Some(worker) = self.worker.get_mut().take() {
			worker.abort();
		}
	}
}

#[derive(Debug)]
pub struct RotationStatus {
	pub paused: bool,
	pub next_rotation_at: i64,
	pub current_icon_path: Option<String>,
}

async fn run_rotation_worker(
	http: Arc<serenity::Http>,
	pool: SqlitePool,
	guild_id: serenity::GuildId,
	wake: Arc<Notify>,
	operation_lock: Arc<Mutex<()>>,
	icon_directory: PathBuf,
) {
	let mut consecutive_failures = 0u32;
	loop {
		let state = match ensure_state(&pool, guild_id).await {
			Ok(state) => state,
			Err(error) => {
				warn!(guild.id = guild_id.get(), error = %error, "Failed to load server icon rotation state");
				retry_delay(guild_id, &mut consecutive_failures).await;
				continue;
			}
		};

		let should_run = state.pending_icon_path.is_some()
			|| (state.paused_at.is_none() && state.next_rotation_at <= now_seconds());
		if should_run {
			let operation_guard = operation_lock.lock().await;
			let span = tracing::info_span!(
				"server_icon_rotation.scheduled_run",
				guild.id = guild_id.get(),
				requested_icon = tracing::field::Empty,
				icon.path = tracing::field::Empty,
			);
			match process_due_icon(&http, &pool, guild_id, &icon_directory)
				.instrument(span)
				.await
			{
				Ok(true) => consecutive_failures = 0,
				Ok(false) => {}
				Err(error) => {
					warn!(guild.id = guild_id.get(), error = %error, "Failed to change server icon");
					drop(operation_guard);
					retry_delay(guild_id, &mut consecutive_failures).await;
					continue;
				}
			}
			continue;
		}

		if state.paused_at.is_some() {
			wake.notified().await;
		} else {
			let remaining = state
				.next_rotation_at
				.saturating_sub(now_seconds())
				.max(0)
				.cast_unsigned();
			tokio::select! {
				() = wake.notified() => {},
				() = tokio::time::sleep(Duration::from_secs(remaining)) => {},
			}
		}
	}
}

#[tracing::instrument(
	name = "server_icon_rotation.process_due_icon",
	skip_all,
	fields(guild.id = guild_id.get()),
	err(Debug),
)]
async fn process_due_icon(
	http: &impl serenity::CacheHttp,
	pool: &SqlitePool,
	guild_id: serenity::GuildId,
	icon_directory: &Path,
) -> Result<bool, Error> {
	let state = ensure_state(pool, guild_id).await?;
	let path = if let Some(path) = state.pending_icon_path.clone() {
		path
	} else if state.paused_at.is_none() && state.next_rotation_at <= now_seconds() {
		choose_random_icon(icon_directory, state.current_icon_path.as_deref())
			.await?
			.ok_or_else(|| {
				anyhow!(
					"No server icons are available in {}",
					icon_directory.display()
				)
			})?
	} else {
		return Ok(false);
	};
	tracing::Span::current().record("requested_icon", state.pending_icon_path.is_some());
	tracing::Span::current().record("icon.path", tracing::field::display(&path));

	if state.pending_icon_path.is_none() {
		sqlx::query("UPDATE server_icon_rotation SET pending_icon_path = ? WHERE guild_id = ?")
			.bind(&path)
			.bind(guild_id.get().to_string())
			.execute(pool)
			.await
			.context("Failed to save the next server icon")?;
	}

	apply_icon(http, guild_id, &path, icon_directory).await?;
	complete_pending_icon(pool, guild_id, &path, state.paused_at.is_some()).await?;
	Ok(true)
}

#[tracing::instrument(
	name = "db.server_icon_rotation.complete_pending",
	skip_all,
	fields(guild.id = guild_id.get(), icon.path = %path),
	err(Debug),
)]
async fn complete_pending_icon(
	pool: &SqlitePool,
	guild_id: serenity::GuildId,
	path: &str,
	paused: bool,
) -> Result<(), Error> {
	let next_rotation_at = if paused {
		None
	} else {
		Some(now_seconds().saturating_add(random_rotation_delay()))
	};
	sqlx::query(
		"UPDATE server_icon_rotation
		 SET current_icon_path = ?, pending_icon_path = NULL,
	     next_rotation_at = COALESCE(?, next_rotation_at)
		 WHERE guild_id = ?",
	)
	.bind(path)
	.bind(next_rotation_at)
	.bind(guild_id.get().to_string())
	.execute(pool)
	.await
	.context("Failed to save the server icon rotation schedule")?;
	info!(icon_path = path, "Server icon changed");
	Ok(())
}

#[tracing::instrument(
	name = "server_icon_rotation.apply_icon",
	skip_all,
	fields(guild.id = guild_id.get(), icon.path = %path),
	err(Debug),
)]
async fn apply_icon(
	ctx: &impl serenity::CacheHttp,
	guild_id: serenity::GuildId,
	path: &str,
	icon_directory: &Path,
) -> Result<(), Error> {
	let catalog = icon_choices(icon_directory).await?;
	if !catalog.iter().any(|choice| choice.path == path) {
		bail!("The saved server icon path `{path}` is not in the icon catalog");
	}
	let timeout_result = tokio::time::timeout(ICON_CHANGE_TIMEOUT, async {
		let attachment = serenity::CreateAttachment::path(path)
			.instrument(tracing::info_span!("filesystem.read_server_icon", file.path = %path))
			.await?;
		guild_id
			.edit(ctx, serenity::EditGuild::new().icon(Some(&attachment)))
			.await?;
		Ok::<(), Error>(())
	})
	.await;
	match timeout_result {
		Ok(result) => result,
		Err(error) => Err(anyhow!("timed out after {ICON_CHANGE_TIMEOUT:?}: {error}")),
	}
}

#[tracing::instrument(
	name = "db.server_icon_rotation.ensure_state",
	skip_all,
	fields(guild.id = guild_id.get()),
	err(Debug),
)]
async fn ensure_state(
	pool: &SqlitePool,
	guild_id: serenity::GuildId,
) -> Result<RotationState, Error> {
	sqlx::query(
		"INSERT OR IGNORE INTO server_icon_rotation (guild_id, next_rotation_at) VALUES (?, ?)",
	)
	.bind(guild_id.get().to_string())
	.bind(now_seconds())
	.execute(pool)
	.await
	.context("Failed to initialize server icon rotation state")?;
	load_state(pool, guild_id).await
}

#[tracing::instrument(
	name = "db.server_icon_rotation.load_state",
	skip_all,
	fields(guild.id = guild_id.get()),
	err(Debug),
)]
async fn load_state(
	pool: &SqlitePool,
	guild_id: serenity::GuildId,
) -> Result<RotationState, Error> {
	sqlx::query_as::<_, RotationState>(
		"SELECT next_rotation_at, paused_at, pending_icon_path, current_icon_path
		 FROM server_icon_rotation WHERE guild_id = ?",
	)
	.bind(guild_id.get().to_string())
	.fetch_one(pool)
	.await
	.context("Failed to read server icon rotation state")
}

#[tracing::instrument(name = "filesystem.list_server_icons", err(Debug))]
async fn icon_choices(icon_directory: &Path) -> Result<Vec<IconChoice>, Error> {
	let mut entries = tokio::fs::read_dir(icon_directory)
		.await
		.with_context(|| format!("Failed to read {}", icon_directory.display()))?;
	let mut raw = Vec::new();
	while let Some(entry) = entries.next_entry().await? {
		let path = entry.path();
		if !path.is_file() {
			continue;
		}
		let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
			continue;
		};
		let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
			continue;
		};
		let Some(suffix) = stem.strip_prefix("server_icon_") else {
			continue;
		};
		let extension = path
			.extension()
			.and_then(|value| value.to_str())
			.unwrap_or("");
		if !matches!(
			extension.to_ascii_lowercase().as_str(),
			"png" | "gif" | "jpg" | "jpeg" | "webp"
		) {
			continue;
		}
		raw.push((
			suffix.to_owned(),
			extension.to_ascii_lowercase(),
			filename.to_owned(),
		));
	}

	let counts = raw.iter().fold(
		HashMap::<String, usize>::new(),
		|mut counts, (suffix, _, _)| {
			*counts.entry(suffix.clone()).or_default() += 1;
			counts
		},
	);
	let mut choices = raw
		.into_iter()
		.map(|(base_suffix, extension, filename)| {
			let suffix = if counts.get(&base_suffix).copied().unwrap_or_default() > 1 {
				if extension == "png" {
					base_suffix.clone()
				} else {
					format!("{base_suffix}.{extension}")
				}
			} else {
				base_suffix
			};
			IconChoice {
				suffix,
				path: icon_directory
					.join(&filename)
					.to_string_lossy()
					.into_owned(),
				filename,
			}
		})
		.collect::<Vec<_>>();
	choices.sort_by(|a, b| a.suffix.cmp(&b.suffix));
	Ok(choices)
}

async fn choose_random_icon(
	icon_directory: &Path,
	current_path: Option<&str>,
) -> Result<Option<String>, Error> {
	let mut choices = icon_choices(icon_directory).await?;
	if choices.len() > 1 {
		choices.retain(|choice| Some(choice.path.as_str()) != current_path);
	}
	Ok(choices
		.into_iter()
		.choose(&mut rand::rng())
		.map(|choice| choice.path))
}

fn now_seconds() -> i64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_secs() as i64
}

fn random_rotation_delay() -> i64 {
	rand::rng().random_range(ROTATION_MIN_SECONDS..ROTATION_MAX_SECONDS) as i64
}

fn resume_deadline(next_rotation_at: i64, paused_at: i64, now: i64) -> i64 {
	next_rotation_at.saturating_add(now.saturating_sub(paused_at).max(0))
}

#[tracing::instrument(
	name = "server_icon_rotation.retry",
	skip_all,
	fields(guild.id = guild_id.get(), failure = tracing::field::Empty),
)]
async fn retry_delay(guild_id: serenity::GuildId, failures: &mut u32) {
	*failures = failures.saturating_add(1);
	tracing::Span::current().record("failure", *failures);
	let exponent = failures.saturating_sub(1).min(6);
	let seconds = (60u64 * 2u64.pow(exponent)).min(60 * 60);
	warn!(
		guild.id = guild_id.get(),
		failure = *failures,
		seconds,
		"Retrying server icon rotation after failure"
	);
	tokio::time::sleep(Duration::from_secs(seconds)).await;
}

fn image_url(path: &str) -> Result<String, Error> {
	let filename = PathBuf::from(path)
		.file_name()
		.and_then(|name| name.to_str())
		.ok_or_else(|| anyhow!("Invalid server icon path `{path}`"))?
		.to_owned();
	Ok(format!("{GITHUB_ICON_URL}{filename}?raw=true"))
}

#[poise::command(
	slash_command,
	prefix_command,
	category = "Utilities",
	subcommands(
		"server_icon_status",
		"server_icon_list",
		"server_icon_pause",
		"server_icon_resume",
		"server_icon_set"
	)
)]
#[tracing::instrument(
	name = "discord.command.server_icon",
	skip_all,
	fields(command = %ctx.command().qualified_name),
	err(Debug),
)]
pub async fn server_icon(ctx: Context<'_>) -> Result<(), Error> {
	Ok(())
}

#[poise::command(
	slash_command,
	prefix_command,
	rename = "status",
	category = "Utilities"
)]
#[tracing::instrument(
	name = "discord.command.server_icon.status",
	skip_all,
	fields(
		command = %ctx.command().qualified_name,
		author.id = ctx.author().id.get(),
		channel.id = ctx.channel_id().get(),
		guild.id = ctx.guild_id().map(serenity::GuildId::get),
	),
	err(Debug),
)]
pub async fn server_icon_status(ctx: Context<'_>) -> Result<(), Error> {
	let service = icon_service(ctx)?;
	let state = service.status().await?;
	let description = if state.paused {
		format!(
			"Rotation is paused. Its remaining time is preserved (next change <t:{}:R>).",
			state.next_rotation_at
		)
	} else {
		format!(
			"Rotation is active. Next change <t:{}:R>.",
			state.next_rotation_at
		)
	};
	let mut embed = serenity::CreateEmbed::new()
		.title("Server icon rotation")
		.description(description.clone());
	let mut reply = poise::CreateReply::default();
	if let Some(path) = state.current_icon_path {
		let url = image_url(&path)?;
		embed = embed.image(url.clone());
		reply = reply.content(format!("<{url}>"));
	}
	ctx.send(reply.embed(embed)).await?;
	Ok(())
}

#[poise::command(slash_command, prefix_command, rename = "list", category = "Utilities")]
#[tracing::instrument(
	name = "discord.command.server_icon.list",
	skip_all,
	fields(
		command = %ctx.command().qualified_name,
		author.id = ctx.author().id.get(),
		channel.id = ctx.channel_id().get(),
		guild.id = ctx.guild_id().map(serenity::GuildId::get),
	),
	err(Debug),
)]
pub async fn server_icon_list(ctx: Context<'_>) -> Result<(), Error> {
	let choices = icon_choices(&ctx.data().server_icon_directory).await?;
	if choices.is_empty() {
		ctx.say("No server icons are available.").await?;
		return Ok(());
	}

	let mut page = String::from("Available icons:\n");
	for choice in choices {
		let image_link = format!("<{GITHUB_ICON_URL}{}?raw=true>", choice.filename);
		let entry = format!("`{}` {image_link}\n", choice.suffix);
		if page.len() + entry.len() > 1800 {
			ctx.say(&page).await?;
			page.clear();
		}
		page.push_str(&entry);
	}
	if !page.is_empty() {
		ctx.say(page).await?;
	}
	Ok(())
}

#[poise::command(
	slash_command,
	prefix_command,
	rename = "pause",
	category = "Utilities",
	check = "crate::checks::check_is_moderator"
)]
#[tracing::instrument(
	name = "discord.command.server_icon.pause",
	skip_all,
	fields(
		command = %ctx.command().qualified_name,
		author.id = ctx.author().id.get(),
		channel.id = ctx.channel_id().get(),
		guild.id = ctx.guild_id().map(serenity::GuildId::get),
	),
	err(Debug),
)]
pub async fn server_icon_pause(ctx: Context<'_>) -> Result<(), Error> {
	let paused = icon_service(ctx)?.pause().await?;
	let message = if paused {
		"Server icon rotation paused. The remaining time is preserved."
	} else {
		"Server icon rotation is already paused."
	};
	ctx.send(
		poise::CreateReply::default()
			.content(message)
			.ephemeral(true),
	)
	.await?;
	Ok(())
}

#[poise::command(
	slash_command,
	prefix_command,
	rename = "resume",
	category = "Utilities",
	check = "crate::checks::check_is_moderator"
)]
#[tracing::instrument(
	name = "discord.command.server_icon.resume",
	skip_all,
	fields(
		command = %ctx.command().qualified_name,
		author.id = ctx.author().id.get(),
		channel.id = ctx.channel_id().get(),
		guild.id = ctx.guild_id().map(serenity::GuildId::get),
	),
	err(Debug),
)]
pub async fn server_icon_resume(ctx: Context<'_>) -> Result<(), Error> {
	let resumed = icon_service(ctx)?.resume().await?;
	let message = if resumed {
		"Server icon rotation resumed. The remaining time is preserved."
	} else {
		"Server icon rotation is already active."
	};
	ctx.send(
		poise::CreateReply::default()
			.content(message)
			.ephemeral(true),
	)
	.await?;
	Ok(())
}

#[poise::command(
	slash_command,
	prefix_command,
	rename = "set",
	category = "Utilities",
	check = "crate::checks::check_is_moderator"
)]
#[tracing::instrument(
	name = "discord.command.server_icon.set",
	skip_all,
	fields(
		command = %ctx.command().qualified_name,
		author.id = ctx.author().id.get(),
		channel.id = ctx.channel_id().get(),
		guild.id = ctx.guild_id().map(serenity::GuildId::get),
		icon.suffix = %suffix,
	),
	err(Debug),
)]
pub async fn server_icon_set(ctx: Context<'_>, suffix: String) -> Result<(), Error> {
	ctx.defer_ephemeral().await?;
	let choice = icon_service(ctx)?
		.set_icon(ctx.serenity_context(), &suffix)
		.await?;
	let message = format!("Changed the server icon to `{}`.", choice.suffix);
	ctx.send(
		poise::CreateReply::default()
			.content(message)
			.ephemeral(true),
	)
	.await?;
	Ok(())
}

fn icon_service(ctx: Context<'_>) -> Result<&Arc<ServerIconRotation>, Error> {
	ctx.data()
		.server_icon_rotation
		.as_ref()
		.ok_or_else(|| anyhow!("Server icon controls require the SQLite database to be enabled"))
}

#[cfg(test)]
mod tests {
	use std::{collections::HashSet, path::Path};

	use poise::serenity_prelude::GuildId;
	use sqlx::sqlite::SqlitePoolOptions;

	use super::icon_choices;
	use super::{ensure_state, load_state, resume_deadline};

	#[tokio::test]
	async fn icon_catalog_has_unique_suffixes_and_paths() {
		let choices = icon_choices(Path::new("assets/server-icons"))
			.await
			.unwrap();
		let suffixes = choices
			.iter()
			.map(|choice| &choice.suffix)
			.collect::<HashSet<_>>();
		let paths = choices
			.iter()
			.map(|choice| &choice.path)
			.collect::<HashSet<_>>();
		assert_eq!(suffixes.len(), choices.len());
		assert_eq!(paths.len(), choices.len());
		assert!(choices.iter().any(|choice| choice.suffix == "owo"));
		assert!(choices.iter().any(|choice| choice.suffix == "gopher_eyes"));
		assert!(
			choices
				.iter()
				.any(|choice| choice.suffix == "gopher_eyes.gif")
		);
	}

	#[test]
	fn resume_preserves_remaining_rotation_time() {
		assert_eq!(resume_deadline(1_000, 200, 800), 1_600);
		assert_eq!(resume_deadline(1_000, 200, 100), 1_000);
	}

	#[tokio::test]
	async fn icon_rotation_state_round_trips_paths_through_sqlite() {
		let pool = SqlitePoolOptions::new()
			.max_connections(1)
			.connect("sqlite::memory:")
			.await
			.unwrap();
		sqlx::query(
			"CREATE TABLE server_icon_rotation (
			 guild_id TEXT PRIMARY KEY NOT NULL,
			 next_rotation_at INTEGER NOT NULL,
			 paused_at INTEGER,
			 pending_icon_path TEXT,
			 current_icon_path TEXT
		 )",
		)
		.execute(&pool)
		.await
		.unwrap();

		let guild_id = GuildId::new(123);
		let initial = ensure_state(&pool, guild_id).await.unwrap();
		assert!(initial.current_icon_path.is_none());
		sqlx::query(
			"UPDATE server_icon_rotation
			 SET pending_icon_path = ?, current_icon_path = ?, paused_at = ?, next_rotation_at = ?
			 WHERE guild_id = ?",
		)
		.bind("assets/server-icons/server_icon_owo.png")
		.bind("assets/server-icons/server_icon_turtle.png")
		.bind(500_i64)
		.bind(900_i64)
		.bind(guild_id.get().to_string())
		.execute(&pool)
		.await
		.unwrap();

		let restored = load_state(&pool, guild_id).await.unwrap();
		assert_eq!(
			restored.pending_icon_path.as_deref(),
			Some("assets/server-icons/server_icon_owo.png")
		);
		assert_eq!(
			restored.current_icon_path.as_deref(),
			Some("assets/server-icons/server_icon_turtle.png")
		);
		assert_eq!(restored.paused_at, Some(500));
		assert_eq!(restored.next_rotation_at, 900);
	}
}
