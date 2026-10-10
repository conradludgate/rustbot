mod server_icon;
mod support;

use anyhow::Result;
use serde_json::json;
use support::{TestBot, fixture};
use wiremock::{
	Mock, ResponseTemplate,
	matchers::{body_partial_json, method, path},
};

// Each test owns its bot, guild/channel, and WireMock server. The cases run in
// parallel; no environment changes, shared journals, or global locks are needed.
// Run these with `cargo test --test fauxcord_snapshots -- --ignored` after
// starting Fauxcord. See tests/README.md for the review workflow.

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn eval_success() -> Result<()> {
	let bot = TestBot::start().await?;

	Mock::given(method("POST"))
		.and(path("/execute"))
		.and(body_partial_json(
			json!({ "channel": "stable", "edition": "2021" }),
		))
		.respond_with(fixture("playground-execute-success.json"))
		.expect(1)
		.mount(&bot.api)
		.await;

	let command = r#"eval channel=stable edition=2021 ```rust
fn main() { println!("Hello, Rust!"); }
```"#;

	let response = bot.send(command).await?;

	insta::assert_snapshot!(response.reply);

	Ok(())
}

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn eval_compile_error() -> Result<()> {
	let bot = TestBot::start().await?;

	Mock::given(method("POST"))
		.and(path("/execute"))
		.respond_with(fixture("playground-execute-compile-error.json"))
		.expect(1)
		.mount(&bot.api)
		.await;

	let command = r#"eval channel=stable edition=2021 ```rust
fn main() { let _: u32 = "hello"; }
```"#;

	let response = bot.send(command).await?;

	insta::assert_snapshot!(response.reply);

	Ok(())
}

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn eval_upstream_error() -> Result<()> {
	let bot = TestBot::start().await?;

	// Failures from the service are synthetic; normal responses use captured data.
	Mock::given(method("POST"))
		.and(path("/execute"))
		.respond_with(
			ResponseTemplate::new(503).set_body_json(json!({"error": "service unavailable"})),
		)
		.expect(1)
		.mount(&bot.api)
		.await;

	let command = r#"eval ```rust
fn main() {}
```"#;

	let response = bot.send(command).await?;

	insta::assert_snapshot!(response.reply);

	Ok(())
}

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn play_success() -> Result<()> {
	let bot = TestBot::start().await?;

	Mock::given(method("POST"))
		.and(path("/execute"))
		.respond_with(fixture("playground-execute-success.json"))
		.expect(1)
		.mount(&bot.api)
		.await;

	let command = r#"play channel=stable edition=2021 ```rust
fn main() { println!("Hello, Rust!"); }
```"#;

	let response = bot.send(command).await?;

	insta::assert_snapshot!(response.reply);

	Ok(())
}

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn microbench_success() -> Result<()> {
	let bot = TestBot::start().await?;

	Mock::given(method("POST"))
		.and(path("/execute"))
		.and(body_partial_json(json!({ "mode": "release" })))
		.respond_with(fixture("playground-execute-microbench-success.json"))
		.expect(1)
		.mount(&bot.api)
		.await;

	let command = r#"microbench channel=stable edition=2021 ```rust
pub fn first() { let _ = 1; }
pub fn second() { let _ = 2; }
```"#;

	let response = bot.send(command).await?;

	insta::assert_snapshot!(response.reply);

	Ok(())
}

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn microbench_requires_public_functions() -> Result<()> {
	let bot = TestBot::start().await?;

	let command = r#"microbench ```rust
fn private_function() {}
```"#;

	let response = bot.send(command).await?;

	// Invalid input should be rejected before calling Playground.
	assert!(
		response.requests.is_empty(),
		"validation should not call an upstream API"
	);

	insta::assert_snapshot!(response.reply);

	Ok(())
}

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn godbolt_success() -> Result<()> {
	let bot = TestBot::start().await?;

	bot.mock_godbolt_metadata().await;

	Mock::given(method("POST"))
		.and(path("/api/compiler/nightly/compile"))
		.respond_with(fixture("godbolt-compile-success.json"))
		.expect(1)
		.mount(&bot.api)
		.await;

	let command = r#"godbolt rustc=nightly ```rust
pub fn add_one(x: u32) -> u32 { x + 1 }
```"#;

	let response = bot.send(command).await?;

	insta::assert_snapshot!(response.reply);

	Ok(())
}

#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn godbolt_compile_error() -> Result<()> {
	let bot = TestBot::start().await?;

	bot.mock_godbolt_metadata().await;

	Mock::given(method("POST"))
		.and(path("/api/compiler/nightly/compile"))
		.respond_with(fixture("godbolt-compile-error.json"))
		.expect(1)
		.mount(&bot.api)
		.await;

	let command = r#"godbolt rustc=nightly ```rust
pub fn add_one( -> u32 {
```"#;

	let response = bot.send(command).await?;

	insta::assert_snapshot!(response.reply);

	Ok(())
}
