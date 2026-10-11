//! Discord workflows whose framework helpers hide multiple API calls.
//! Keep the waits internal and trace each outgoing operation separately.

use poise::{Modal, serenity_prelude as serenity};
use tracing::Instrument as _;

use crate::{helpers::discord_operation_span, types::Context};

#[tracing::instrument(name = "interaction.modal", skip_all, err(Debug))]
pub async fn execute_modal_on_component_interaction<M: Modal>(
	ctx: impl AsRef<serenity::Context>,
	interaction: serenity::ComponentInteraction,
	defaults: Option<M>,
	timeout: Option<std::time::Duration>,
) -> Result<Option<M>, anyhow::Error> {
	let ctx = ctx.as_ref();
	let custom_id = interaction.id.to_string();
	interaction
		.create_response(ctx, M::create(defaults, custom_id.clone()))
		.instrument(discord_operation_span("interaction.open_modal"))
		.await?;
	let response = serenity::collector::ModalInteractionCollector::new(&ctx.shard)
		.filter(move |d| d.data.custom_id == custom_id)
		.timeout(timeout.unwrap_or(std::time::Duration::from_secs(3600)))
		.await;
	let Some(response) = response else {
		return Ok(None);
	};
	response
		.create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
		.instrument(discord_operation_span("interaction.acknowledge"))
		.await?;
	Ok(Some(
		M::parse(response.data.clone()).map_err(serenity::Error::Other)?,
	))
}

/// Same registration choices and owner check as Poise's button helper, with
/// per-call spans instead of treating the entire interactive workflow as a client call.
pub async fn register_application_commands_buttons(ctx: Context<'_>) -> Result<(), anyhow::Error> {
	if !ctx.framework().options().owners.contains(&ctx.author().id) {
		ctx.say("Can only be used by bot owner")
			.instrument(discord_operation_span("reply.send"))
			.await?;
		return Ok(());
	}
	let commands =
		poise::builtins::create_application_commands(&ctx.framework().options().commands);
	let count = commands.len();
	let buttons = [
		(
			"register.guild",
			"Register in guild",
			"📋",
			serenity::ButtonStyle::Primary,
		),
		(
			"unregister.guild",
			"Delete in guild",
			"🗑",
			serenity::ButtonStyle::Danger,
		),
		(
			"register.global",
			"Register globally",
			"📋",
			serenity::ButtonStyle::Primary,
		),
		(
			"unregister.global",
			"Unregister globally",
			"🗑",
			serenity::ButtonStyle::Danger,
		),
	]
	.into_iter()
	.map(|(id, label, emoji, style)| {
		serenity::CreateButton::new(id)
			.label(label)
			.emoji(serenity::ReactionType::Unicode(emoji.to_owned()))
			.style(style)
	})
	.collect();
	let reply = ctx
		.send(
			poise::CreateReply::default()
				.content("Choose what to do with the commands:")
				.components(vec![serenity::CreateActionRow::Buttons(buttons)]),
		)
		.instrument(discord_operation_span("reply.send"))
		.await?;
	let message = reply
		.message()
		.instrument(discord_operation_span("reply.fetch"))
		.await?;
	let interaction = message
		.await_component_interaction(ctx)
		.author_id(ctx.author().id)
		.await;
	reply
		.edit(
			ctx,
			poise::CreateReply::default()
				.components(vec![])
				.content("Processing... Please wait."),
		)
		.instrument(discord_operation_span("reply.edit"))
		.await?;
	let Some(interaction) = interaction else {
		ctx.say(":warning: You didn't interact in time - please run the command again.")
			.instrument(discord_operation_span("reply.send"))
			.await?;
		return Ok(());
	};
	let (register, global) = match interaction.data.custom_id.as_str() {
		"register.guild" => (true, false),
		"unregister.guild" => (false, false),
		"register.global" => (true, true),
		"unregister.global" => (false, true),
		other => {
			tracing::warn!(button = other, "Unknown registration button");
			return Ok(());
		}
	};
	let started = std::time::Instant::now();
	let scope = if global { "global" } else { "guild" };
	let guild = if global {
		None
	} else {
		let Some(guild) = ctx.guild_id() else {
			ctx.say(":x: Must be called in guild")
				.instrument(discord_operation_span("reply.send"))
				.await?;
			return Ok(());
		};
		Some(guild)
	};
	let status = if register {
		format!(":gear: Registering {count} {scope} commands...")
	} else {
		format!(":gear: Unregistering {scope} commands...")
	};
	ctx.say(status)
		.instrument(discord_operation_span("reply.send"))
		.await?;
	let commands = if register { commands } else { vec![] };
	if let Some(guild) = guild {
		guild
			.set_commands(ctx, commands)
			.instrument(discord_operation_span("guild.set_commands"))
			.await?;
	} else {
		serenity::Command::set_global_commands(ctx, commands)
			.instrument(discord_operation_span("application.set_global_commands"))
			.await?;
	}
	ctx.say(format!(
		":white_check_mark: Done! Took {}ms",
		started.elapsed().as_millis()
	))
	.instrument(discord_operation_span("reply.send"))
	.await?;
	Ok(())
}
