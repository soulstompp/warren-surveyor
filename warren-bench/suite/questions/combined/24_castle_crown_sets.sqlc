-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Castle's sets that hold a minifig accessory named "crown", per theme
WITH RECURSIVE castle (id) AS (
    SELECT t.id FROM lego_themes t WHERE t.id = 186
  UNION ALL
    SELECT t.id FROM lego_themes t JOIN castle c ON t.parent_id = c.id
)
SELECT s.theme_id, count(*) AS sets
FROM castle
JOIN lego_sets s ON s.theme_id = castle.id
WHERE EXISTS (
        SELECT 1
        FROM lego_inventories i
        JOIN lego_inventory_parts ip ON ip.inventory_id = i.id
        JOIN lego_parts pt ON pt.part_num = ip.part_num
        WHERE i.set_num = s.set_num
          AND pt.part_cat_id = 27
          AND to_tsvector('english', pt.name) @@ to_tsquery('english', 'crown'))
GROUP BY s.theme_id
ORDER BY s.theme_id;
