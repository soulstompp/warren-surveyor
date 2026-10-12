-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Purchases per day in December 2020
SELECT lego.clock(p.ordered_at)::date AS day, count(*) AS purchases
FROM lego_purchases p
WHERE lego.clock(p.ordered_at) >= '2020-12-01' AND lego.clock(p.ordered_at) < '2021-01-01'
GROUP BY 1
ORDER BY 1;
