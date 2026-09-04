-- Native multipart completion ownership (spec 02).
--
-- External CompleteMultipart/rename and the PostgreSQL commit cannot be one
-- transaction. This durable row excludes generic expiry reclaim while the
-- request owns completion and gives the reconciler enough information to
-- finish or reopen an interrupted completion.
CREATE TABLE native_multipart_completions (
    file_id       uuid PRIMARY KEY REFERENCES files (id) ON DELETE CASCADE,
    state         text NOT NULL DEFAULT 'completing'
                  CHECK (state IN ('completing', 'cleaning')),
    expected_etag text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX native_multipart_completions_state_idx
    ON native_multipart_completions (state, updated_at);
