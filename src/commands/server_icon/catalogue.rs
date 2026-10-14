use std::{future::Future, io::Cursor, sync::Arc, time::Duration};

use anyhow::{Context as _, Error};
use futures::{Stream, StreamExt as _};
use image::{ImageFormat, Rgba, RgbaImage};
use imageproc::drawing::{draw_text_mut, text_size};
use opentelemetry::trace::{SpanContext, TraceContextExt as _};
use poise::serenity_prelude as serenity;
use tracing::{Instrument as _, instrument::WithSubscriber as _};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use super::IconChoice;
use crate::{helpers::discord_operation_span, types::Context};

const PAGE_SIZE: usize = 12;
const COLUMNS: u32 = 3;
const CELL_WIDTH: u32 = 224;
const PREVIEW_SIZE: u32 = 176;
const PADDING: u32 = 16;
const FONT_SIZE: f32 = 18.0;
const BACKGROUND: Rgba<u8> = Rgba([43, 45, 49, 255]);
const FOREGROUND: Rgba<u8> = Rgba([242, 243, 245, 255]);
const LIFETIME: Duration = Duration::from_mins(10);

struct CataloguePage {
	png: Vec<u8>,
	names: Vec<String>,
}

impl CataloguePage {
	fn attachment(&self, page: usize) -> serenity::CreateAttachment {
		serenity::CreateAttachment::bytes(self.png.clone(), format!("server-icons-{page}.png"))
			.description(
				"Contact sheet of server icons. Names are listed in the embed description.",
			)
	}

	fn embed(&self, page: usize, pages: usize, total: usize) -> serenity::CreateEmbed {
		serenity::CreateEmbed::new()
			.title("Available server icons")
			.description(format!(
				"{}\n\nModerators can use `/server_icon set` with a name above.\nAnimated icons are shown as still previews.",
				self.names.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(" · ")
			))
			.colour(0x00f7_8c32)
			.image(format!("attachment://server-icons-{page}.png"))
			.footer(serenity::CreateEmbedFooter::new(format!(
				"Page {} of {pages} · {total} icons", page + 1
			)))
	}
}

fn buttons(
	previous: &str,
	next: &str,
	page: usize,
	pages: usize,
	expired: bool,
) -> Vec<serenity::CreateActionRow> {
	vec![serenity::CreateActionRow::Buttons(vec![
		serenity::CreateButton::new(previous)
			.label("Previous")
			.emoji('◀')
			.disabled(expired || page == 0),
		serenity::CreateButton::new(next)
			.label("Next")
			.emoji('▶')
			.disabled(expired || page + 1 >= pages),
	])]
}

fn next_page(page: usize, pages: usize, forward: bool) -> usize {
	if forward {
		(page + 1).min(pages - 1)
	} else {
		page.saturating_sub(1)
	}
}

pub(super) async fn show(ctx: Context<'_>, choices: Vec<IconChoice>) -> Result<(), Error> {
	ctx.defer()
		.instrument(discord_operation_span("reply.defer"))
		.await?;
	let total = choices.len();
	// Image decoding and drawing must not block the Gateway's async runtime.
	let pages = tokio::task::spawn_blocking(move || render_pages(&choices)).await??;
	let previous = format!("{}:server-icons:previous", ctx.id());
	let next = format!("{}:server-icons:next", ctx.id());
	// Register before sending so a fast click cannot be lost between API calls.
	let presses = serenity::collector::ComponentInteractionCollector::new(ctx)
		.channel_id(ctx.channel_id())
		.custom_ids(vec![previous.clone(), next.clone()])
		.timeout(LIFETIME)
		.stream();
	let reply = ctx
		.send(initial_reply(&pages, total, &previous, &next))
		.instrument(discord_operation_span("reply.send"))
		.await?;
	if pages.len() == 1 {
		return Ok(());
	}
	let message = reply
		.message()
		.instrument(discord_operation_span("reply.fetch"))
		.await?;
	// A SpanContext is just IDs/flags: unlike a tracing::Span, it does not keep
	// the command or its Gateway ancestors open while the user is browsing.
	let origin = tracing::Span::current()
		.context()
		.span()
		.span_context()
		.clone();
	let session = CatalogueSession {
		http: ctx.serenity_context().http.clone(),
		channel: ctx.channel_id(),
		message: message.id,
		author: ctx.author().id,
		pages,
		total,
		previous,
		next,
		origin,
		page: 0,
	};
	spawn_listener(session.run(presses));
	Ok(())
}

