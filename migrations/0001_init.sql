-- Copyright 2026 The Ontele Authors
-- SPDX-License-Identifier: Apache-2.0
-- Ontele schema. One `items` table holds every playable thing (movies,
-- episodes, music tracks, DVR recordings) so watch state, tags, search and
-- artwork share one key space. Kind-specific columns are nullable.

CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TABLE users (
    id          BIGSERIAL PRIMARY KEY,
    subject     TEXT NOT NULL UNIQUE,
    email       TEXT,
    name        TEXT,
    groups      TEXT[] NOT NULL DEFAULT '{}',
    is_admin    BOOLEAN NOT NULL DEFAULT FALSE,
    created     TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE settings (
    id      INT PRIMARY KEY CHECK (id = 1),
    data    JSONB NOT NULL,
    updated TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE items (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL CHECK (kind IN ('movie', 'episode', 'track', 'recording')),
    path          TEXT UNIQUE,
    title         TEXT NOT NULL,
    sort_title    TEXT,
    year          INT,
    -- episodes
    show          TEXT,
    season        INT,
    episode       INT,
    episode_end   INT,
    air_date      DATE,
    -- music
    artist        TEXT,
    album_artist  TEXT,
    album         TEXT,
    album_id      TEXT,
    track_no      INT,
    disc_no       INT,
    genre         TEXT,
    -- shared descriptive
    subtitle      TEXT,
    description   TEXT,
    -- recordings
    channel_id    TEXT,
    channel_name  TEXT,
    start_at      TIMESTAMPTZ,
    end_at        TIMESTAMPTZ,
    status        TEXT,
    error         TEXT,
    rule_id       TEXT,
    breaks        JSONB,
    breaks_state  TEXT,
    -- technical + enrichment
    info          JSONB NOT NULL DEFAULT '{}'::jsonb,
    meta          JSONB NOT NULL DEFAULT '{}'::jsonb,
    auto_tags     TEXT[] NOT NULL DEFAULT '{}',
    size_bytes    BIGINT,
    mtime         TIMESTAMPTZ,
    added         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX items_kind_idx        ON items (kind);
CREATE INDEX items_added_idx       ON items (added DESC);
CREATE INDEX items_show_idx        ON items (lower(show), season, episode) WHERE kind = 'episode';
CREATE INDEX items_album_idx       ON items (album_id, disc_no, track_no) WHERE kind = 'track';
CREATE INDEX items_artist_idx      ON items (lower(album_artist)) WHERE kind = 'track';
CREATE INDEX items_rec_status_idx  ON items (status, start_at DESC) WHERE kind = 'recording';
CREATE INDEX items_rule_idx        ON items (rule_id) WHERE rule_id IS NOT NULL;
CREATE INDEX items_title_trgm_idx  ON items USING gin (title gin_trgm_ops);
CREATE INDEX items_show_trgm_idx   ON items USING gin (show gin_trgm_ops) WHERE show IS NOT NULL;
CREATE INDEX items_album_trgm_idx  ON items USING gin (album gin_trgm_ops) WHERE album IS NOT NULL;
CREATE INDEX items_artist_trgm_idx ON items USING gin (artist gin_trgm_ops) WHERE artist IS NOT NULL;
CREATE INDEX items_genres_idx      ON items USING gin ((meta -> 'genres'));

CREATE TABLE shows (
    key     TEXT PRIMARY KEY,
    name    TEXT NOT NULL,
    meta    JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE albums (
    id      TEXT PRIMARY KEY,
    artist  TEXT NOT NULL,
    title   TEXT NOT NULL,
    year    INT,
    meta    JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE watch (
    user_id BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    item_id TEXT NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    pos     DOUBLE PRECISION NOT NULL DEFAULT 0,
    dur     DOUBLE PRECISION NOT NULL DEFAULT 0,
    done    BOOLEAN NOT NULL DEFAULT FALSE,
    updated TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, item_id)
);
CREATE INDEX watch_user_updated_idx ON watch (user_id, updated DESC);

CREATE TABLE rules (
    id         TEXT PRIMARY KEY,
    title      TEXT NOT NULL,
    channel_id TEXT,
    keep       INT NOT NULL DEFAULT 0,
    user_id    BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE tags (
    id   SERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE item_tags (
    item_id TEXT NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    tag_id  INT NOT NULL REFERENCES tags (id) ON DELETE CASCADE,
    PRIMARY KEY (item_id, tag_id)
);
CREATE INDEX item_tags_tag_idx ON item_tags (tag_id);

CREATE TABLE activity (
    id      BIGSERIAL PRIMARY KEY,
    ts      TIMESTAMPTZ NOT NULL DEFAULT now(),
    user_id BIGINT REFERENCES users (id) ON DELETE SET NULL,
    kind    TEXT NOT NULL,
    item_id TEXT,
    detail  JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX activity_ts_idx ON activity (ts DESC);

CREATE TABLE channels (
    guide_number TEXT PRIMARY KEY,
    guide_name   TEXT NOT NULL,
    url          TEXT NOT NULL,
    hd           BOOLEAN NOT NULL DEFAULT FALSE,
    icon         TEXT,
    updated      TIMESTAMPTZ NOT NULL DEFAULT now()
);
