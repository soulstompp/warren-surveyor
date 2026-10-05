-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- The sets of the theme "Unlisted theme 618", with how many collection rows name each
SELECT s.set_num, s.name, s.year, count(c.row_no) AS collection_rows
FROM lego_sets s
LEFT JOIN lego_collection c ON c.set_num = s.set_num
WHERE s.theme_id = 618
GROUP BY s.set_num, s.name, s.year
ORDER BY s.set_num;
