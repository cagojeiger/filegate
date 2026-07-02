-- files: identity and lifecycle. Physical location lives in file_locations (ADR 001).
create table files (
    id uuid primary key,
    client text not null,
    intent text not null,
    status text not null,
    current_location_id uuid,
    client_metadata jsonb not null default '{}',
    content_type text,
    verified_size bigint,
    verified_etag text,
    created_at timestamptz not null default now(),
    detached_at timestamptz
);
create index files_status_idx on files (status);
create index files_client_idx on files (client);

-- file_locations: where the bytes physically are. May change over a file's life.
create table file_locations (
    id uuid primary key,
    file_id uuid not null references files (id),
    provider text not null,
    bucket text not null,
    object_key text not null,
    state text not null,
    created_at timestamptz not null default now()
);
create index file_locations_file_idx on file_locations (file_id);

-- leases: the single audit unit for every byte-plane access (ADR 002).
create table leases (
    id uuid primary key,
    file_id uuid not null references files (id),
    client text not null,
    mode text not null,
    status text not null,
    declared_size bigint,
    expires_at timestamptz not null,
    committed_at timestamptz,
    created_at timestamptz not null default now()
);
create index leases_sweep_idx on leases (status, expires_at);
create index leases_file_idx on leases (file_id);

-- usage_counters: quota accounting. reserved at lease issue, settled at commit.
create table usage_counters (
    client text not null,
    intent text not null,
    active_bytes bigint not null default 0,
    reserved_bytes bigint not null default 0,
    primary key (client, intent)
);

-- audit_logs: append-only record of every control-plane decision.
create table audit_logs (
    id bigserial primary key,
    at timestamptz not null default now(),
    client text not null,
    file_id uuid,
    lease_id uuid,
    action text not null,
    detail jsonb not null default '{}'
);
create index audit_logs_file_idx on audit_logs (file_id);
