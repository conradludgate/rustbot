# Local Discord and tracing stack

This stack runs Ferrisbot against Fauxcord, a local Discord REST API and Gateway mock, and sends sampled traces to Jaeger. It uses a fake bot token and isolated Docker volumes; it does not use the production Discord credentials or database.

Start everything from the repository root:

```sh
docker compose -f compose.local.yaml up --build
```

Open Jaeger at <http://localhost:16686>. Look for the `ferrisbot-local` service. Sampling is set to 100% in `local-test/config/ferris.toml` so local command traces are easy to find.
The log filter keeps application `INFO` spans and suppresses Serenity spans, avoiding a trace that grows for the lifetime of the Gateway shard.
Command spans use names based on the invoked command, such as `discord.command.uptime` or `discord.command.tags.create`; the `command` attribute records the runtime-qualified command name as well.

To inject a `?uptime` message into the mock `general` channel and exercise a traced command, register a fake user and send a Gateway message:

```sh
tester_id=$(curl -fsS -X POST http://localhost:3000/_test/users \
  -H 'Content-Type: application/json' \
  -d '{"username":"Local Tester"}' | jq -r .id)

curl -fsS -X POST http://localhost:3000/_test/channels/1234567890123456789/messages \
  -H 'Content-Type: application/json' \
  -d "{\"content\":\"?uptime\",\"author\":{\"id\":\"${tester_id}\"}}"
```

Stop the stack with `docker compose -f compose.local.yaml down`. Its data volumes are separate from the regular Compose setup. To reset only this local test state, remove the volumes for this Compose project with `docker compose -f compose.local.yaml down --volumes`.
