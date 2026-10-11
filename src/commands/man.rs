use anyhow::Result;
use anyhow::bail;
use reqwest::header;
use tracing::Instrument as _;

use crate::serenity;
use crate::types::Context;

const USER_AGENT: &str = "kangalioo/rustbot";

#[poise::command(
	prefix_command,
	slash_command,
	broadcast_typing,
	category = "Utilities"
)]
#[tracing::instrument(
	name = "command.man",
	skip_all,
	fields(
		command = %ctx.command().qualified_name,
		author.id = ctx.author().id.get(),
		channel.id = ctx.channel_id().get(),
		guild.id = ctx.guild_id().map(poise::serenity_prelude::GuildId::get),
	),
	err(Debug),
)]
pub async fn man(
	ctx: Context<'_>,
	#[description = "Section of the man page"] section: Option<String>,
	#[description = "Name of the man page"] man_page: String,
) -> Result<()> {
	let section = section.unwrap_or_else(|| "1".to_owned());

	// Make sure that the section is a valid number
	if section.parse::<u8>().is_err() {
		bail!("Invalid section number");
	}

	let mut url = format!("https://manpages.debian.org/{section}/{man_page}");

	if let Ok(response) = ctx
		.data()
		.http
		.get(&url)
		.header(header::USER_AGENT, USER_AGENT)
		.with_extension(crate::telemetry::ExternalOperation::new(
			"manpages.fetch",
			"debian-manpages",
		))
		.send()
		.await
	{
		if response.status() == 404 {
			ctx.say("Man page not found.")
				.instrument(crate::helpers::discord_operation_span("reply.send"))
				.await?;
			return Ok(());
		}
	} else {
		ctx.say("Failed to fetch man page.")
			.instrument(crate::helpers::discord_operation_span("reply.send"))
			.await?;
		return Ok(());
	}

	url.push_str(".html");

	ctx.send(
		poise::CreateReply::default().embed(
			serenity::CreateEmbed::new()
				.title(format!("man {man_page}({section})"))
				.description(format!("View the man page for `{man_page}` on the web"))
				.url(&url)
				.color(crate::types::EMBED_COLOR)
				.footer(serenity::CreateEmbedFooter::new(
					"Powered by manpages.debian.org",
				))
				.thumbnail("https://www.debian.org/logos/openlogo-nd-100.jpg")
				.field("Section", &section, true)
				.field("Page", &man_page, true)
				.timestamp(serenity::Timestamp::now()),
		),
	)
	.instrument(crate::helpers::discord_operation_span("reply.send"))
	.await?;

	Ok(())
}
