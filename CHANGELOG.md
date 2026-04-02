# Changelog

## Unreleased

### Signal

- Added Signal `mention_only` support for group chats using `signal-cli` mention metadata, while still allowing direct messages through the normal sender checks.
- Added Signal `allowed_groups` so members of approved groups can message the bot without listing every phone number in `allowed_from`.
- Kept `group_id` as a backward-compatible scope filter for existing configs instead of removing it and breaking persisted Signal setups.
