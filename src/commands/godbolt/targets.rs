use std::iter::once;
use tracing::Instrument as _;

use anyhow::{Error, anyhow};
use poise::serenity_prelude as serenity;
use tracing::{error, info};

use crate::types::Context;
use crate::types::Data;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GodboltTarget {
	id: String,
	name: String,
	semver: String,
	instruction_set: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct GodboltLibraryVersion {
	#[allow(unused)]
	id: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(unused)]
struct GodboltLibrary {
	#[allow(unused)]
	id: String,
	#[allow(unused)]
	versions: Vec<GodboltLibraryVersion>,
}

#[derive(Default, Debug)]
pub struct GodboltMetadata {
	targets: Vec<GodboltTarget>,
	#[allow(unused)]
	libraries: Vec<GodboltLibrary>,
	last_update_time: Option<std::time::Instant>,
}

impl GodboltTarget {
	fn clean_request_data(&mut self) {
		// Some semvers get weird characters like `()` in them or spaces, we strip that out here
		self.semver = self
			.semver
			.chars()
			.filter(|char| char.is_alphanumeric() || matches!(char, '.' | '-' | '_'))
			.map(|char| char.to_ascii_lowercase())
			.collect();
	}
}

async fn update_godbolt_metadata(data: &Data) -> Result<(), Error> {
	let last_update_time = data.godbolt_metadata.lock().unwrap().last_update_time;
	let needs_update = if let Some(last_update_time) = last_update_time {
		// Get the time to wait between each update of the godbolt metadata
		let update_period = std::env::var("GODBOLT_UPDATE_DURATION")
			.ok()
			.and_then(|duration| duration.parse::<u64>().ok())
			.map_or_else(
				// Currently set for 12 hours
				|| std::time::Duration::from_hours(12),
				std::time::Duration::from_secs,
			);

		let time_since_update =
			std::time::Instant::now().saturating_duration_since(last_update_time);
		let needs_update = time_since_update >= update_period;
		if needs_update {
			info!(
				"godbolt metadata was last updated {:#?} ago, updating it",
				time_since_update,
			);
		}

		needs_update
	} else {
		info!("godbolt metadata hasn't yet been updated, fetching it");

		true
	};

	// If we should perform an update then do so
	if needs_update {
		let request = data
			.http
			.get(data.external_apis.godbolt_url("api/compilers/rust"))
			.header(reqwest::header::ACCEPT, "application/json");
		let mut targets: Vec<GodboltTarget> = request
			.with_extension(crate::telemetry::ExternalOperation::new(
				"godbolt.compilers.list",
				"godbolt",
			))
			.send()
			.await?
			.json()
			.await?;
		// Clean up the data we've gotten from the request
		for target in &mut targets {
			target.clean_request_data();
			if let Some(semver) = target.semver.strip_prefix("rustc ") {
				target.semver = semver.to_owned();
			}
		}

		let request = data
			.http
			.get(data.external_apis.godbolt_url("api/libraries/rust"))
			.header(reqwest::header::ACCEPT, "application/json");
		let libraries: Vec<GodboltLibrary> = request
			.with_extension(crate::telemetry::ExternalOperation::new(
				"godbolt.libraries.list",
				"godbolt",
			))
			.send()
			.await?
			.json()
			.await?;

		info!(
			"updating godbolt metadata: {} targets, {} libraries",
			targets.len(),
			libraries.len()
		);
		*data.godbolt_metadata.lock().unwrap() = GodboltMetadata {
			targets,
			libraries,
			last_update_time: Some(std::time::Instant::now()),
		};
	}

	Ok(())
}

pub async fn fetch_godbolt_metadata(
	data: &Data,
) -> impl std::ops::Deref<Target = GodboltMetadata> + '_ {
	// If we encounter an error while updating the targets list, just log it
	if let Err(error) = update_godbolt_metadata(data).await {
		error!("failed to update godbolt metadata: {:?}", error);
	}

	data.godbolt_metadata.lock().unwrap()
}

