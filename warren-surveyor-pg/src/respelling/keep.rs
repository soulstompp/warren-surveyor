// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! Each side keeps only the keys the other holds: where a grouping is taken on each side first, the
//! side the counted table is not on is grouped only over the keys the counted side's grouping holds.

#[pgrx::pg_schema]
mod tests {
    use crate::respelling::catalogue::*;
    use pgrx::prelude::*;

    /// The December purchases, by home zone and part category.
    const IN_DECEMBER: &str = "WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12";

    #[pg_test]
    fn the_side_without_the_counted_table_is_grouped_only_at_the_keys_the_counted_side_holds() {
        lego(Indexes::Surveyors);
        let query = by_zone_and_category_with(
            "GROUP BY 1, 2 ORDER BY",
            &format!("{IN_DECEMBER} GROUP BY 1, 2 ORDER BY"),
        );
        let per_set = BY_ZONE_AND_CATEGORY_PER_SET.replacen(
            "GROUP BY 1, 2), k AS",
            &format!("{IN_DECEMBER} GROUP BY 1, 2), k AS"),
            1,
        );
        assert_ne!(per_set, BY_ZONE_AND_CATEGORY_PER_SET);
        assert!(respelled(&query));
        let answer = rows(&query);
        assert!(answer.len() > 7, "{answer:?}");
        assert_eq!(answer, rows(&per_set));
        // the purchases' grouping is a WITH query, and the lines are grouped under a read of it
        let plan = explain(&query);
        assert!(plan.contains("CTE side_a"), "{plan}");
        assert!(
            grouped_with(
                &plan,
                " on lego_inventory_parts",
                " on lego_purchases",
                "CTE Scan on side_a"
            ),
            "{plan}"
        );
    }
}
