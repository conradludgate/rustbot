# Local Discord and tracing stack

Run Ferrisbot against Fauxcord (a local Discord REST API and Gateway mock) and
Jaeger using a fake bot token and isolated Docker volumes. Interactive
Playground and Godbolt commands contact the public APIs.

For Rust integration tests, snapshot review, and API fixture refreshes, see
[the test guide](../tests/README.md).

## Start the stack

Run from the repository root:

```sh
docker compose -f compose.local.yaml up --build
```

Configuration is in `local-test/config/`. Open Jaeger at
<http://localhost:16686> and select the `ferrisbot-local` service. Sampling is
set to 100%; the log filter keeps application `INFO` spans and suppresses
Serenity spans. Command spans are named `discord.command.<command>`, with the
invoked command name in the `command` attribute.

## Send a command

Register a fake user and send `?uptime` to the mock `general` channel:

```sh
tester_id=$(curl -fsS -X POST http://localhost:3000/_test/users \
  -H 'Content-Type: application/json' \
  -d '{"username":"Local Tester"}' | jq -r .id)

curl -fsS -X POST http://localhost:3000/_test/channels/1234567890123456789/messages \
  -H 'Content-Type: application/json' \
  -d "{\"content\":\"?uptime\",\"author\":{\"id\":\"${tester_id}\"}}"
```

To exercise moderator controls, give the fake member the `MOD_ROLE_ID` from
`local-test/config/ferris.secrets.toml`, then send `?server_icon pause`,
`?server_icon resume`, or `?server_icon set owo` to the same channel.

## Stop or reset

```sh
docker compose -f compose.local.yaml down
```

To also remove this stack's database and mock state:

```sh
docker compose -f compose.local.yaml down --volumes
```

The local stack and the integration test service both bind port 3000. Stop one
before starting the other. Their Compose projects use separate data volumes.
