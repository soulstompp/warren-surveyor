-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- The purchases of two hours on 15 December 2020, with the set each bought
SELECT p.purchase_id, lego.clock(p.ordered_at) AS ordered, s.set_num, s.name
FROM lego_purchases p
JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no
JOIN lego_sets s ON s.set_num = c.set_num
WHERE lego.clock(p.ordered_at) >= '2020-12-15 10:00' AND lego.clock(p.ordered_at) < '2020-12-15 12:00'
ORDER BY p.purchase_id;
