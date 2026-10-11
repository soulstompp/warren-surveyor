-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- The 20 newest Castle sets whose name contains "knight"
WITH RECURSIVE castle (id) AS (
    SELECT t.id FROM lego_themes t WHERE t.id = 186
  UNION ALL
    SELECT t.id FROM lego_themes t JOIN castle c ON t.parent_id = c.id
)
SELECT s.set_num, s.name, s.year
FROM castle
JOIN lego_sets s ON s.theme_id = castle.id
WHERE s.name ILIKE '%knight%'
ORDER BY s.year DESC, s.set_num
LIMIT 20;