// Generates godbolt-compatible rustc identifier and flags from command input
//
// Transforms human readable rustc version (e.g. "1.34.1") into compiler id on godbolt (e.g. "r1341")
// Full list of version<->id can be obtained at https://godbolt.org/api/compilers/rust
pub(crate) async fn rustc_id_and_flags(
	data: &Data,
	params: &poise::KeyValueArgs,
) -> Result<(String, String), Error> {
	let rustc = params.get("rustc").unwrap_or("nightly");
	let target = fetch_godbolt_metadata(data)
		.await
		.targets
		.iter()
		.find(|target| target.semver == rustc.trim())
		.cloned()
		.ok_or(anyhow!(
			"the `rustc` argument should be a version specifier like `nightly` `beta` or `1.45.2`. \
            Run ?targets for a full list"
		))?;

	let opt_level = params.get("-Copt-level").unwrap_or("3");
	let edition = params.get("--edition").unwrap_or("2024");
	let flags = itertools::Itertools::intersperse(params
		.0
		.iter()
		.filter(|(k, _)| !matches!(k.as_str(), "rustc" | "-Copt-level" | "--edition"))
		.map(|(a, b)| format!("{a}={b}"))
		.chain(once(format!("-Copt-level={opt_level}")))
		.chain(once(format!("--edition={edition}"))), " ".to_string())
		// itertools was already imported by prost
		.collect::<String>();
	println!("{flags}");

	Ok((target.id, flags))
}

/// Used to rank godbolt compiler versions for listing them out
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SemverRanking<'a> {
	Beta,
	Nightly,
	Compiler(&'a str),
	Semver(std::cmp::Reverse<(u16, u16, u16)>),
}

impl<'a> From<&'a str> for SemverRanking<'a> {
	fn from(semver: &'a str) -> Self {
		match semver {
			"beta" => Self::Beta,
			"nightly" => Self::Nightly,

			semver => {
				// Rustc versions are received in a `X.X.X` form, so we parse out
				// the major/minor/patch versions and then order them in *reverse*
				// order based on their version triple, this means that the most
				// recent (read: higher) versions will be at the top of the list
				let mut version_triple = semver.splitn(3, '.');
				let version_triple = version_triple
					.next()
					.zip(version_triple.next())
					.zip(version_triple.next())
					.and_then(|((major, minor), patch)| {
						Some((
							major.parse().ok()?,
							minor.parse().ok()?,
							patch.parse().ok()?,
						))
					});

				// If we successfully parsed out a semver tuple, return it
				if let Some((major, minor, patch)) = version_triple {
					Self::Semver(std::cmp::Reverse((major, minor, patch)))

				// Anything that doesn't fit the `X.X.X` format we treat as an alternative
				// compiler, we list these after beta & nightly but before the many canonical
				// rustc versions
				} else {
					Self::Compiler(semver)
				}
			}
		}
	}
}

/// Lists all available godbolt rustc targets
#[poise::command(prefix_command, slash_command, broadcast_typing, category = "Godbolt")]
#[tracing::instrument(
	name = "command.targets",
	skip_all,
	fields(
		command = %ctx.command().qualified_name,
		author.id = ctx.author().id.get(),
		channel.id = ctx.channel_id().get(),
		guild.id = ctx.guild_id().map(poise::serenity_prelude::GuildId::get),
	),
	err(Debug),
)]
pub async fn targets(ctx: Context<'_>) -> Result<(), Error> {
	let mut targets = fetch_godbolt_metadata(ctx.data()).await.targets.clone();

	// Can't use sort_by_key because https://github.com/rust-lang/rust/issues/34162
	targets.sort_unstable_by(|lhs, rhs| {
		SemverRanking::from(&*lhs.semver).cmp(&SemverRanking::from(&*rhs.semver))
	});

	ctx.send(targets_reply(&targets))
		.instrument(crate::helpers::discord_operation_span("reply.send"))
		.await?;

	Ok(())
}

