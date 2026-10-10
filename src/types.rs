use std::{
	collections::HashSet,
	path::PathBuf,
	sync::{Arc, Mutex as StdMutex},
};

use anyhow::{Error, Result};
use poise::serenity_prelude as serenity;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_tracing::TracingMiddleware;
use tokio::sync::RwLock;

use crate::{SecretStore, commands};

#[derive(Debug, Clone)]
pub struct ExternalApiBases {
	playground: String,
	godbolt: String,
}

impl ExternalApiBases {
	#[must_use]
	pub fn new(playground: impl Into<String>, godbolt: impl Into<String>) -> Self {
		Self {
			playground: playground.into(),
			godbolt: godbolt.into(),
		}
	}

	#[must_use]
	pub fn from_env() -> Self {
		Self {
			playground: std::env::var("FERRIS_PLAYGROUND_BASE_URL")
				.unwrap_or_else(|_| "https://play.rust-lang.org".to_owned()),
			godbolt: std::env::var("FERRIS_GODBOLT_BASE_URL")
				.unwrap_or_else(|_| "https://godbolt.org".to_owned()),
		}
	}

	#[must_use]
	pub fn playground_url(&self, path: &str) -> String {
		join_url(&self.playground, path)
	}

	#[must_use]
	pub fn godbolt_url(&self, path: &str) -> String {
		join_url(&self.godbolt, path)
	}
}

fn join_url(base: &str, path: &str) -> String {
	format!("{}/{path}", base.trim_end_matches('/'))
}

#[derive(Debug)]
pub struct Data {
	pub highlights: RwLock<commands::highlight::RegexHolder>,
	pub database: Option<sqlx::SqlitePool>,
	pub discord_guild_id: serenity::GuildId,
	pub application_id: serenity::UserId,
	pub mod_role_id: serenity::RoleId,
	pub mod_consultant_role_id: serenity::RoleId,
	pub rustacean_role_id: serenity::RoleId,
	pub modmail_channel_id: serenity::ChannelId,
	pub modlog_channel_id: serenity::ChannelId,
	pub modmail_message: Arc<tokio::sync::RwLock<Option<serenity::Message>>>,
	pub bot_start_time: std::time::Instant,
	pub http: ClientWithMiddleware,
	pub external_apis: ExternalApiBases,
	pub godbolt_metadata: StdMutex<commands::godbolt::GodboltMetadata>,
	pub move_channel_locks: StdMutex<HashSet<serenity::ChannelId>>,
	pub server_icon_rotation: Option<Arc<commands::server_icon::ServerIconRotation>>,
	pub server_icon_directory: PathBuf,
}

impl Data {
	pub async fn new(
		secret_store: &SecretStore,
		database: Option<sqlx::SqlitePool>,
		external_apis: ExternalApiBases,
		server_icon_directory: PathBuf,
	) -> Result<Self> {
		let discord_guild_id = secret_store.get_discord_id("DISCORD_GUILD")?.into();
		let server_icon_rotation = database.clone().map(|pool| {
			Arc::new(commands::server_icon::ServerIconRotation::new(
				pool,
				discord_guild_id,
				server_icon_directory.clone(),
			))
		});
		Ok(Self {
			highlights: RwLock::new(commands::highlight::RegexHolder::new(database.as_ref()).await),
			database,
			discord_guild_id,
			application_id: secret_store.get_discord_id("APPLICATION_ID")?.into(),
			mod_role_id: secret_store.get_discord_id("MOD_ROLE_ID")?.into(),
			mod_consultant_role_id: secret_store
				.get_discord_id("MOD_CONSULTANT_ROLE_ID")?
				.into(),
			rustacean_role_id: secret_store.get_discord_id("RUSTACEAN_ROLE_ID")?.into(),
			modmail_channel_id: secret_store.get_discord_id("MODMAIL_CHANNEL_ID")?.into(),
			modlog_channel_id: secret_store.get_discord_id("MODLOG_CHANNEL_ID")?.into(),
			modmail_message: Arc::default(),
			bot_start_time: std::time::Instant::now(),
			http: ClientBuilder::new(reqwest::Client::new())
				.with(TracingMiddleware::default())
				.build(),
			external_apis,
			godbolt_metadata: StdMutex::new(commands::godbolt::GodboltMetadata::default()),
			move_channel_locks: StdMutex::new(HashSet::new()),
			server_icon_rotation,
			server_icon_directory,
		})
	}
}

pub type Context<'a> = poise::Context<'a, Data, Error>;

// const EMBED_COLOR: (u8, u8, u8) = (0xf7, 0x4c, 0x00);
pub const EMBED_COLOR: (u8, u8, u8) = (0xb7, 0x47, 0x00); // slightly less saturated
