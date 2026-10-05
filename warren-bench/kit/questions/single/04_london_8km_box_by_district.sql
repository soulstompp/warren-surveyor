-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Builders in the 8 km box around central London: the 20 postcode districts with the most
SELECT pc.code, count(*) AS builders
FROM lego_builders b
JOIN lego_streets st ON st.street_id = b.street_id
JOIN lego_postcodes pc ON pc.postcode_id = st.postcode_id
WHERE earth_box(ll_to_earth(51.5085, -0.1115), 8000) @> ll_to_earth(b.latitude, b.longitude)
GROUP BY pc.code
ORDER BY builders DESC, pc.code
LIMIT 20;
