//! The Fauxcord control API used to create and remove one test's Discord state.

use std::{collections::HashMap, time::Duration};

use anyhow::{Context, Result, anyhow};
use ferrisbot_for_discord::SecretStore;
use serde::Deserialize;
use serde_json::json;

pub(super) struct TestDiscord {
	client: reqwest::Client,
	pub(super) base_url: String,
	bot_token: String,
	bot_id: String,
	pub(super) guild_id: String,
	pub(super) command_channel: String,
	pub(super) modmail_channel: String,
	modlog_channel: String,
	user_id: String,
}

impl TestDiscord {
	pub(super) async fn register() -> Result<Self> {
		let client = reqwest::Client::builder()
			.timeout(Duration::from_secs(5))
			.build()?;

		let base_url =
			std::env::var("FAUXCORD_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".to_owned());
		let bot_token = format!("rustbot-snapshot-{:032x}", rand::random::<u128>());

		let setup: SetupResponse = client
			.post(format!("{base_url}/_test/setup"))
			.json(&json!({
				"token": format!("Bot {bot_token}"),
				"user": { "username": "Snapshot bot" },
				"guilds": [{
					"name": "Snapshot test",
					"channels": [
						{ "name": "command", "type": 0 },
						{ "name": "modmail", "type": 0 },
						{ "name": "bot-log", "type": 0 }
					]
				}]
			}))
			.send()
			.await
			.context("Fauxcord unavailable; start it as described in tests/README.md")?
			.error_for_status()?
			.json()
			.await?;

		let guild = setup
			.guilds
			.into_iter()
			.next()
			.context("Fauxcord returned no guild")?;
		let mut channels: HashMap<_, _> = guild
			.channels
			.into_iter()
			.map(|channel| (channel.name, channel.id))
			.collect();

		let user: User = client
			.post(format!("{base_url}/_test/users"))
			.json(&json!({ "username": "Snapshot tester" }))
			.send()
			.await?
			.error_for_status()?
			.json()
			.await?;

		Ok(Self {
			client,
			base_url,
			bot_token,
			bot_id: setup.user.id,
			guild_id: guild.id,
			command_channel: channels
				.remove("command")
				.context("missing command channel")?,
			modmail_channel: channels
				.remove("modmail")
				.context("missing modmail channel")?,
			modlog_channel: channels
				.remove("bot-log")
				.context("missing bot-log channel")?,
			user_id: user.id,
		})
	}

	pub(super) fn secrets(&self) -> SecretStore {
		SecretStore(
			[
				("DISCORD_TOKEN", self.bot_token.clone()),
				("DISCORD_GUILD", self.guild_id.clone()),
				("APPLICATION_ID", self.bot_id.clone()),
				("MOD_ROLE_ID", "1".to_owned()),
				("MOD_CONSULTANT_ROLE_ID", "2".to_owned()),
				("RUSTACEAN_ROLE_ID", "3".to_owned()),
				("MODMAIL_CHANNEL_ID", self.modmail_channel.clone()),
				("MODLOG_CHANNEL_ID", self.modlog_channel.clone()),
			]
			.into_iter()
			.map(|(key, value)| (key.to_owned(), value))
			.collect(),
		)
	}

	pub(super) async fn inject_command(&self, command: &str) -> Result<()> {
		// Fauxcord broadcasts across guilds; a mention targets only this test's bot.
		self.client
			.post(format!(
				"{}/_test/channels/{}/messages",
				self.base_url, self.command_channel
			))
			.json(&json!({
				"content": format!("<@{}> {command}", self.bot_id),
				"author": { "id": self.user_id }
			}))
			.send()
			.await?
			.error_for_status()?;

		Ok(())
	}

	pub(super) async fn bot_messages(&self, channel: &str) -> Result<Vec<String>> {
		let response: ChannelMessages = self
			.client
			.get(format!("{}/_test/messages/{channel}", self.base_url))
			.send()
			.await?
			.error_for_status()?
			.json()
			.await?;

		let author = format!("Bot {}", self.bot_token);

		Ok(response
			.messages
			.into_iter()
			.filter(|message| message.author_token.as_deref() == Some(author.as_str()))
			.map(|message| message.content)
			.collect())
	}

	pub(super) async fn cleanup(&self) -> Result<()> {
		let mut url = reqwest::Url::parse(&format!("{}/_test/setup/", self.base_url))?;
		url.path_segments_mut()
			.map_err(|()| anyhow!("Fauxcord URL must support path segments"))?
			.pop_if_empty()
			.push(&format!("Bot {}", self.bot_token));

		self.client.delete(url).send().await?.error_for_status()?;

		Ok(())
	}
}

#[derive(Deserialize)]
struct SetupResponse {
	user: User,
	guilds: Vec<Guild>,
}

#[derive(Deserialize)]
struct Guild {
	id: String,
	channels: Vec<Channel>,
}

#[derive(Deserialize)]
struct Channel {
	id: String,
	name: String,
}

#[derive(Deserialize)]
struct User {
	id: String,
}

#[derive(Deserialize)]
struct ChannelMessages {
	messages: Vec<Message>,
}

#[derive(Deserialize)]
struct Message {
	#[serde(default)]
	content: String,
	author_token: Option<String>,
}
