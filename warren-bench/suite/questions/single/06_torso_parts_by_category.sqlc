-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Parts whose name has the word "torso", per category
SELECT pc.id, pc.name AS category, count(*) AS parts
FROM lego_parts p
JOIN lego_part_categories pc ON pc.id = p.part_cat_id
WHERE to_tsvector('english', p.name) @@ to_tsquery('english', 'torso')
GROUP BY pc.id, pc.name
ORDER BY parts DESC, pc.id;
