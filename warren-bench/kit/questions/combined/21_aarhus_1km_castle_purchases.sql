-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- The 20 newest purchases of sets named with "castle" by builders in the 1 km box around central Aarhus
SELECT p.purchase_id, lego.clock(p.ordered_at) AS ordered, s.set_num, s.name
FROM lego_builders b
JOIN lego_purchases p ON p.builder_id = b.builder_id
JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no
JOIN lego_sets s ON s.set_num = c.set_num
WHERE earth_box(ll_to_earth(56.1572, 10.2107), 1000) @> ll_to_earth(b.latitude, b.longitude)
  AND s.name ILIKE '%castle%'
ORDER BY p.ordered_at DESC, p.purchase_id
LIMIT 20;
