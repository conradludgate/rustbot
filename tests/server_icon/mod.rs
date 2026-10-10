use anyhow::Result;

use crate::support::TestBot;

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn pause_requires_moderator() -> Result<()> {
	let bot = TestBot::start().await?;

	let response = bot.send("server_icon pause").await?;

	insta::assert_snapshot!(response.reply, @"This command is only available to moderators.");

	assert!(response.requests.is_empty());

	Ok(())
}

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn status_links_current_icon() -> Result<()> {
	let bot = TestBot::start().await?;

	let response = bot.send("server_icon status").await?;

	insta::assert_snapshot!(response.reply, @"<https://github.com/conradludgate/rustbot/blob/main/assets/server-icons/server_icon_owo.png?raw=true>");

	assert!(response.requests.is_empty());

	Ok(())
}

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn list_links_available_icons() -> Result<()> {
	let bot = TestBot::start().await?;

	let response = bot.send("server_icon list").await?;

	insta::assert_snapshot!(response.reply, @r"
	Available icons:
	`ferris` <https://github.com/conradludgate/rustbot/blob/main/assets/server-icons/server_icon_ferris.png?raw=true>
	`owo` <https://github.com/conradludgate/rustbot/blob/main/assets/server-icons/server_icon_owo.png?raw=true>
	");

	assert!(response.requests.is_empty());

	Ok(())
}
