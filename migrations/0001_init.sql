CREATE TABLE artifacts (
  id TEXT PRIMARY KEY,
  source TEXT NOT NULL,
  kind TEXT NOT NULL,
  uri TEXT NOT NULL,
  external_id TEXT,
  hash TEXT,
  authored_at INTEGER,
  observed_at INTEGER NOT NULL,
  author TEXT,
  metadata_json TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE spans (
  id TEXT PRIMARY KEY,
  artifact_id TEXT NOT NULL,
  text TEXT NOT NULL,
  start_line INTEGER,
  end_line INTEGER,
  start_byte INTEGER,
  end_byte INTEGER,
  metadata_json TEXT NOT NULL DEFAULT '{}',
  FOREIGN KEY (artifact_id) REFERENCES artifacts(id)
);

CREATE TABLE concepts (
  id TEXT PRIMARY KEY,
  canonical_name TEXT NOT NULL,
  kind TEXT,
  aliases_json TEXT NOT NULL DEFAULT '[]',
  metadata_json TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE evidence_items (
  id TEXT PRIMARY KEY,
  span_id TEXT NOT NULL,
  evidence_type TEXT NOT NULL,
  subject_text TEXT,
  subject_concept_id TEXT,
  predicate TEXT,
  object_text TEXT,
  value_json TEXT,
  unit TEXT,
  confidence REAL NOT NULL,
  authority TEXT NOT NULL,
  valid_from INTEGER,
  valid_to INTEGER,
  extraction_method TEXT NOT NULL,
  metadata_json TEXT NOT NULL DEFAULT '{}',
  FOREIGN KEY (span_id) REFERENCES spans(id),
  FOREIGN KEY (subject_concept_id) REFERENCES concepts(id)
);

CREATE TABLE checks (
  id TEXT PRIMARY KEY,
  trigger TEXT NOT NULL,
  question TEXT NOT NULL,
  question_type TEXT,
  status TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  metadata_json TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE evidence_queries (
  id TEXT PRIMARY KEY,
  check_id TEXT NOT NULL,
  source TEXT NOT NULL,
  query_type TEXT NOT NULL,
  query_text TEXT NOT NULL,
  time_from INTEGER,
  time_to INTEGER,
  result_summary_json TEXT NOT NULL DEFAULT '{}',
  executed_at INTEGER NOT NULL,
  FOREIGN KEY (check_id) REFERENCES checks(id)
);

CREATE TABLE verdicts (
  id TEXT PRIMARY KEY,
  check_id TEXT NOT NULL,
  status TEXT NOT NULL,
  confidence REAL NOT NULL,
  summary TEXT NOT NULL,
  caveats_json TEXT NOT NULL DEFAULT '[]',
  evidence_ids_json TEXT NOT NULL DEFAULT '[]',
  suggested_action TEXT,
  created_at INTEGER NOT NULL,
  FOREIGN KEY (check_id) REFERENCES checks(id)
);

CREATE INDEX idx_artifacts_source ON artifacts(source);
CREATE INDEX idx_artifacts_kind ON artifacts(kind);
CREATE INDEX idx_spans_artifact_id ON spans(artifact_id);
CREATE INDEX idx_evidence_subject ON evidence_items(subject_text);
CREATE INDEX idx_evidence_type ON evidence_items(evidence_type);
CREATE INDEX idx_checks_created_at ON checks(created_at);
CREATE INDEX idx_evidence_queries_check_id ON evidence_queries(check_id);
CREATE INDEX idx_verdicts_check_id ON verdicts(check_id);
