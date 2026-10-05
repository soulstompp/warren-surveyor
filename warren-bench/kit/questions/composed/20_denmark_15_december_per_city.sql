-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Purchases on 15 December 2020 by builders in Denmark, per city
SELECT ci.city_id, ci.name, count(*) AS purchases
FROM lego_purchases p
JOIN lego_builders b ON b.builder_id = p.builder_id
JOIN lego_streets st ON st.street_id = b.street_id
JOIN lego_postcodes pc ON pc.postcode_id = st.postcode_id
JOIN lego_cities ci ON ci.city_id = pc.city_id
WHERE lego.clock(p.ordered_at) >= '2020-12-15' AND lego.clock(p.ordered_at) < '2020-12-16'
  AND ci.country = 'DK'
GROUP BY ci.city_id, ci.name
ORDER BY purchases DESC, ci.city_id;
