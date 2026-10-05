-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- December purchases of 2016 to 2020, per year, with how many builders bought
SELECT extract(year FROM lego.clock(p.ordered_at))::int AS year,
       count(*) AS purchases, count(DISTINCT p.builder_id) AS builders
FROM lego_purchases p
WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12
  AND lego.clock(p.ordered_at) >= '2016-12-01' AND lego.clock(p.ordered_at) < '2021-01-01'
GROUP BY 1
ORDER BY 1;
