-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- December's purchases in every year, per year
SELECT extract(year FROM lego.clock(p.ordered_at))::int AS year, count(*) AS purchases
FROM lego_purchases p
WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12
GROUP BY 1
ORDER BY 1;
