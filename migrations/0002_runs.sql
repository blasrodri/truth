-- Command receipts: `truth run -- <cmd>` records what ran, when, and how it
-- exited, making agent success claims ("tests pass") verifiable. Output is
-- never stored raw — only a digest and a short redacted tail.
CREATE TABLE runs (
  id TEXT PRIMARY KEY,
  command TEXT NOT NULL,
  kind TEXT NOT NULL,
  exit_code INTEGER NOT NULL,
  started_at INTEGER NOT NULL,
  finished_at INTEGER NOT NULL,
  duration_ms INTEGER,
  output_digest TEXT,
  output_tail TEXT,
  metadata_json TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX idx_runs_kind_finished ON runs(kind, finished_at);
