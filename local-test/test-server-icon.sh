#!/bin/sh
set -eu

fauxcord_url=${FAUXCORD_URL:-http://localhost:3000}
channel_id=1234567890123456789

tester_id=$(curl -fsS -X POST "$fauxcord_url/_test/users" \
	-H 'Content-Type: application/json' \
	-d '{"username":"Server icon test"}' | jq -r '.id')

inject_command() {
	command=$1
	curl -fsS -X POST "$fauxcord_url/_test/channels/$channel_id/messages" \
		-H 'Content-Type: application/json' \
		-d "{\"content\":\"?server_icon $command\",\"author\":{\"id\":\"$tester_id\"}}" >/dev/null
}

wait_for_reply() {
	needle=$1
	attempt=0
	while [ "$attempt" -lt 15 ]; do
		messages=$(curl -fsS "$fauxcord_url/_test/messages/$channel_id")
		if printf '%s' "$messages" | jq -e --arg needle "$needle" \
			'.messages[] | select((.author_token // "") != "" and (.content | contains($needle)))' \
			>/dev/null; then
			return 0
		fi
		attempt=$((attempt + 1))
		sleep 1
	done
	echo "Timed out waiting for bot reply containing: $needle" >&2
	return 1
}

inject_command pause
wait_for_reply 'This command is only available to moderators.'

inject_command status
wait_for_reply 'github.com/conradludgate/rustbot/blob/main/assets/server-icons/'

inject_command list
wait_for_reply 'Available icons:'
wait_for_reply '`owo` <https://github.com/conradludgate/rustbot/blob/main/assets/server-icons/server_icon_owo.png?raw=true>'

echo 'Fauxcord moderator denial, status, and list checks passed.'
