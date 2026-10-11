//! Run the production bot, SQLite queue, and Discord Gateway/role API together.
use std::time::Duration;

use crate::support::TestBot;
use anyhow::{Result, ensure};

const DELAY_SECONDS: u32 = 3;

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn first_message_schedules_and_grants_after_the_configured_delay() -> Result<()> {
	let bot = TestBot::start_with_rustification_delay(DELAY_SECONDS).await?;
	let result = async {
		bot.inject_message("hello Rust").await?;
		let queue = bot.rustification_queue().await?;
		ensure!(
			queue.len() == 1,
			"expected one pending assignment: {queue:?}"
		);
		let (Some(joined), due) = queue[0] else {
			anyhow::bail!("missing member join time")
		};
		ensure!(
			due == joined + i64::from(DELAY_SECONDS),
			"configured delay was not applied"
		);
		ensure!(
			!bot.has_rustacean_role().await?,
			"role granted before the delay"
		);
		bot.inject_message("another message").await?;
		ensure!(
			bot.rustification_queue().await? == queue,
			"duplicate message changed the deadline"
		);
		bot.wait_for_rustacean_role().await?;
		bot.inject_message("already a Rustacean").await?;
		ensure!(
			bot.rustification_queue().await?.is_empty(),
			"existing role was queued again"
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
async fn a_join_missed_while_offline_is_repaired_on_the_next_message() -> Result<()> {
	let mut bot = TestBot::start_with_rustification_delay(DELAY_SECONDS).await?;
	let result = async {
		bot.pause_gateway().await;
		bot.post_while_offline().await?;
		tokio::time::sleep(Duration::from_secs(u64::from(DELAY_SECONDS) + 1)).await;
		ensure!(
			bot.rustification_queue().await?.is_empty(),
			"offline join was unexpectedly observed"
		);
		bot.resume_gateway().await?;
		ensure!(
			!bot.has_rustacean_role().await?,
			"role granted without discovery"
		);
		bot.inject_message("my first message after Ferris returned")
			.await?;
		bot.wait_for_rustacean_role().await?;
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
async fn a_pending_assignment_survives_a_gateway_restart_without_another_message() -> Result<()> {
	let mut bot = TestBot::start_with_rustification_delay(DELAY_SECONDS).await?;
	let result = async {
		bot.inject_message("schedule my role").await?;
		ensure!(
			bot.rustification_queue().await?.len() == 1,
			"missing pending assignment"
		);
		bot.pause_gateway().await;
		bot.resume_gateway().await?;
		bot.wait_for_rustacean_role().await?;
		Ok::<_, anyhow::Error>(())
	}
	.await;
	let cleanup = bot.finish().await;
	result?;
	cleanup?;
	Ok(())
}
