-- Give a task a git branch name (task #81). Additive nullable column, the same
-- shape as `pr_url` in 0003: it holds the *intended* branch dispatch injects
-- into the agent's prompt, and is overwritten with the branch the agent
-- *reports* it actually worked on. Voro never runs git — it only passes this
-- name into the prompt and records what comes back.
ALTER TABLE tasks ADD COLUMN branch TEXT;
