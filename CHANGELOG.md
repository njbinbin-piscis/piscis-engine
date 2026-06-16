# Changelog

## [0.8.62] - 2026-06-13

### Added

- **`list_all_artifacts`**: global My Files aggregation across sessions.
- **Notification `pool_name` and session schema extensions** (from v0.8.61 line).

### Fixed

- **`is_allowed_plan_path`**: cross-platform plan path validation on Windows/macOS (merged from v0.8.60).

## [0.8.60] - 2026-06-13

### Fixed

- **`is_allowed_plan_path`**: cross-platform plan path validation on Windows/macOS (no longer requires the target file to exist; avoids `canonicalize` prefix mismatches).

## [0.8.59] - 2026-06-12

### Added

- **`pool_sessions.team_id` / `workflow_run_id`**：按团队与工作流 run 隔离 pool；`find_active_pool(project_dir, team_id, team_name)`。
- **`assign_koi` dependency waiting**：依赖未满足时 emit `TodoChanged` + `dependency_waiting` pool message。

### Changed

- **`pool_org` 工具描述**：补充 workflow_hint 与 depends_on 语义说明。

## [0.8.55] - 2026-06-06

### Fixed
- **OpenAI tool_calls pairing**: per-`tool_use_id` sanitize strips partially satisfied assistant tool calls (cancel / parallel interrupt / supersede collapse) so the API no longer returns 400 for orphaned `tool_call_id`s.
- **Agent loop cancel**: inject synthetic error `ToolResult`s for unexecuted tools before persisting; sanitize pairing after each tool round and in `build_request_messages` before every LLM call.

## [0.8.54] - 2026-06-08

### Added
- Journal turn diffs and improved agent loop error surfacing.

## [0.8.47] - 2026-06-07

### Added
- **`SceneKind::KoiPersona`**: main-chat persona mode with a dedicated tool registry profile and `koi_persona_*` prompt protocol.
- **`build_koi_persona_system_prompt`**: assembles Koi identity, instructions, and project context for direct user conversation (no pool collaboration tools).

### Changed
- **Vision delegation**: `delegate_vision_analysis` runs per attachment image instead of batching multiple images in one call.

## [0.8.38] - 2026-06-05

### Added
- **Pluggable loop strategies** (`LoopStrategy` trait) and runtime **contrib registry** for host-supplied loop/compaction strategies.
- Built-in compaction modes: `sliding_window`, `vector_retrieval`.
- `HarnessConfig` slots: `loop_strategy`, `memory_retrieval_prompt`.

### Changed
- Crate versions aligned to the **0.8.x** release line (supersedes the mistaken `v0.7.38` tag).
- Consumers should pin `rev = "v0.8.38"` (not `v0.7.38`).

## [0.8.25] - 2026-05-31

- Stricter heartbeat / org_spec convergence (consumed by openpiscis 0.8.25).
