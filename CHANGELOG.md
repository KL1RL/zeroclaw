# Changelog

## Unreleased

### Home Assistant

- Added a shared Home Assistant client/runtime module so the built-in tool and related utilities can reuse the same URL validation, token loading, and request execution behavior.
- Fixed `call_service` request shaping to match Home Assistant's REST API by flattening `entity_id` and `target` fields into top-level service data instead of sending a nested `target` object that many services reject with `400 Bad Request`.
- Improved token parsing from `.env`-style files so quoted values, `export` prefixes, and trailing inline comments are handled consistently.
- Expanded the tool guidance and tests around shorthand entity targeting and light dimming payloads, including `brightness_pct` usage for `light.turn_on`.
