use anyhow::{Result, ensure};

use crate::support::TestBot;

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn matching_messages_dm_the_highlight_owner() -> Result<()> {
	let mut bot = TestBot::start().await?;

	let result = async {
		let reply = bot.command("highlight add ^zebra$").await?;
		ensure!(reply == "hl added!", "unexpected add reply: {reply:?}");

		bot.inject_message("this message does not match").await?;
		let messages = bot.dm_messages().await?;
		ensure!(messages.is_empty(), "unexpected DMs: {messages:#?}");

		bot.inject_message("zebra").await?;
		let dm = bot
			.wait_for_dm(|message| message.contains("your match `^zebra$` was satisfied"))
			.await?;
		ensure!(
			dm.contains("message https://discord.com/channels/"),
			"DM has no message link: {dm:?}"
		);

		Ok::<_, anyhow::Error>(())
	}
	.await;

	let cleanup = bot.finish().await;
	result?;
	cleanup?;

	Ok(())
}

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn hidden_channel_messages_do_not_trigger_highlights() -> Result<()> {
	let mut bot = TestBot::start().await?;

	let result = async {
		let reply = bot.command("highlight add ^zebra$").await?;
		ensure!(reply == "hl added!", "unexpected add reply: {reply:?}");

		bot.restrict_channel_visibility().await?;
		bot.inject_restricted_message("zebra").await?;

		let messages = bot.dm_messages().await?;
		ensure!(messages.is_empty(), "unexpected DMs: {messages:#?}");

		Ok::<_, anyhow::Error>(())
	}
	.await;

	let cleanup = bot.finish().await;
	result?;
	cleanup?;

	Ok(())
}

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn removing_a_highlight_stops_future_notifications() -> Result<()> {
	let mut bot = TestBot::start().await?;

	let result = async {
		let reply = bot.command("highlight add ^zebra$").await?;
		ensure!(reply == "hl added!", "unexpected add reply: {reply:?}");
		let reply = bot.command("highlight remove 1").await?;
		ensure!(reply == "hl removed!", "unexpected remove reply: {reply:?}");

		bot.inject_message("zebra").await?;
		let messages = bot.dm_messages().await?;
		ensure!(messages.is_empty(), "unexpected DMs: {messages:#?}");

		Ok::<_, anyhow::Error>(())
	}
	.await;

	let cleanup = bot.finish().await;
	result?;
	cleanup?;

	Ok(())
}

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn invalid_regex_is_rejected_without_adding_a_highlight() -> Result<()> {
	let mut bot = TestBot::start().await?;

	let result = async {
		let reply = bot.command("highlight add (").await?;
		ensure!(
			reply.starts_with("```"),
			"unexpected invalid-regex reply: {reply:?}"
		);
		ensure!(
			reply.contains("unclosed group"),
			"regex error missing: {reply:?}"
		);

		bot.inject_message("zebra").await?;
		let messages = bot.dm_messages().await?;
		ensure!(messages.is_empty(), "unexpected DMs: {messages:#?}");

		Ok::<_, anyhow::Error>(())
	}
	.await;

	let cleanup = bot.finish().await;
	result?;
	cleanup?;

	Ok(())
}
