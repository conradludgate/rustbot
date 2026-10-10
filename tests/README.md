# Command integration tests

The integration tests in `tests/fauxcord_snapshots.rs` use `insta` to compare the
bot's final Discord reply with reviewed snapshots. API command snapshots live
in `tests/snapshots/`; server icon snapshots are inline in their Rust cases.
Each test shows its WireMock expectations, command, and snapshot assertion
together. Bot lifecycle and polling live in `tests/support/mod.rs`; the
Fauxcord control API wrapper lives in `tests/support/fauxcord.rs`.

## Run the tests

Stop the [interactive local stack](../local-test/README.md) first if it is
running; both use port 3000.

Run these commands from the repository root:

```sh
FAUXCORD_BASE_URL=http://127.0.0.1:3000 \
  docker compose -f compose.local.yaml -p rustbot-tests up --detach --wait discord-mock
cargo test --locked --test fauxcord_snapshots -- --ignored --test-threads=8
```

Only Fauxcord runs in Docker. Each test constructs the production bot directly
with a `BotConfig` and runs it as a Tokio task. Every case uses its own migrated
in-memory SQLite database, WireMock server, fake bot, guild, and command channel.
Configuration is passed in memory; tests do not write config files, set bot
environment variables, or change the process working directory.

Cases run in parallel. A completed command shuts down its Gateway shards and
bot task, closes SQLite, and removes its Fauxcord setup. Failed tests abort
their bot task; their Tokio runtime shuts down remaining tasks.

Pass the command to `bot.send` without a prefix. The helper adds this bot's
Discord mention prefix (`<@bot-id>`), which production Poise already supports.
The pinned Fauxcord version broadcasts guild messages to all connected bots;
targeting a specific bot prevents other tests from executing the command.

Polling currently takes the first reply whose content does not start with
`_Running code on playground..._`, with a 20-second timeout. This assumes one
final result. It reads the latest message contents, so snapshots cover the
final reply rather than its edit history.

`FAUXCORD_URL` defaults to `http://127.0.0.1:3000`. A different Fauxcord server
must advertise a `BASE_URL` whose Gateway is reachable by the host tests.
These tests are marked `#[ignore]`: ordinary `cargo test --all-targets` reports
them as ignored, and CI explicitly runs them with `--ignored`. An explicitly
requested run fails if the services are unavailable.

## Review snapshots

When a reply changes or a test has no snapshot yet, `insta` prints a diff and
records a proposed snapshot for review. Install its review tool once, then
review the changes and rerun the tests:

```sh
cargo install cargo-insta --locked
cargo insta review
cargo test --locked --test fauxcord_snapshots -- --ignored
```

Commit accepted `.snap` files or changes to inline snapshots alongside the
tests. Pending `.snap.new` files are ignored by Git. CI uses `INSTA_UPDATE=no`
and fails on missing or changed snapshots.

## Add a test

Copy a small test such as `play_success`, give it a descriptive function name,
and replace the command in its multiline string:

```rust
#[tokio::test]
#[ignore = "requires the local Fauxcord stack"]
async fn another_play_example() -> anyhow::Result<()> {
    let bot = TestBot::start().await?;
    Mock::given(method("POST"))
        .and(path("/execute"))
        .respond_with(fixture("playground-execute-success.json"))
        .expect(1)
        .mount(&bot.api).await;

    let command = r#"play channel=stable edition=2021 ```rust
fn main() { println!("Hello, Rust!"); }
```"#;

    let response = bot.send(command).await?;

    insta::assert_snapshot!(response.reply);

    Ok(())
}
```

The function name determines the snapshot name. Run just that case with
`cargo test --locked --test fauxcord_snapshots another_play_example -- --ignored`,
then use `cargo insta review` to accept its reply.

WireMock selects responses using the matchers in each test. Reuse a captured
response, or add a capture to `tests/fixtures/refresh.py` for a new upstream
scenario. Then review the resulting bot snapshot. Validation errors such as
`microbench_requires_public_functions` need no fixture and assert an empty
request list.

Use WireMock's `body_partial_json` matcher to check request fields, as the
microbench test does for release mode. `.expect(1)` is verified before the
snapshot assertion. `response.requests` exposes recorded requests for further
checks. Godbolt cases call `bot.mock_godbolt_metadata()` to expect both metadata
reads; each bot starts with a cold cache.

## Server icon cases

The server icon cases run with the same Fauxcord service and are included in
the snapshot suite and CI. To run only those cases:

```sh
cargo test --locked --test fauxcord_snapshots server_icon:: -- --ignored
```

The Rust cases in `tests/server_icon/mod.rs` verify moderator denial for `pause`,
the current icon's raw GitHub link in `status`, and the available icon links in
`list`. Each case has its own in-memory SQLite database and two real bundled
images (`ferris` and `owo`). Rotation is seeded paused with `owo` selected, so these
snapshots do not depend on random icon selection or timing. Expected replies
are inline `insta` snapshots alongside each command.

## Refresh API fixtures

```sh
python3 tests/fixtures/refresh.py
```

This script contacts the public Rust Playground and Godbolt APIs and writes
response bodies, HTTP statuses, and capture metadata to `tests/fixtures/`.
It captures execution, real microbench measurements, compiler diagnostics,
assembly, and Godbolt metadata. The HTTP 503 case is defined directly in Rust.
Tests and CI replay the captured data without contacting these upstream APIs.

After refreshing, rerun the tests and review the proposed snapshots before
committing the fixtures and accepted replies.

## Stop the test service

```sh
docker compose -f compose.local.yaml -p rustbot-tests down --volumes
```
