-- Copyright 2026 The Ontele Authors
-- SPDX-License-Identifier: Apache-2.0
-- Item deletions cascade into play_log; without this the FK check walks the
-- whole table per deleted item (library rescans delete in bulk).
CREATE INDEX play_log_item_idx ON play_log (item_id);
