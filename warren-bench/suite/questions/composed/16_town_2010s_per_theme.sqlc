-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Town's sets released from 2010 to 2019, per theme
WITH RECURSIVE town (id) AS (
    SELECT t.id FROM lego_themes t WHERE t.id = 50
  UNION ALL
    SELECT t.id FROM lego_themes t JOIN town w ON t.parent_id = w.id
)
SELECT s.theme_id, th.name AS theme, count(*) AS sets
FROM town
JOIN lego_sets s ON s.theme_id = town.id
JOIN lego_themes th ON th.id = s.theme_id
WHERE s.year >= 2010 AND s.year < 2020
GROUP BY s.theme_id, th.name
ORDER BY sets DESC, s.theme_id;
