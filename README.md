<p align="center">
        <img src="assets/server-icons/bases/server_icon_base.png" />
</p>
<h1 color="#000000" size="100px" align="center">Ferrisbot for Discord</h1>

A Discord bot for the Rust programming language community Discord server, nicknamed "Ferris".

## Inviting the bot

Some permissions are required:

- Send Messages: base command functionality
- Manage Roles: for automatic Rustacean assignment
- Manage Messages: for `?cleanup` command

Furthermore, the `applications.commands` OAuth2 scope is required for slash commands.

Here's an invite link to an instance hosted by @kangalioo on my Raspberry Pi, with the permissions and scopes incorporated:

[invite the bot](https://discord.com/oauth2/authorize?client_id=804340127433752646&permissions=268445760&scope=bot%20applications.commands)

Adjust the `client_id` in the URL for your own hosted instances of the bot.

## Configuration

The application loads configuration from two TOML files plus environment variables (all env vars are prefixed with `FERRIS_`).

- `config/ferris.toml` (optional): general settings.
- `config/ferris.secrets.toml` (required): copy from `config/ferris.secrets.template.toml` and fill in the required secrets.

`[gateway].privileged_intents` optionally enables `guild_members`, `guild_presences`, or `message_content`. All non-privileged intents are enabled automatically. Only enable privileged intents that are also enabled for the application in the Discord Developer Portal.

## Automatic Rustacean role

With SQLite enabled, Ferris assigns the configured Rustacean role once a member
has been in the configured server for 30 minutes by default. Set the delay in
`config/ferris.toml`:

```toml
[rustification]
delay_seconds = 1800
```

The environment override is `FERRIS_RUSTIFICATION__DELAY_SECONDS`. Zero allows
immediate assignment. Pending entries retain their persisted wake-up times;
eligibility is rechecked using the current delay before granting.

Discord system welcome messages (`USER_JOIN`) schedule assignments; ordinary
messages also repair missed joins.
Members who already have the role are skipped using the message's member data,
without a database query or HTTP request. Messages from members missing the role
upsert one queue entry per user; duplicate observations preserve the deadline and
retry backoff. Message text is not inspected.

The queue survives bot restarts. A single worker checks up to 100 due entries per
pass, waking on new entries, the next deadline, or at least every 15 seconds.
It fetches each member before assigning to check the current join time, existing
role, and membership screening.
Departed members are discarded; failures retry after 60 seconds. Leaving and
rejoining starts a fresh wait using the configured delay.

Enable Discord's system welcome messages in a channel Ferris can view to schedule
silent members. A join missed during downtime is repaired on the member's next
message in a channel Ferris can view; channel history is not scanned. There is no
`/rustify` command. Neither Guild Members nor Message Content privileged intents
are required for role assignment. Ferris needs Manage Roles and its highest role
must be above Rustacean. Disabling SQLite disables this feature.

## Database setup (sqlx)

We use sqlx with SQLite. Point `DATABASE_URL` at the database file you want to use (default: `sqlite://database/ferris.sqlite3`). Run the following after changing schemas or when setting up a fresh checkout:

1. Initialize the database: `cargo sqlx database setup`
2. Apply migrations: `cargo sqlx database migrate`
3. Generate offline metadata: `cargo sqlx prepare`

If you need to skip the database entirely, set `database.disabled = true` in `ferris.toml` (or override with `FERRIS_DATABASE_DISABLED=true`).

## Running locally (without containers)

1. Ensure `config/ferris.secrets.toml` is populated and `DATABASE_URL` points at your SQLite file.
2. Run the sqlx commands above to get the schema and offline metadata ready.
3. Start the bot: `cargo run` (or use the VS Code task "cargo run").

## Running with containers

The repository ships with a `Containerfile` and `compose.yaml` for containerized runs. The commands below use Docker Compose, but Podman Compose should also work (and is preferred).

1. Ensure `config/ferris.secrets.toml` is populated and `DATABASE_URL` points at your SQLite file.
2. Make sure your SQLite file exists at `database/ferris.sqlite3`. The compose file binds this, the `config/`, and the `logs/` directories into the container.
3. Build and run: `docker compose up --build ferris`

To run migrations inside the container instead of locally, you can use `docker compose run --rm ferris cargo sqlx database migrate` (or `... database setup` / `... prepare`).

If you need to access the container directly, you can use `docker compose run --rm --entrypoint /bin/sh ferris`.

## Local development and tests

- [Local Discord and tracing stack](local-test/README.md): run the bot with Fauxcord and Jaeger.
- [Command integration tests](tests/README.md): run Rust tests, review snapshots, and refresh API fixtures.

## Credits

This codebase has its roots in [rust-lang/discord-mods-bot](https://github.com/rust-lang/discord-mods-bot/), the Discord bot running on the official Rust server.
