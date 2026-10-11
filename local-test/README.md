# Local Discord and tracing stack

Run Ferrisbot against Fauxcord (a local Discord REST API and Gateway mock), an
OpenTelemetry Collector tail sampler, and Jaeger using a fake bot token and
isolated Docker volumes. Interactive Playground and Godbolt commands contact
the public APIs.

For Rust integration tests, snapshot review, and API fixture refreshes, see
[the test guide](../tests/README.md).

## Start the stack

Run from the repository root:

```sh
docker compose -f compose.local.yaml up --build
```

Configuration is in `local-test/`. Open Jaeger at <http://localhost:16686> and
select the `ferrisbot-local` service. Rustbot exports every span to the local
Collector. It keeps all traces except message highlight checks, which are
sampled at 1%; a check that matches is always kept. Command spans are named
`command.<command>`, with the invoked command name in the `command`
attribute. Internal highlight spans use `highlight.*`; each Discord SDK operation
exports a low-cardinality name such as `discord.guild.member`, with CLIENT span
kind, `peer.service=discord`, and `server.address=discord.com`. Jaeger uses the
peer service to distinguish calls to the uninstrumented Discord dependency.
These spans measure SDK operations (which may use caches or multiple requests),
not individual HTTP requests. The Collector adds up to three seconds of visibility delay while it
waits to decide.

External HTTP requests use operation names such as `rust-playground.execute`
and `godbolt.compile`, with CLIENT kind, `peer.service`, server address, request
method, and numeric response status. SQLite executions use names such as
`SELECT highlights` and `UPDATE server_icon_rotation`, with CLIENT kind,
`db.system.name=sqlite`, `db.namespace=main`, operation and collection attributes,
and `db.query.summary`. Legacy `db.system` and `db.name` attributes support
consumers still migrating to the stable database conventions. Internal database
workflows keep their `db.*` names and contain the individual query spans.
Query spans omit SQL text and bound values; SQLite has no remote server address.

The `ferris` service depends on `otel-collector`; traces flow through the
Collector to Jaeger. The Collector's OTLP endpoint is also available on
`localhost:4319` for local OTLP experiments.

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