struct CatalogueSession {
	http: Arc<serenity::Http>,
	channel: serenity::ChannelId,
	message: serenity::MessageId,
	author: serenity::UserId,
	pages: Vec<CataloguePage>,
	total: usize,
	previous: String,
	next: String,
	origin: SpanContext,
	page: usize,
}

impl CatalogueSession {
	fn controls(&self, expired: bool) -> Vec<serenity::CreateActionRow> {
		buttons(
			&self.previous,
			&self.next,
			self.page,
			self.pages.len(),
			expired,
		)
	}

	async fn run(
		mut self,
		mut presses: impl Stream<Item = serenity::ComponentInteraction> + Unpin,
	) -> Result<(), Error> {
		// Waiting is intentionally uninstrumented. Each interaction gets a short
		// root trace; Discord API spans stay children of that individual action.
		while let Some(press) = presses.next().await {
			if press.message.id != self.message {
				continue;
			}
			let origin = self.origin.clone();
			linked_action(&origin, "click", self.handle_press(&press)).await?;
		}
		linked_action(&self.origin, "expire", async {
			// Edit only the controls: preserve the last image and embed after expiry.
			self.channel
				.edit_message(
					&self.http,
					self.message,
					serenity::EditMessage::new().components(self.controls(true)),
				)
				.instrument(discord_operation_span("message.edit"))
				.await?;
			Ok(())
		})
		.await
	}

	async fn handle_press(&mut self, press: &serenity::ComponentInteraction) -> Result<(), Error> {
		let span = tracing::Span::current();
		let allowed = press.user.id == self.author;
		span.record("author.id", press.user.id.get());
		span.record("channel.id", self.channel.get());
		span.record("message.id", self.message.get());
		span.record("catalogue.allowed", allowed);
		if !allowed {
			press
				.create_response(
					&self.http,
					serenity::CreateInteractionResponse::Message(
						serenity::CreateInteractionResponseMessage::new()
							.content("Run `/server_icon list` to browse your own catalogue.")
							.ephemeral(true),
					),
				)
				.instrument(discord_operation_span("interaction.create_response"))
				.await?;
			return Ok(());
		}
		let forward = press.data.custom_id == self.next;
		span.record(
			"catalogue.direction",
			if forward { "next" } else { "previous" },
		);
		self.page = next_page(self.page, self.pages.len(), forward);
		span.record("catalogue.page", self.page + 1);
		press
			.create_response(&self.http, serenity::CreateInteractionResponse::Acknowledge)
			.instrument(discord_operation_span("interaction.acknowledge"))
			.await?;
		self.channel
			.edit_message(
				&self.http,
				self.message,
				page_update(
					&self.pages[self.page],
					self.page,
					self.pages.len(),
					self.total,
					self.controls(false),
				),
			)
			.instrument(discord_operation_span("message.edit"))
			.await?;
		Ok(())
	}
}

fn spawn_listener(
	operation: impl Future<Output = Result<(), Error>> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
	// Keep the subscriber, not the current span. Instrumenting this task with
	// the command span would keep its entire trace open for the session lifetime.
	tokio::spawn(
		async move {
			if operation.await.is_err() {
				tracing::error!("Server icon catalogue listener stopped after an action failed");
			}
		}
		.with_current_subscriber(),
	)
}

