-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Gear's sets, per year
SELECT s.year, count(*) AS sets
FROM lego_sets s
WHERE s.theme_id = 501
GROUP BY s.year
ORDER BY s.year;
