-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Purchases of the Star Wars class's sets in the Decembers of 2016 to 2020, per year
SELECT extract(year FROM lego.clock(p.ordered_at))::int AS year, count(*) AS purchases
FROM lego_purchases p
JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no
JOIN lego_sets s ON s.set_num = c.set_num
WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12
  AND lego.clock(p.ordered_at) >= '2016-12-01' AND lego.clock(p.ordered_at) < '2021-01-01'
  AND s.theme_id IN (18, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 209, 225, 261, 431, 612, 613)
GROUP BY 1
ORDER BY 1;
