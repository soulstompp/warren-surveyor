-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Town's truck sets with a helmet accessory, bought in the Decembers of 2016 to 2020 near central London
WITH RECURSIVE town (id, name) AS (
    SELECT t.id, t.name FROM lego_themes t WHERE t.id = 50
  UNION ALL
    SELECT t.id, t.name FROM lego_themes t JOIN town w ON t.parent_id = w.id
)
SELECT town.id AS theme_id,
       town.name AS theme,
       count(*) AS purchases,
       count(DISTINCT p.builder_id) AS builders,
       max(lego.clock(p.ordered_at)) AS newest
FROM lego_purchases p
JOIN lego_builders b ON b.builder_id = p.builder_id
JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no
JOIN lego_sets s ON s.set_num = c.set_num
JOIN town ON town.id = s.theme_id
WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12
  AND lego.clock(p.ordered_at) >= '2016-12-01' AND lego.clock(p.ordered_at) < '2021-01-01'
  AND earth_box(ll_to_earth(51.5085, -0.1115), 15000) @> ll_to_earth(b.latitude, b.longitude)
  AND earth_distance(ll_to_earth(51.5085, -0.1115), ll_to_earth(b.latitude, b.longitude)) < 15000
  AND s.name ILIKE '%truck%'
  AND EXISTS (
        SELECT 1
        FROM lego_inventories i
        JOIN lego_inventory_parts ip ON ip.inventory_id = i.id
        JOIN lego_parts pt ON pt.part_num = ip.part_num
        WHERE i.set_num = s.set_num
          AND pt.part_cat_id = 27
          AND to_tsvector('english', pt.name) @@ to_tsquery('english', 'helmet'))
GROUP BY town.id, town.name
ORDER BY purchases DESC, builders DESC, theme_id
LIMIT 10;
