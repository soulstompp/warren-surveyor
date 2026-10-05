-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Builders within 50 km of central Aarhus, per city
SELECT ci.city_id, ci.name, count(*) AS builders
FROM lego_builders b
JOIN lego_streets st ON st.street_id = b.street_id
JOIN lego_postcodes pc ON pc.postcode_id = st.postcode_id
JOIN lego_cities ci ON ci.city_id = pc.city_id
WHERE earth_box(ll_to_earth(56.1572, 10.2107), 50000) @> ll_to_earth(b.latitude, b.longitude)
  AND earth_distance(ll_to_earth(56.1572, 10.2107), ll_to_earth(b.latitude, b.longitude)) < 50000
GROUP BY ci.city_id, ci.name
ORDER BY builders DESC, ci.city_id;
