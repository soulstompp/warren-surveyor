-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Sets released from 1980 to 1999 under LEGO's own themes, per root theme
WITH RECURSIVE root (theme_id, root_id) AS (
    SELECT t.id, t.id FROM lego_themes t WHERE t.parent_id IS NULL
  UNION ALL
    SELECT t.id, r.root_id FROM lego_themes t JOIN root r ON t.parent_id = r.theme_id
)
SELECT rt.id AS root_id, rt.name AS root_theme, count(*) AS sets
FROM lego_sets s
LEFT JOIN root r ON r.theme_id = s.theme_id
LEFT JOIN lego_themes rt ON rt.id = r.root_id
WHERE s.year >= 1980 AND s.year < 2000
  AND (r.root_id IS NULL OR r.root_id NOT IN (158, 482, 246, 561, 264, 269, 272, 570, 579, 577))
GROUP BY rt.id, rt.name
ORDER BY sets DESC, rt.id;
