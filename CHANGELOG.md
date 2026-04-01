# Changelog

## Unreleased

### Home Assistant

- Added a shared Home Assistant client/runtime module so the built-in tool and related utilities can reuse the same URL validation, token loading, and request execution behavior.
- Fixed `call_service` request shaping to match Home Assistant's REST API by flattening `entity_id` and `target` fields into top-level service data instead of sending a nested `target` object that many services reject with `400 Bad Request`.
- Improved token parsing from `.env`-style files so quoted values, `export` prefixes, and trailing inline comments are handled consistently.
- Expanded the tool guidance and tests around shorthand entity targeting and light dimming payloads, including `brightness_pct` usage for `light.turn_on`.

### Signal

- Added Signal `mention_only` support for group chats using `signal-cli` mention metadata, while still allowing direct messages through the normal sender checks.
- Added Signal `allowed_groups` so members of approved groups can message the bot without listing every phone number in `allowed_from`.
- Kept `group_id` as a backward-compatible scope filter for existing configs instead of removing it and breaking persisted Signal setups.
