-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- How many of Castle's sets use each primary colour
WITH RECURSIVE castle (id) AS (
    SELECT t.id FROM lego_themes t WHERE t.id = 186
  UNION ALL
    SELECT t.id FROM lego_themes t JOIN castle c ON t.parent_id = c.id
)
SELECT co.id, co.name AS colour, count(DISTINCT s.set_num) AS sets
FROM castle
JOIN lego_sets s ON s.theme_id = castle.id
JOIN lego_inventories i ON i.set_num = s.set_num
JOIN lego_inventory_parts ip ON ip.inventory_id = i.id
JOIN lego_colors co ON co.id = ip.color_id
WHERE lego_oo.colour_wheel(co.rgb) IN (0, 4, 8)
GROUP BY co.id, co.name
ORDER BY sets DESC, co.id;
