-- Copyright 2026 The Ontele Authors
-- SPDX-License-Identifier: Apache-2.0
-- Day-grain playback log behind the Trending views: one row per
-- (user, item, day) accumulating watched seconds and started plays.
CREATE TABLE play_log (
    user_id BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    item_id TEXT NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    day     DATE NOT NULL,
    seconds DOUBLE PRECISION NOT NULL DEFAULT 0,
    views   INT NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, item_id, day)
);
CREATE INDEX play_log_day_idx ON play_log (day DESC);
