-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Minifig accessories whose name has the word "helmet"
SELECT p.part_num, p.name
FROM lego_parts p
WHERE p.part_cat_id = 27
  AND to_tsvector('english', p.name) @@ to_tsquery('english', 'helmet')
ORDER BY p.part_num;