fn targets_reply(targets: &[GodboltTarget]) -> poise::CreateReply {
	let mut builder = poise::CreateReply::default();
	if targets.is_empty() {
		return builder.embed(serenity::CreateEmbed::default().title("Godbolt Targets"));
	}

	// Discord allows at most 25 fields per embed.
	for (i, chunk) in targets.chunks(25).enumerate() {
		let mut embed = serenity::CreateEmbed::default();
		if i == 0 {
			embed = embed.title("Godbolt Targets");
		}
		embed = embed.fields(chunk.iter().map(|target| {
			(
				if target.semver.is_empty() {
					target.id.as_str()
				} else {
					target.semver.as_str()
				},
				format!("{} (runs on {})", target.name, target.instruction_set),
				true,
			)
		}));
		builder = builder.embed(embed);
	}

	builder
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn targets_reply_respects_field_limit_and_preserves_targets() {
		for count in [1_usize, 24, 25, 26, 50, 51, 124, 250] {
			let targets: Vec<_> = (0..count)
				.map(|i| GodboltTarget {
					id: format!("r{i}"),
					name: format!("rustc {i}"),
					semver: format!("1.{i}.0"),
					instruction_set: "amd64".into(),
				})
				.collect();
			let reply = targets_reply(&targets);
			assert_eq!(reply.embeds.len(), count.div_ceil(25), "{count} targets");
			let embeds: Vec<serde_json::Value> = reply
				.embeds
				.iter()
				.map(|embed| serde_json::to_value(embed).unwrap())
				.collect();
			assert_eq!(embeds[0]["title"], "Godbolt Targets");
			assert!(
				embeds
					.iter()
					.skip(1)
					.all(|embed| embed.get("title").is_none())
			);
			let mut fields = Vec::new();
			for embed in &embeds {
				let chunk = embed["fields"].as_array().unwrap();
				assert!(!chunk.is_empty() && chunk.len() <= 25);
				fields.extend(chunk);
			}
			assert_eq!(fields.len(), count);
			for (field, target) in fields.iter().zip(&targets) {
				assert_eq!(field["name"], target.semver);
				assert_eq!(
					field["value"],
					format!("{} (runs on {})", target.name, target.instruction_set)
				);
				assert_eq!(field["inline"], true);
			}
		}
	}

	#[test]
	fn recorded_godbolt_targets_fit_discord_message_limits() {
		let targets: Vec<GodboltTarget> = serde_json::from_str(include_str!(
			"../../../tests/fixtures/godbolt-compilers-rust.json"
		))
		.unwrap();
		assert!(targets.len() > 25);
		let reply = targets_reply(&targets);
		assert!(reply.embeds.len() <= 10);
		let mut characters = 0;
		let mut field_count = 0;
		for embed in &reply.embeds {
			let embed = serde_json::to_value(embed).unwrap();
			characters += embed["title"]
				.as_str()
				.map_or(0, |title| title.chars().count());
			let fields = embed["fields"].as_array().unwrap();
			assert!(fields.len() <= 25);
			field_count += fields.len();
			for field in fields {
				let name = field["name"].as_str().unwrap().chars().count();
				let value = field["value"].as_str().unwrap().chars().count();
				assert!((1..=256).contains(&name));
				assert!((1..=1024).contains(&value));
				characters += name + value;
			}
		}
		assert_eq!(field_count, targets.len());
		assert!(characters <= 6000);
	}

	#[test]
	fn targets_reply_uses_compiler_id_for_empty_versions() {
		let target = GodboltTarget {
			id: "mrustc-master".into(),
			name: "mrustc (master)".into(),
			semver: String::new(),
			instruction_set: "amd64".into(),
		};
		let reply = targets_reply(&[target]);
		let embed = serde_json::to_value(&reply.embeds[0]).unwrap();
		assert_eq!(embed["fields"][0]["name"], "mrustc-master");
	}

	#[test]
	fn targets_reply_keeps_title_when_empty() {
		let reply = targets_reply(&[]);
		assert_eq!(reply.embeds.len(), 1);
		let embed = serde_json::to_value(&reply.embeds[0]).unwrap();
		assert_eq!(embed["title"], "Godbolt Targets");
		assert!(
			embed
				.get("fields")
				.is_none_or(|fields| fields.as_array().unwrap().is_empty())
		);
	}
}
