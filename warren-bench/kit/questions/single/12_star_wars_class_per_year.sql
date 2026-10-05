-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- The Star Wars class's sets, per year
SELECT s.year, count(*) AS sets
FROM lego_sets s
WHERE s.theme_id IN (18, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 209, 225, 261, 431, 612, 613)
GROUP BY s.year
ORDER BY s.year;
