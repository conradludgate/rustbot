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
async fn list_shows_one_contact_sheet_without_navigation_for_a_single_page() -> Result<()> {
	let mut bot = TestBot::start().await?;
	let content = bot.command("server_icon list").await?;
	assert!(
		content.is_empty(),
		"the catalogue should not dump URLs into chat"
	);
	let messages = bot.wait_for_command_attachment().await?;
	assert_eq!(messages.len(), 1);
	let message = &messages[0];
	assert_eq!(message["embeds"].as_array().unwrap().len(), 1);
	assert_eq!(message["embeds"][0]["title"], "Available server icons");
	assert_eq!(
		message["embeds"][0]["footer"]["text"],
		"Page 1 of 1 · 2 icons"
	);
	assert!(
		message["embeds"][0]["description"]
			.as_str()
			.unwrap()
			.contains("`ferris` · `owo`")
	);
	assert_eq!(message["attachments"].as_array().unwrap().len(), 1);
	assert_eq!(message["attachments"][0]["filename"], "server-icons-0.png");
	assert!(bot.finish().await?.is_empty());
	Ok(())
}

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn list_keeps_multiple_pages_in_one_message() -> Result<()> {
	let mut bot = TestBot::start().await?;
	for i in 0..11 {
		std::fs::copy(
			bot.server_icon_directory().join("server_icon_ferris.png"),
			bot.server_icon_directory()
				.join(format!("server_icon_test{i:02}.png")),
		)?;
	}
	bot.command("server_icon list").await?;
	let messages = bot.wait_for_command_attachment().await?;
	assert_eq!(messages.len(), 1);
	let message = &messages[0];
	assert_eq!(
		message["embeds"][0]["footer"]["text"],
		"Page 1 of 2 · 13 icons"
	);
	assert_eq!(message["attachments"].as_array().unwrap().len(), 1);
	let description = message["embeds"][0]["description"].as_str().unwrap();
	assert!(description.contains("`test09`"));
	assert!(!description.contains("`test10`"));
	// Fauxcord currently always returns components: []; navigation is checked in unit tests.
	assert!(bot.finish().await?.is_empty());
	Ok(())
}
