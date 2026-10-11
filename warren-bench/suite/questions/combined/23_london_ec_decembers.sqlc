-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- December purchases of 2016 to 2020 by builders in London's EC postcode districts, per district
SELECT pc.code, count(*) AS purchases, count(DISTINCT p.builder_id) AS builders
FROM lego_postcodes pc
JOIN lego_streets st ON st.postcode_id = pc.postcode_id
JOIN lego_builders b ON b.street_id = st.street_id
JOIN lego_purchases p ON p.builder_id = b.builder_id
WHERE pc.code LIKE 'EC%'
  AND extract(month FROM lego.clock(p.ordered_at))::smallint = 12
  AND lego.clock(p.ordered_at) >= '2016-12-01' AND lego.clock(p.ordered_at) < '2021-01-01'
GROUP BY pc.code
ORDER BY purchases DESC, pc.code;
