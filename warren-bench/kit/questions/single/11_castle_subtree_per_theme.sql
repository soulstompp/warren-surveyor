-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- The sets under Castle (Castle and every theme below it), per theme
WITH RECURSIVE castle (id) AS (
    SELECT t.id FROM lego_themes t WHERE t.id = 186
  UNION ALL
    SELECT t.id FROM lego_themes t JOIN castle c ON t.parent_id = c.id
)
SELECT s.theme_id, count(*) AS sets
FROM castle
JOIN lego_sets s ON s.theme_id = castle.id
GROUP BY s.theme_id
ORDER BY s.theme_id;
