-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- December 2020 purchases by builders living within 15 km of central London, per day
SELECT lego.clock(p.ordered_at)::date AS day, count(*) AS purchases
FROM lego_purchases p
JOIN lego_builders b ON b.builder_id = p.builder_id
WHERE lego.clock(p.ordered_at) >= '2020-12-01' AND lego.clock(p.ordered_at) < '2021-01-01'
  AND earth_box(ll_to_earth(51.5085, -0.1115), 15000) @> ll_to_earth(b.latitude, b.longitude)
  AND earth_distance(ll_to_earth(51.5085, -0.1115), ll_to_earth(b.latitude, b.longitude)) < 15000
GROUP BY 1
ORDER BY 1;