async fn linked_action<T>(
	origin: &SpanContext,
	action: &'static str,
	operation: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
	let span = tracing::info_span!(parent: None, "server_icon.catalogue.action",
		otel.name = format!("server_icon.catalogue.{action}"),
		catalogue.action = action,
		author.id = tracing::field::Empty,
		channel.id = tracing::field::Empty,
		message.id = tracing::field::Empty,
		catalogue.allowed = tracing::field::Empty,
		catalogue.direction = tracing::field::Empty,
		catalogue.page = tracing::field::Empty,
		otel.status_code = tracing::field::Empty,
		error.type = tracing::field::Empty,
	);
	if origin.is_valid() {
		span.add_link(origin.clone());
	}
	async {
		let result = operation.await;
		if result.is_err() {
			span.record("otel.status_code", "ERROR");
			span.record("error.type", "catalogue.action.failed");
		}
		result
	}
	.instrument(span.clone())
	.await
}

fn initial_reply(
	pages: &[CataloguePage],
	total: usize,
	previous: &str,
	next: &str,
) -> poise::CreateReply {
	poise::CreateReply::default()
		.embed(pages[0].embed(0, pages.len(), total))
		.attachment(pages[0].attachment(0))
		.components(if pages.len() > 1 {
			buttons(previous, next, 0, pages.len(), false)
		} else {
			vec![]
		})
}

fn page_update(
	page: &CataloguePage,
	index: usize,
	pages: usize,
	total: usize,
	controls: Vec<serenity::CreateActionRow>,
) -> serenity::EditMessage {
	serenity::EditMessage::new()
		.embed(page.embed(index, pages, total))
		// A fresh attachment list replaces the previous sheet rather than accumulating uploads.
		.attachments(serenity::EditAttachments::new().add(page.attachment(index)))
		.components(controls)
}

fn render_pages(choices: &[IconChoice]) -> Result<Vec<CataloguePage>, Error> {
	choices
		.chunks(PAGE_SIZE)
		.map(|chunk| {
			Ok(CataloguePage {
				png: render_sheet(chunk)?,
				names: chunk.iter().map(|choice| choice.suffix.clone()).collect(),
			})
		})
		.collect()
}

fn render_sheet(choices: &[IconChoice]) -> Result<Vec<u8>, Error> {
	let font = ab_glyph::FontRef::try_from_slice(include_bytes!("../../../assets/OpenSans.ttf"))?;
	let labels: Vec<_> = choices
		.iter()
		.map(|choice| label_lines(&choice.suffix, &font))
		.collect();
	let label_rows = u32::try_from(labels.iter().map(Vec::len).max().unwrap_or(1))?;
	let cell_height = PREVIEW_SIZE + 12 + label_rows * 22 + 16;
	let rows = u32::try_from(choices.len())?.div_ceil(COLUMNS);
	let mut sheet = RgbaImage::from_pixel(
		COLUMNS * CELL_WIDTH + PADDING * 2,
		rows * cell_height + PADDING * 2,
		BACKGROUND,
	);
	for (i, choice) in choices.iter().enumerate() {
		let mut reader = image::ImageReader::open(&choice.path)
			.with_context(|| format!("Failed to open icon {}", choice.suffix))?
			.with_guessed_format()?;
		let mut limits = image::Limits::default();
		limits.max_image_width = Some(8192);
		limits.max_image_height = Some(8192);
		limits.max_alloc = Some(64 * 1024 * 1024);
		reader.limits(limits);
		let preview = reader
			.decode()
			.with_context(|| format!("Failed to decode icon {}", choice.suffix))?
			.thumbnail(PREVIEW_SIZE, PREVIEW_SIZE)
			.to_rgba8();
		let index = u32::try_from(i)?;
		let x = PADDING + (index % COLUMNS) * CELL_WIDTH;
		let y = PADDING + (index / COLUMNS) * cell_height;
		image::imageops::overlay(
			&mut sheet,
			&preview,
			i64::from(x + (CELL_WIDTH - preview.width()) / 2),
			i64::from(y + (PREVIEW_SIZE - preview.height()) / 2),
		);
		for (line, text) in labels[i].iter().enumerate() {
			let (width, _) = text_size(FONT_SIZE, &font, text);
			draw_text_mut(
				&mut sheet,
				FOREGROUND,
				i32::try_from(x + (CELL_WIDTH - width) / 2)?,
				i32::try_from(y + PREVIEW_SIZE + 12 + u32::try_from(line)? * 22)?,
				FONT_SIZE,
				&font,
				text,
			);
		}
	}
	let mut png = Cursor::new(Vec::new());
	sheet.write_to(&mut png, ImageFormat::Png)?;
	Ok(png.into_inner())
}

