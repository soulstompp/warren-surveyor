-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Sets whose name contains "aeroflot", newest first
SELECT s.set_num, s.name, s.year, t.name AS theme
FROM lego_sets s
JOIN lego_themes t ON t.id = s.theme_id
WHERE s.name ILIKE '%aeroflot%'
ORDER BY s.year DESC, s.set_num;
