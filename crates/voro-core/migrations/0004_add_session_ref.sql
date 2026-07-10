-- The agent's own reference for a dispatched session (task #75): a Claude
-- session UUID, a Codex session id, a tmux session name. Captured after
-- launch (nullable — not every agent has a capture story) and substituted
-- into the agent's attach/resume/continue verb templates.
ALTER TABLE sessions ADD COLUMN session_ref TEXT;