fn label_lines(name: &str, font: &ab_glyph::FontRef<'_>) -> Vec<String> {
	let mut lines = vec![String::new()];
	for ch in name.chars() {
		let line = lines.last_mut().unwrap();
		let candidate = format!("{line}{ch}");
		if text_size(FONT_SIZE, font, &candidate).0 > CELL_WIDTH - 16 {
			lines.push(ch.to_string());
		} else {
			line.push(ch);
		}
	}
	lines
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn navigation_stops_at_both_ends() {
		let mut page = 0;
		for _ in 0..8 {
			page = next_page(page, 5, true);
		}
		assert_eq!(page, 4);
		for _ in 0..8 {
			page = next_page(page, 5, false);
		}
		assert_eq!(page, 0);
		assert_eq!(next_page(0, 1, true), 0);
		assert_eq!(next_page(0, 1, false), 0);
	}

	#[test]
	fn controls_disable_boundaries_and_expired_sessions() {
		for (page, expired, expected) in [
			(0, false, [true, false]),
			(1, false, [false, false]),
			(2, false, [false, true]),
			(1, true, [true, true]),
		] {
			let rows = serde_json::to_value(buttons("prev", "next", page, 3, expired)).unwrap();
			for (button, disabled) in rows[0]["components"]
				.as_array()
				.unwrap()
				.iter()
				.zip(expected)
			{
				assert_eq!(button["disabled"], disabled);
			}
		}
	}

	#[test]
	fn sheet_keeps_aspect_ratio_and_renders_readable_labels() {
		let files = tempfile::tempdir().unwrap();
		let path = files.path().join("test.png");
		RgbaImage::from_pixel(80, 40, Rgba([255, 0, 0, 255]))
			.save(&path)
			.unwrap();
		let choices = vec![
			IconChoice {
				suffix: "red_rectangle".into(),
				path: path.to_string_lossy().into_owned(),
				filename: "test.png".into(),
			};
			4
		];
		let sheet = image::load_from_memory(&render_sheet(&choices).unwrap())
			.unwrap()
			.to_rgba8();
		assert_eq!(sheet.width(), 704);
		// The thumbnail is centered, retains its 2:1 aspect ratio, and is not stretched.
		assert_eq!(*sheet.get_pixel(128, 104), Rgba([255, 0, 0, 255]));
		assert_eq!(*sheet.get_pixel(128, 30), BACKGROUND);
		assert_eq!(*sheet.get_pixel(352, 104), Rgba([255, 0, 0, 255]));
		assert_eq!(*sheet.get_pixel(576, 104), Rgba([255, 0, 0, 255]));
		let label_ink = (200..222).any(|y| (16..240).any(|x| *sheet.get_pixel(x, y) != BACKGROUND));
		assert!(label_ink, "labels should be drawn beneath the preview");
		assert_eq!(*sheet.get_pixel(576, sheet.height() - 80), BACKGROUND);
	}

	#[test]
	fn long_labels_wrap_without_losing_characters() {
		let font =
			ab_glyph::FontRef::try_from_slice(include_bytes!("../../../assets/OpenSans.ttf"))
				.unwrap();
		let name = "a_very_long_server_icon_name_with_underscores.gif";
		let lines = label_lines(name, &font);
		assert!(lines.len() > 1);
		assert_eq!(lines.concat(), name);
		assert!(
			lines
				.iter()
				.all(|line| text_size(FONT_SIZE, &font, line).0 <= CELL_WIDTH - 16)
		);
	}

	#[tokio::test]
	async fn real_catalogue_renders_all_formats_without_missing_or_repeating_icons() {
		let choices = super::super::icon_choices(std::path::Path::new("assets/server-icons"))
			.await
			.unwrap();
		assert!(choices.iter().any(|choice| {
			std::path::Path::new(&choice.filename)
				.extension()
				.is_some_and(|ext| ext.eq_ignore_ascii_case("gif"))
		}));
		let pages = render_pages(&choices).unwrap();
		assert_eq!(pages.len(), choices.len().div_ceil(PAGE_SIZE));
		let names: Vec<_> = pages
			.iter()
			.flat_map(|page| page.names.iter().cloned())
			.collect();
		assert_eq!(
			names,
			choices
				.iter()
				.map(|choice| choice.suffix.clone())
				.collect::<Vec<_>>()
		);
		for (i, page) in pages.iter().enumerate() {
			assert!(!page.png.is_empty() && page.png.len() < 10 * 1024 * 1024);
			let image = image::load_from_memory(&page.png).unwrap();
			assert_eq!(image.width(), 704);
			assert!(image.height() > 200);
			let embed = serde_json::to_value(page.embed(i, pages.len(), choices.len())).unwrap();
			assert_eq!(
				embed["image"]["url"],
				format!("attachment://server-icons-{i}.png")
			);
			assert!(embed["description"].as_str().unwrap().chars().count() <= 4096);
			assert!(
				page.names
					.iter()
					.all(|name| embed["description"].as_str().unwrap().contains(name))
			);
			assert_eq!(page.attachment(i).filename, format!("server-icons-{i}.png"));
		}
	}

	#[test]
	fn initial_message_has_one_sheet_and_navigation_only_for_multiple_pages() {
		let page = || CataloguePage {
			png: vec![1, 2, 3],
			names: vec!["ferris".into()],
		};
		let single = initial_reply(&[page()], 1, "prev", "next");
		assert_eq!(single.components.unwrap().len(), 0);
		let multi = initial_reply(&[page(), page()], 13, "prev", "next");
		assert!(multi.content.is_none());
		assert_eq!(multi.embeds.len(), 1);
		assert_eq!(multi.attachments.len(), 1);
		let controls = serde_json::to_value(multi.components.unwrap()).unwrap();
		assert_eq!(controls[0]["components"].as_array().unwrap().len(), 2);
		assert_eq!(controls[0]["components"][0]["custom_id"], "prev");
		assert_eq!(controls[0]["components"][0]["disabled"], true);
		assert_eq!(controls[0]["components"][1]["custom_id"], "next");
		assert_eq!(controls[0]["components"][1]["disabled"], false);
	}

	#[test]
	fn page_updates_replace_the_previous_attachment_and_embed() {
		let page = CataloguePage {
			png: vec![1, 2, 3],
			names: vec!["ferris".into()],
		};
		let edit = page_update(&page, 1, 2, 13, buttons("prev", "next", 1, 2, false));
		let body = serde_json::to_value(edit).unwrap();
		assert_eq!(body["attachments"].as_array().unwrap().len(), 1);
		assert_eq!(body["attachments"][0]["filename"], "server-icons-1.png");
		assert_eq!(body["embeds"].as_array().unwrap().len(), 1);
		assert_eq!(
			body["embeds"][0]["image"]["url"],
			"attachment://server-icons-1.png"
		);
		assert_eq!(
			body["embeds"][0]["footer"]["text"],
			"Page 2 of 2 · 13 icons"
		);
		assert_eq!(body["components"][0]["components"][1]["disabled"], true);
	}

	#[test]
	fn empty_catalogue_has_no_pages() {
		assert!(render_pages(&[]).unwrap().is_empty());
	}

	#[test]
	fn unreadable_icons_return_context() {
		let choice = IconChoice {
			suffix: "missing".into(),
			path: "/nonexistent/server-icon.png".into(),
			filename: "missing.png".into(),
		};
		assert!(
			render_sheet(&[choice])
				.unwrap_err()
				.to_string()
				.contains("missing")
		);
	}
}

#[cfg(test)]
mod telemetry_tests;
