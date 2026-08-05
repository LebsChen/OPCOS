# Devin web API and timeline rendering, observed

Captured from a real logged-in `app.devin.ai` session on 2026-08-05 by recording the
browser's own network traffic (no credentials, ids, or bodies are reproduced here
beyond the shapes needed to implement against them).

## Session view endpoints

| Endpoint | Purpose |
| --- | --- |
| `GET /api/events/first-load/<devin_id>` | `{"result": [event, ...]}` — the renderable subset of the event log, newest last |
| `GET /api/events/<devin_id>/stream?order=desc` | NDJSON `{"result": [event, ...]}` — the full event log, and the live tail |
| `GET /api/sessions/<devin_id>` | session metadata (title, status, machine, agent version) |
| `GET /api/sessions/<devin_id>/prs` | right rail: pull requests |
| `GET /api/ide/<devin_id>/file_diffs` | right rail: Changes tab |
| `GET /api/presigned-url/batch/<devin_id>` | S3 URLs for `editor_files/*` and `terminal_contents/*` payloads referenced by events |
| `GET /api/knowledge-suggestions/all-suggestions/<devin_id>` | knowledge suggestions |
| `GET /api/billing/usage/session/<devin_id>` | ACU usage for the session |

The important structural fact: **first-load and the stream return the same event
objects.** The client keeps one append-only list keyed by `event_id` and ordered by
`created_at_ms`; a reload refetches that list. Rendering is a pure function of it, so
live and reloaded views cannot diverge. There is no second server-side projection of
the conversation.

Every event carries `type`, `timestamp` (RFC 3339), `event_id` (`event-<ulid>`) and
`created_at_ms`.

## Event types

Observed in one session (176 events):

| Type | Payload fields beyond the common ones |
| --- | --- |
| `initial_user_message` | `message`, `rich_content`, `origin`, `user_id`, `username`, `email` |
| `user_message` | `message`, `rich_content`, `origin`, `user_id`, `username` |
| `devin_message` | `message` |
| `devin_thoughts` | `message`, `thinking_duration_ms` |
| `one_line_thoughts` | `short`, `summary` |
| `shell_process_started` | `command`, `shell_id`, `process_id`, `starting_dir`, `acu_consumption`, `is_major_action` |
| `shell_process_completed` | `process_id`, `exit_code`, `output_trunc` |
| `shell_process_completed_background` | same as above |
| `terminal_update` | `shell_id`, `contents_gzip` (base64 gzip) or `contents_key` (S3) |
| `multi_edit_result` | `action_uuid`, `has_write`, `is_major_action`, `acu_consumption`, `file_updates[]` |
| `todo_update` | `todos[{status, content}]`, `total_count`, `pending_count`, `in_progress_count`, `completed_count` |
| `status_update` | `enum`, `message`, `reason`, `user_action_required`, `resume_reason`, `hours_inactivity`, `minutes_inactivity` |
| `simple_activity_update` | `enum` |
| `is_typing` | `value` |
| `iteration_stats` | `iteration`, `num_tool_calls`, `total_ms`, `inference_ms`, `tool_exec_ms`, `harness_ms` |
| `iteration_checkpoint` | `iteration`, `last_processed_incoming_event_id` |
| `context_growth_update` | `current_context_bytes`, `current_context_tokens`, `iteration_count`, `per_source_context_bytes`, `tool_aggregates[]`, `total_tool_*` |
| `session_snapshot` | `iteration`, `cogs_s3_key`, `forest_s3_key` |
| `skills_available` | `skills[{name, path, description, repo_name, git_branch, git_commit_sha, content_hash, working_tree_dirty}]` |
| `repo_setup_initialized` | `org_id`, `devin_id`, `repo_info[]` |
| `initializing`, `initialized` | `title` on `initialized` |
| `resuming_session` | `message`, `resume_reason` |
| `resume_requested_frontend` | `resume_reason` |
| `self_suspend`, `devin_suspended` | `reason`, `hours_inactivity`, `minutes_inactivity` |
| `play` | `user_id`, `username` |
| `set_devin_version` | `version`, `user_id`, `username` |
| `session_analysis` | `state_update`, `session_analysis_id`, `data.classification{category, confidence, programming_languages, tools_and_frameworks}` |
| `acu_consumption_at_last_user_interaction` | `amount`, `overage_amount` |

`multi_edit_result.file_updates[]` entries carry `file_path`, `action_type`
(`edit` / `create`), `start_line`, `end_line`, `lines_added`, `lines_removed`,
`contents_key` and `prev_contents_key`.

## Timeline rendering

DOM markers: `message-history--item message-item` for message bubbles,
`worklog-group` and `highlight-wrapper` for grouped work, `group/event-header` for
the expandable header of a group, `data-compact-row` for compact rows.

Work between two message bubbles collapses into one group whose header is
`Worked for <duration>` plus `+<additions>` and `−<deletions>` aggregated from the
`multi_edit_result` events inside it. Rows inside the group, in event order:

| Event | Row |
| --- | --- |
| `shell_process_started` | the raw command text |
| `devin_thoughts` | `Thought for <thinking_duration_ms rounded to seconds>` — a single collapsed line, the reasoning text is only shown when expanded |
| `multi_edit_result` with `action_type=edit` | `Edited <basename>` `+N` `−M` |
| `multi_edit_result` with `action_type=create` | `Created <basename>` `+N` |
| `todo_update` that adds items | `Created <n> Task(s)` |
| `todo_update` that advances items | `<completed>/<total>#<index> <content>` |

`devin_message` renders as an assistant bubble outside the group and closes it.
`user_message` renders as a user bubble with a `Tue 5:17 PM` style time header and
starts a new group. `set_devin_version` renders as `Switched to <version>`.
`devin_suspended` renders as `Devin went to sleep` with a `Wake Devin up?` action.

## Right rail

Two tabs: `Progress` and `Changes`.

`Progress` is the machine view. Its header is the current action rendered as
`Executed<command>` for a shell call, below it an xterm surface replaying the
`terminal_update` contents for that `shell_id`, and a `Live` badge with a red dot
while the session is attached. A scrubber above the toolbar seeks through the
recorded terminal history.

`Changes` is backed by `GET /api/ide/<devin_id>/file_diffs`. Each entry renders as
the file basename, its directory path on a second line, a `+N` / `−M` badge and a
status word (`Added`, `Modified`), followed by the file body with line numbers and
per-line add/remove highlighting.
